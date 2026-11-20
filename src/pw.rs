//! PipeWire virtual audio source.
//!
//! Runs on a dedicated OS thread (not tokio-compatible). Receives decoded PCM
//! frames via [`crossbeam_queue::ArrayQueue`] and pushes them to PipeWire as a virtual
//! microphone source.
//!
//! Uses `MainLoopRc` so the shutdown handler can clone the mainloop reference,
//! and normal Rust drop order handles teardown after `mainloop.run()` returns.

use crossbeam_queue::ArrayQueue;
use std::io;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use pipewire::context::ContextRc;
use pipewire::keys;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::properties;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Pod, Value};
use pipewire::spa::utils::{Direction, SpaTypes};
use pipewire::stream::{Stream, StreamFlags, StreamRc, StreamState};
use pipewire::types::ObjectType;

/// Number of audio channels (mono).
const CHANNELS: u32 = 1;

/// Size of one sample in bytes (i16 = 2 bytes).
const SAMPLE_SIZE: usize = std::mem::size_of::<i16>();

/// Control messages for the PipeWire source.
#[derive(Debug)]
pub enum SessionControl {
    Shutdown,
}

#[derive(Debug)]
pub struct PcmFrame {
    pub generation: u64,
    pub samples: Vec<i16>,
}

/// Return true when a frame belongs to a newer utterance.
fn frame_is_newer(frame_generation: u64, previous_generation: u64) -> bool {
    frame_generation > previous_generation
}

fn sync_generation(pending: &mut Vec<i16>, previous: &mut u64, current: u64) {
    if frame_is_newer(current, *previous) {
        pending.clear();
        *previous = current;
    }
}

fn sync_consumer_generation(
    generation: &AtomicU64,
    disconnected: &std::cell::Cell<Option<u64>>,
    previous: u32,
    current: u32,
) {
    match (previous == 0, current == 0) {
        (false, true) => {
            disconnected.set(Some(generation.fetch_add(1, Ordering::Relaxed) + 1));
        }
        (true, false) => {
            if let Some(expired) = disconnected.take() {
                advance_generation_if_current(generation, expired);
            }
        }
        _ => {}
    }
}

fn is_real_consumer(monitor: Option<&bool>) -> bool {
    monitor == Some(&false)
}

fn is_audio_consumer(media_class: Option<&str>) -> bool {
    matches!(
        media_class,
        Some("Stream/Input/Audio" | "Audio/Sink" | "Audio/Duplex")
    )
}

fn advance_generation_if_current(generation: &AtomicU64, current: u64) {
    let _ = generation.compare_exchange(current, current + 1, Ordering::Relaxed, Ordering::Relaxed);
}

fn expire_tail_generation(tail: &crate::audio::TailDeadline, generation: &AtomicU64) {
    if let Some(expired) = tail.expire_if_due() {
        advance_generation_if_current(generation, expired);
    }
}

fn stream_failure(old: &StreamState, new: &StreamState, shutting_down: bool) -> Option<String> {
    if shutting_down {
        return None;
    }
    match new {
        StreamState::Error(error) => Some(format!("PipeWire stream error: {error}")),
        StreamState::Unconnected if !matches!(old, StreamState::Unconnected) => {
            Some("PipeWire stream disconnected unexpectedly".to_string())
        }
        _ => None,
    }
}

/// Run the PipeWire audio source on the current thread (blocking).
///
/// Reads decoded PCM frames from `audio_rx` and pushes them to PipeWire as a
/// virtual microphone. Returns when `shutdown_rx` receives a [`SessionControl::Shutdown`] message.
///
/// Call from a dedicated `std::thread::spawn`.
#[allow(clippy::too_many_arguments)]
pub fn run_pw_source(
    audio_rx: Arc<ArrayQueue<PcmFrame>>,
    generation: Arc<AtomicU64>,
    tail_expiration: crate::audio::TailDeadline,
    gain_db: f32,
    sample_rate: u32,
    node_name: &str,
    node_description: &str,
    shutdown_rx: pipewire::channel::Receiver<SessionControl>,
    startup_tx: Option<std::sync::mpsc::Sender<Result<(), String>>>,
    failure_tx: tokio::sync::watch::Sender<Option<String>>,
    consumer_tx: Option<tokio::sync::watch::Sender<crate::consumer::ConsumerEvent>>,
) -> Result<(), pipewire::Error> {
    pipewire::init();

    let mainloop = MainLoopRc::new(None)?;

    // Shared slot for the stream - set after creation, used by shutdown handler.
    let stream_slot: std::rc::Rc<std::cell::RefCell<Option<StreamRc>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let shutting_down = std::rc::Rc::new(std::cell::Cell::new(false));
    let _receiver = shutdown_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let stream_slot = stream_slot.clone();
        let shutting_down = shutting_down.clone();
        move |_: SessionControl| {
            shutting_down.set(true);
            tracing::info!("PipeWire source shutting down");
            if let Some(stream) = stream_slot.borrow().as_ref() {
                let _ = stream.disconnect();
            }
            mainloop.quit();
        }
    });

    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let core_clone = core.clone(); // Keep a clone for registry creation later

    let stream = StreamRc::new(
        core,
        node_name,
        properties! {
            *keys::MEDIA_TYPE => "Audio",
            *keys::MEDIA_CATEGORY => "Capture",
            *keys::MEDIA_CLASS => "Audio/Source",
            *keys::MEDIA_ROLE => "Communication",
            *keys::NODE_NAME => node_name,
            *keys::NODE_DESCRIPTION => node_description,
        },
    )?;

    // Give the shutdown handler a reference to the stream.
    *stream_slot.borrow_mut() = Some(stream.clone());

    // Precompute linear gain multiplier outside the RT callback to avoid
    // recomputing powf() on every frame.
    let gain_linear = 10f32.powf(gain_db / 20.0);

    /// Maximum pending samples before overflow truncation (~500ms at 16kHz).
    const MAX_PENDING: usize = 8000;

    // Track our PipeWire node ID once the stream reaches Paused/Streaming.
    let our_node_id: std::rc::Rc<std::cell::Cell<u32>> =
        std::rc::Rc::new(std::cell::Cell::new(u32::MAX));

    let consumer_generation = generation.clone();
    let mut startup_tx = startup_tx;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let our_node_id = our_node_id.clone();
            let stream_for_id = stream.clone();
            let mainloop = mainloop.clone();
            move |_, _, old, new| {
                tracing::debug!("PipeWire stream state: {old:?} -> {new:?}");
                if matches!(&new, StreamState::Paused | StreamState::Streaming) {
                    let id = stream_for_id.node_id();
                    if id != u32::MAX {
                        our_node_id.set(id);
                        tracing::debug!("PipeWire node ID: {id}");
                    }
                    if let Some(startup_tx) = startup_tx.take() {
                        let _ = startup_tx.send(Ok(()));
                        tracing::info!(
                            "PipeWire source running ({}kHz S16LE mono, gain={gain_db}dB)",
                            sample_rate / 1000
                        );
                    }
                }
                if let Some(failure) = stream_failure(&old, &new, shutting_down.get()) {
                    if let Some(startup_tx) = startup_tx.take() {
                        let _ = startup_tx.send(Err(failure.clone()));
                    }
                    let _ = failure_tx.send(Some(failure));
                    mainloop.quit();
                }
            }
        })
        .process({
            // The RT callback exclusively owns its PCM buffer. Other threads only
            // publish the current utterance through an atomic generation counter.
            let mut buf = Vec::new();
            let mut previous_generation = 0;
            move |stream: &Stream, _| {
                expire_tail_generation(&tail_expiration, &generation);
                sync_generation(
                    &mut buf,
                    &mut previous_generation,
                    generation.load(Ordering::Relaxed),
                );

                while let Some(mut frame) = audio_rx.pop() {
                    sync_generation(&mut buf, &mut previous_generation, frame.generation);
                    if frame.generation == previous_generation {
                        crate::adpcm::apply_gain_linear(&mut frame.samples, gain_linear);
                        buf.extend_from_slice(&frame.samples);
                    }
                }

                // Bound pending buffer to prevent unbounded growth under backpressure.
                if buf.len() > MAX_PENDING {
                    let dropped = buf.len() - MAX_PENDING;
                    buf.drain(..dropped);
                    tracing::warn!("audio buffer overflow: dropped {dropped} samples");
                }

                let Some(mut pw_buf) = stream.dequeue_buffer() else {
                    return;
                };

                let requested = pw_buf.requested() as usize;

                let Some(pw_data) = pw_buf.datas_mut().first_mut() else {
                    return;
                };

                let Some(slice) = pw_data.data() else {
                    return;
                };

                let buf_capacity = slice.len() / SAMPLE_SIZE;
                let max_samples = if requested > 0 {
                    buf_capacity.min(requested)
                } else {
                    buf_capacity
                };

                let available = buf.len().min(max_samples);

                // Copy i16 samples to the output buffer as little-endian bytes.
                for (src, dst) in buf
                    .iter()
                    .take(available)
                    .zip(slice.as_chunks_mut::<SAMPLE_SIZE>().0)
                {
                    dst.copy_from_slice(&src.to_le_bytes());
                }

                // Zero remaining buffer bytes (silence).
                let silence_start = available * SAMPLE_SIZE;
                let silence_end = max_samples * SAMPLE_SIZE;
                slice[silence_start..silence_end].fill(0);

                if available > 0 {
                    buf.drain(..available);
                }

                let chunk = pw_data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = SAMPLE_SIZE as i32;
                *chunk.size_mut() = (max_samples * SAMPLE_SIZE) as u32;
            }
        })
        .register()?;

    // Build the SPA audio format pod for format negotiation.
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::S16LE);
    audio_info.set_rate(sample_rate);
    audio_info.set_channels(CHANNELS);

    let mut position = [0u32; 64];
    position[0] = pipewire::spa::sys::SPA_AUDIO_CHANNEL_MONO;
    audio_info.set_position(position);

    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> =
        PodSerializer::serialize(io::Cursor::new(Vec::new()), &Value::Object(obj))
            .expect("failed to serialize audio format pod")
            .0
            .into_inner();

    let mut params = [Pod::from_bytes(&values).expect("invalid pod bytes")];

    stream.connect(
        Direction::Output,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    // Registry link monitoring for --mic-on-demand.
    //
    // Tracks PipeWire Link objects that connect to our stream node. Excludes
    // passive links (pavucontrol peak meters, level meters) so they don't
    // trigger mic open or hold it open.
    //
    // Detection uses bound Link proxies: the registry global props don't include
    // `link.passive`, but the Link proxy's `info` event does. When a link to our
    // node appears, we bind its proxy and wait for the info callback to determine
    // if it's passive. This handles the race where link globals arrive before
    // their full properties are available.
    //
    // Detection: for each link to our node, we bind the OTHER node's proxy and
    // check its full info properties for `node.want-driver == "true"`. The
    // registry global only shows a subset of node properties; the bound Node
    // proxy's `info` event includes everything — specifically `stream.monitor`
    // which is set on peak detect streams (pavucontrol) but not real consumers.
    #[allow(clippy::type_complexity)]
    let (_registry, _registry_listener, _proxies) = if let Some(consumer_tx) = consumer_tx {
        let registry = core_clone.get_registry_rc()?;

        // Links to our node: link_id → other_node_id.
        let our_links: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, u32>>> =
            std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));
        // Whether each Stream/Input/Audio node is a monitor (from bound Node info).
        // true = monitor (don't count), false = real consumer.
        // Nodes not yet in the map are ignored until their first info arrives.
        let node_is_monitor: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, bool>>> =
            std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));
        // Last emitted consumer count (to avoid duplicate events).
        let last_count: std::rc::Rc<std::cell::Cell<u32>> =
            std::rc::Rc::new(std::cell::Cell::new(0));
        let disconnected_generation = std::rc::Rc::new(std::cell::Cell::new(None));
        // Bound proxies (must stay alive): link proxies + node proxies with listeners.
        let proxies: std::rc::Rc<
            std::cell::RefCell<std::collections::HashMap<u32, Box<dyn std::any::Any>>>,
        > = std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));

        // Count only links whose other node is classified as a real consumer.
        let recount = {
            let our_links = our_links.clone();
            let node_is_monitor = node_is_monitor.clone();
            let last_count = last_count.clone();
            let disconnected_generation = disconnected_generation.clone();
            let consumer_tx = consumer_tx.clone();
            move || {
                let links = our_links.borrow();
                let monitors = node_is_monitor.borrow();
                let count = links
                    .values()
                    .filter(|other_node| is_real_consumer(monitors.get(other_node)))
                    .count() as u32;
                let previous = last_count.get();
                if count != previous {
                    sync_consumer_generation(
                        &consumer_generation,
                        &disconnected_generation,
                        previous,
                        count,
                    );
                    last_count.set(count);
                    tracing::debug!("PipeWire consumer count: {count}");
                    if let Err(e) = consumer_tx.send(crate::consumer::ConsumerEvent::Changed(count))
                    {
                        tracing::warn!("Consumer event dropped: {e}");
                    }
                }
            }
        };

        let listener = registry
            .add_listener_local()
            .global({
                let our_node_id = our_node_id.clone();
                let our_links = our_links.clone();
                let node_is_monitor = node_is_monitor.clone();
                let proxies = proxies.clone();
                let registry = registry.clone();
                let recount = recount.clone();
                move |global| {
                    match global.type_ {
                        ObjectType::Node => {
                            // Bind potential consumers to get their full info.
                            let media_class =
                                global.props.as_ref().and_then(|p| p.get("media.class"));
                            if !is_audio_consumer(media_class) {
                                return;
                            }
                            let node_id = global.id;
                            let Ok(node) = registry.bind::<pipewire::node::Node, _>(global) else {
                                return;
                            };
                            let node_listener = node
                                .add_listener_local()
                                .info({
                                    let node_is_monitor = node_is_monitor.clone();
                                    let recount = recount.clone();
                                    move |info| {
                                        let is_monitor =
                                            info.props().and_then(|p| p.get("stream.monitor"))
                                                == Some("true");
                                        let mut map = node_is_monitor.borrow_mut();
                                        let was_monitor =
                                            map.get(&node_id).copied().unwrap_or(false);
                                        if is_monitor && !was_monitor {
                                            // Sticky: once classified as monitor, stays monitor.
                                            // stream.monitor flickers during PW reconfiguration
                                            // but a peak detect stream never becomes a real consumer.
                                            tracing::debug!(
                                                "PipeWire node (id={node_id}) classified as monitor"
                                            );
                                            map.insert(node_id, true);
                                            drop(map);
                                            recount();
                                        } else if let std::collections::hash_map::Entry::Vacant(e) =
                                            map.entry(node_id)
                                        {
                                            // First info: not a monitor stream.
                                            e.insert(false);
                                            drop(map);
                                            recount();
                                        }
                                    }
                                })
                                .register();
                            proxies
                                .borrow_mut()
                                .insert(node_id, Box::new((node, node_listener)));
                        }
                        ObjectType::Link => {
                            let our_id = our_node_id.get();
                            if our_id == u32::MAX {
                                return;
                            }
                            let Some(props) = &global.props else {
                                return;
                            };
                            let output_node = props
                                .get("link.output.node")
                                .and_then(|v| v.parse::<u32>().ok());
                            let input_node = props
                                .get("link.input.node")
                                .and_then(|v| v.parse::<u32>().ok());
                            if output_node != Some(our_id) && input_node != Some(our_id) {
                                return;
                            }
                            let other_node = if output_node == Some(our_id) {
                                input_node
                            } else {
                                output_node
                            };
                            let Some(other_id) = other_node else { return };

                            let link_id = global.id;
                            tracing::debug!(
                                "PipeWire link to our node (id={link_id}, other_node={other_id})"
                            );
                            our_links.borrow_mut().insert(link_id, other_id);
                            // Recount — if node info is already cached, this will
                            // immediately include or exclude the link. If not yet
                            // cached, the node's info callback will trigger recount.
                            recount();
                        }
                        _ => {}
                    }
                }
            })
            .global_remove({
                let our_links = our_links.clone();
                let node_is_monitor = node_is_monitor.clone();
                let proxies = proxies.clone();
                let recount = recount.clone();
                move |id| {
                    proxies.borrow_mut().remove(&id);
                    let had_link = our_links.borrow_mut().remove(&id).is_some();
                    let had_node = node_is_monitor.borrow_mut().remove(&id).is_some();
                    if had_link || had_node {
                        recount();
                    }
                }
            })
            .register();

        (Some(registry), Some(listener), Some(proxies))
    } else {
        (None, None, None)
    };

    // Block until quit.
    mainloop.run();

    // Follow cpal's exact drop pattern: explicitly drop listener and context
    // first, let stream, _receiver, and mainloop drop at function scope end.
    // Do NOT call pipewire::deinit() - it's process-global and we may create
    // another PW thread on reconnect.
    drop(_listener);
    drop(context);

    tracing::info!("PipeWire source stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_queue_drops_oldest_frame_when_full() {
        let queue = ArrayQueue::new(2);
        for generation in 1..=3 {
            let _ = queue.force_push(PcmFrame {
                generation,
                samples: vec![generation as i16],
            });
        }
        assert_eq!(queue.pop().unwrap().generation, 2);
        assert_eq!(queue.pop().unwrap().generation, 3);
    }

    #[test]
    fn only_current_session_frames_are_accepted() {
        assert!(frame_is_newer(7, 6));
        assert!(!frame_is_newer(6, 7));
    }

    #[test]
    fn generation_change_clears_only_older_samples() {
        let mut pending = vec![10, 11];
        let mut generation = 1;
        sync_generation(&mut pending, &mut generation, 1);
        assert_eq!(pending, vec![10, 11]);
        sync_generation(&mut pending, &mut generation, 2);
        assert!(pending.is_empty());
        assert_eq!(generation, 2);
        pending.extend_from_slice(&[20, 21]);
        sync_generation(&mut pending, &mut generation, 2);
        assert_eq!(pending, vec![20, 21]);
        sync_generation(&mut pending, &mut generation, 1);
        assert_eq!(pending, vec![20, 21]);
    }

    #[test]
    fn recording_streams_sinks_and_duplexes_are_consumer_candidates() {
        for class in ["Stream/Input/Audio", "Audio/Sink", "Audio/Duplex"] {
            assert!(is_audio_consumer(Some(class)));
        }
        assert!(!is_audio_consumer(Some("Audio/Source")));
        assert!(!is_audio_consumer(None));
    }

    #[test]
    fn unclassified_nodes_are_not_consumers() {
        assert!(!is_real_consumer(None));
        assert!(!is_real_consumer(Some(&true)));
        assert!(is_real_consumer(Some(&false)));
    }

    #[test]
    fn consumer_presence_transitions_invalidate_audio() {
        let generation = AtomicU64::new(0);
        let disconnected = std::cell::Cell::new(None);
        sync_consumer_generation(&generation, &disconnected, 0, 1);
        assert_eq!(generation.load(Ordering::Relaxed), 0);
        sync_consumer_generation(&generation, &disconnected, 1, 0);
        assert_eq!(generation.load(Ordering::Relaxed), 1);
        sync_consumer_generation(&generation, &disconnected, 0, 1);
        assert_eq!(generation.load(Ordering::Relaxed), 2);
        sync_consumer_generation(&generation, &disconnected, 1, 0);
        assert_eq!(generation.load(Ordering::Relaxed), 3);
        generation.fetch_add(1, Ordering::Relaxed);
        sync_consumer_generation(&generation, &disconnected, 0, 1);
        assert_eq!(generation.load(Ordering::Relaxed), 4);
        sync_consumer_generation(&generation, &disconnected, 1, 2);
        assert_eq!(generation.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn stale_expiry_does_not_advance_a_new_recording() {
        let generation = AtomicU64::new(1);
        let tail = crate::audio::TailDeadline::new();
        tail.arm_after(std::time::Duration::from_millis(1), 1);
        generation.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(3));
        expire_tail_generation(&tail, &generation);
        assert_eq!(generation.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn output_expiry_invalidates_tail_without_ble() {
        let generation = AtomicU64::new(0);
        let tail = crate::audio::TailDeadline::new();
        tail.arm_after(std::time::Duration::from_millis(1), 0);
        std::thread::sleep(std::time::Duration::from_millis(3));
        expire_tail_generation(&tail, &generation);
        assert_eq!(generation.load(Ordering::Relaxed), 1);
        expire_tail_generation(&tail, &generation);
        assert_eq!(generation.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stream_errors_and_unexpected_disconnects_are_failures() {
        assert!(stream_failure(
            &StreamState::Connecting,
            &StreamState::Error("boom".to_string()),
            false
        )
        .is_some());
        assert!(stream_failure(&StreamState::Paused, &StreamState::Unconnected, false).is_some());
        assert!(stream_failure(&StreamState::Paused, &StreamState::Unconnected, true).is_none());
        assert!(stream_failure(&StreamState::Connecting, &StreamState::Paused, false).is_none());
    }
}
