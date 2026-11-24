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

/// Inline sample capacity per PCM frame passed across the lock-free queue.
/// Covers ATVV v1.0 16 kHz frames (240 samples) and v0.4 frames (257 samples)
/// without any heap allocation or deallocation inside the PipeWire RT callback.
pub const MAX_PCM_FRAME_SAMPLES: usize = 512;

#[derive(Debug)]
pub struct PcmFrame {
    pub generation: u64,
    len: u16,
    samples: [i16; MAX_PCM_FRAME_SAMPLES],
}

impl PcmFrame {
    pub fn from_slice(generation: u64, slice: &[i16]) -> Self {
        let len = slice.len().min(MAX_PCM_FRAME_SAMPLES);
        let mut samples = [0i16; MAX_PCM_FRAME_SAMPLES];
        samples[..len].copy_from_slice(&slice[..len]);
        Self {
            generation,
            len: len as u16,
            samples,
        }
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples[..self.len as usize]
    }

    pub fn samples_mut(&mut self) -> &mut [i16] {
        &mut self.samples[..self.len as usize]
    }
}

/// Return true when a frame belongs to a newer utterance.
fn frame_is_newer(frame_generation: u64, previous_generation: u64) -> bool {
    crate::audio::generation_number(frame_generation)
        > crate::audio::generation_number(previous_generation)
}

fn sync_generation(pending: &mut Vec<i16>, previous: &mut u64, current: u64) {
    if frame_is_newer(current, *previous) {
        pending.clear();
        *previous = current;
    }
}

fn frame_is_output_current(frame_generation: u64, previous_generation: u64, current: u64) -> bool {
    frame_generation == previous_generation
        && previous_generation == current
        && !crate::audio::generation_is_blocked(current)
}

fn sync_output_buffer(pending: &mut Vec<i16>, previous: &mut u64, current: u64) {
    sync_generation(pending, previous, current);
    if crate::audio::generation_is_blocked(current) {
        pending.clear();
    }
}

fn sync_consumer_generation(generation: &AtomicU64, previous: u32, current: u32) {
    match (previous == 0, current == 0) {
        (false, true) => crate::audio::advance_blocked_generation(generation),
        (true, false) => {
            let current = generation.load(Ordering::Acquire);
            if crate::audio::generation_is_blocked(current) {
                advance_unblocked_generation_if_current(generation, current);
            }
        }
        _ => {}
    }
}

fn is_active_consumer(passive: Option<bool>, monitor: Option<&bool>) -> bool {
    passive == Some(false) && monitor == Some(&false)
}

fn pipewire_bool(value: Option<&str>) -> bool {
    matches!(value, Some("true" | "1"))
}

fn update_passive(state: &mut Option<bool>, props_changed: bool, value: Option<&str>) -> bool {
    if !props_changed {
        return false;
    }
    let passive = pipewire_bool(value);
    if *state == Some(passive) {
        return false;
    }
    *state = Some(passive);
    true
}

fn is_audio_consumer(media_class: Option<&str>) -> bool {
    matches!(
        media_class,
        Some("Stream/Input/Audio" | "Audio/Sink" | "Audio/Duplex")
    )
}

fn advance_generation_if_current(generation: &AtomicU64, current: u64) {
    let next = crate::audio::generation_number(current).wrapping_add(1)
        | (current & crate::audio::GENERATION_BLOCKED);
    let _ = generation.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire);
}

fn advance_unblocked_generation_if_current(generation: &AtomicU64, current: u64) {
    let next = crate::audio::generation_number(current).wrapping_add(1);
    let _ = generation.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire);
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

/// Maximum pending samples before overflow truncation (~500ms at 16kHz).
const MAX_PENDING: usize = 8000;

/// Append decoded PCM samples into a pre-allocated buffer without ever growing
/// past `max_pending`, returning the number of dropped samples.
fn push_samples_bounded(buf: &mut Vec<i16>, samples: &[i16], max_pending: usize) -> usize {
    if samples.len() >= max_pending {
        let dropped = buf.len() + (samples.len() - max_pending);
        buf.clear();
        buf.extend_from_slice(&samples[samples.len() - max_pending..]);
        return dropped;
    }
    let overflow = (buf.len() + samples.len()).saturating_sub(max_pending);
    if overflow > 0 {
        buf.drain(..overflow);
    }
    buf.extend_from_slice(samples);
    overflow
}

fn log_dropped_samples(counter: &AtomicU64) {
    let dropped = counter.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        tracing::warn!("audio buffer overflow: dropped {dropped} samples");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingLink {
    output_node: u32,
    input_node: u32,
    passive: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PromotedLink {
    link_id: u32,
    other_node: u32,
    passive: Option<bool>,
}

#[derive(Debug, Default)]
struct PendingLinks {
    entries: std::collections::HashMap<u32, PendingLink>,
}

impl PendingLinks {
    fn insert(&mut self, link_id: u32, output_node: u32, input_node: u32) {
        self.entries.insert(
            link_id,
            PendingLink {
                output_node,
                input_node,
                passive: None,
            },
        );
    }

    fn update_passive(&mut self, link_id: u32, props_changed: bool, value: Option<&str>) -> bool {
        self.entries
            .get_mut(&link_id)
            .is_some_and(|link| update_passive(&mut link.passive, props_changed, value))
    }

    fn remove(&mut self, link_id: u32) -> bool {
        self.entries.remove(&link_id).is_some()
    }

    fn drain_for_node(&mut self, our_id: u32) -> (Vec<PromotedLink>, Vec<u32>) {
        let mut matched = Vec::new();
        let mut unrelated = Vec::new();
        for (link_id, link) in self.entries.drain() {
            if link.output_node == our_id {
                matched.push(PromotedLink {
                    link_id,
                    other_node: link.input_node,
                    passive: link.passive,
                });
            } else if link.input_node == our_id {
                matched.push(PromotedLink {
                    link_id,
                    other_node: link.output_node,
                    passive: link.passive,
                });
            } else {
                unrelated.push(link_id);
            }
        }
        (matched, unrelated)
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
    let dropped_samples = Arc::new(AtomicU64::new(0));

    // Track our PipeWire node ID once the stream reaches Paused/Streaming.
    let our_node_id: std::rc::Rc<std::cell::Cell<u32>> =
        std::rc::Rc::new(std::cell::Cell::new(u32::MAX));
    #[allow(clippy::type_complexity)]
    let on_node_id_ready: std::rc::Rc<std::cell::RefCell<Option<Box<dyn FnMut(u32)>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));

    let consumer_generation = generation.clone();
    let mut startup_tx = startup_tx;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let our_node_id = our_node_id.clone();
            let on_node_id_ready = on_node_id_ready.clone();
            let stream_for_id = stream.clone();
            let mainloop = mainloop.clone();
            let dropped_samples = dropped_samples.clone();
            move |_, _, old, new| {
                tracing::debug!("PipeWire stream state: {old:?} -> {new:?}");
                log_dropped_samples(&dropped_samples);
                if matches!(&new, StreamState::Paused | StreamState::Streaming) {
                    let id = stream_for_id.node_id();
                    if id != u32::MAX && our_node_id.get() == u32::MAX {
                        our_node_id.set(id);
                        tracing::debug!("PipeWire node ID: {id}");
                        if let Some(mut callback) = on_node_id_ready.borrow_mut().take() {
                            callback(id);
                        }
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
            // The RT callback exclusively owns its pre-allocated PCM buffer.
            // Other threads only publish the current utterance through an atomic
            // generation counter; overflow drops are recorded atomically to avoid
            // allocations or synchronous logging inside the real-time thread.
            let mut buf = Vec::with_capacity(MAX_PENDING);
            let mut previous_generation = 0;
            let dropped_samples = dropped_samples.clone();
            move |stream: &Stream, _| {
                expire_tail_generation(&tail_expiration, &generation);
                sync_output_buffer(
                    &mut buf,
                    &mut previous_generation,
                    generation.load(Ordering::Acquire),
                );

                while let Some(mut frame) = audio_rx.pop() {
                    sync_generation(&mut buf, &mut previous_generation, frame.generation);
                    let current = generation.load(Ordering::Acquire);
                    if frame_is_output_current(frame.generation, previous_generation, current) {
                        crate::adpcm::apply_gain_linear(frame.samples_mut(), gain_linear);
                        let dropped = push_samples_bounded(&mut buf, frame.samples(), MAX_PENDING);
                        if dropped > 0 {
                            dropped_samples.fetch_add(dropped as u64, Ordering::Relaxed);
                        }
                    }
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

                sync_output_buffer(
                    &mut buf,
                    &mut previous_generation,
                    generation.load(Ordering::Acquire),
                );
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

        // Links to our node: link_id → (other_node_id, passive after Link info).
        let our_links: std::rc::Rc<
            std::cell::RefCell<std::collections::HashMap<u32, (u32, Option<bool>)>>,
        > = std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));
        // Whether each potential consumer node is a monitor (from bound Node info).
        // true = monitor (don't count), false = real consumer.
        // Nodes not yet in the map are ignored until their first info arrives.
        let node_is_monitor: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, bool>>> =
            std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));
        // Last emitted consumer count (to avoid duplicate events).
        let last_count: std::rc::Rc<std::cell::Cell<u32>> =
            std::rc::Rc::new(std::cell::Cell::new(0));
        // Bound proxies (must stay alive): link proxies + node proxies with listeners.
        let proxies: std::rc::Rc<
            std::cell::RefCell<std::collections::HashMap<u32, Box<dyn std::any::Any>>>,
        > = std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));
        // Links discovered before Stream::state_changed reports our_node_id.
        let pending_links: std::rc::Rc<std::cell::RefCell<PendingLinks>> =
            std::rc::Rc::new(std::cell::RefCell::new(PendingLinks::default()));
        let pending_proxies: std::rc::Rc<
            std::cell::RefCell<std::collections::HashMap<u32, Box<dyn std::any::Any>>>,
        > = std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));

        // Count only classified, non-passive links to real consumers.
        let recount = {
            let our_links = our_links.clone();
            let node_is_monitor = node_is_monitor.clone();
            let last_count = last_count.clone();
            let consumer_tx = consumer_tx.clone();
            move || {
                let links = our_links.borrow();
                let monitors = node_is_monitor.borrow();
                let count = links
                    .values()
                    .filter(|(other_node, passive)| {
                        is_active_consumer(*passive, monitors.get(other_node))
                    })
                    .count() as u32;
                let previous = last_count.get();
                if count != previous {
                    sync_consumer_generation(&consumer_generation, previous, count);
                    last_count.set(count);
                    tracing::debug!("PipeWire consumer count: {count}");
                    if let Err(e) = consumer_tx.send(crate::consumer::ConsumerEvent::Changed(count))
                    {
                        tracing::warn!("Consumer event dropped: {e}");
                    }
                }
            }
        };

        *on_node_id_ready.borrow_mut() = Some(Box::new({
            let pending_links = pending_links.clone();
            let pending_proxies = pending_proxies.clone();
            let our_links = our_links.clone();
            let proxies = proxies.clone();
            let recount = recount.clone();
            move |our_id| {
                let (matched, unrelated) = pending_links.borrow_mut().drain_for_node(our_id);
                {
                    let mut pend_proxies = pending_proxies.borrow_mut();
                    for id in unrelated {
                        pend_proxies.remove(&id);
                    }
                    let mut links = our_links.borrow_mut();
                    let mut active_proxies = proxies.borrow_mut();
                    for promoted in &matched {
                        tracing::debug!(
                            "Promoting buffered PipeWire link to our node (id={}, other_node={})",
                            promoted.link_id,
                            promoted.other_node
                        );
                        links.insert(promoted.link_id, (promoted.other_node, promoted.passive));
                        if let Some(proxy) = pend_proxies.remove(&promoted.link_id) {
                            active_proxies.insert(promoted.link_id, proxy);
                        }
                    }
                }
                if !matched.is_empty() {
                    recount();
                }
            }
        }));

        let listener = registry
            .add_listener_local()
            .global({
                let our_node_id = our_node_id.clone();
                let our_links = our_links.clone();
                let pending_links = pending_links.clone();
                let node_is_monitor = node_is_monitor.clone();
                let proxies = proxies.clone();
                let pending_proxies = pending_proxies.clone();
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
                                        let is_monitor = pipewire_bool(
                                            info.props().and_then(|p| p.get("stream.monitor")),
                                        );
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
                                .insert(node_id, Box::new((node_listener, node)));
                        }
                        ObjectType::Link => {
                            let Some(props) = &global.props else {
                                return;
                            };
                            let Some(output_node) = props
                                .get("link.output.node")
                                .and_then(|v| v.parse::<u32>().ok())
                            else {
                                return;
                            };
                            let Some(input_node) = props
                                .get("link.input.node")
                                .and_then(|v| v.parse::<u32>().ok())
                            else {
                                return;
                            };

                            let our_id = our_node_id.get();
                            let link_id = global.id;
                            if our_id == u32::MAX {
                                // Buffer until state_changed reports our node ID so early
                                // WirePlumber auto-links are never lost.
                                let Ok(link) = registry.bind::<pipewire::link::Link, _>(global)
                                else {
                                    return;
                                };
                                pending_links
                                    .borrow_mut()
                                    .insert(link_id, output_node, input_node);
                                let link_listener = link
                                    .add_listener_local()
                                    .info({
                                        let our_links = our_links.clone();
                                        let pending_links = pending_links.clone();
                                        let recount = recount.clone();
                                        move |info| {
                                            let props_changed = info
                                                .change_mask()
                                                .contains(pipewire::link::LinkChangeMask::PROPS);
                                            let passive =
                                                info.props().and_then(|p| p.get("link.passive"));
                                            let changed_active = {
                                                let mut links = our_links.borrow_mut();
                                                links.get_mut(&link_id).is_some_and(|(_, state)| {
                                                    update_passive(state, props_changed, passive)
                                                })
                                            };
                                            if changed_active {
                                                recount();
                                            } else {
                                                pending_links.borrow_mut().update_passive(
                                                    link_id,
                                                    props_changed,
                                                    passive,
                                                );
                                            }
                                        }
                                    })
                                    .register();
                                pending_proxies
                                    .borrow_mut()
                                    .insert(link_id, Box::new((link_listener, link)));
                                return;
                            }

                            if output_node != our_id && input_node != our_id {
                                return;
                            }
                            let other_id = if output_node == our_id {
                                input_node
                            } else {
                                output_node
                            };

                            tracing::debug!(
                                "PipeWire link to our node (id={link_id}, other_node={other_id})"
                            );
                            our_links.borrow_mut().insert(link_id, (other_id, None));
                            let Ok(link) = registry.bind::<pipewire::link::Link, _>(global) else {
                                our_links.borrow_mut().remove(&link_id);
                                return;
                            };
                            let link_listener = link
                                .add_listener_local()
                                .info({
                                    let our_links = our_links.clone();
                                    let recount = recount.clone();
                                    move |info| {
                                        let changed = {
                                            let mut links = our_links.borrow_mut();
                                            links.get_mut(&link_id).is_some_and(|(_, state)| {
                                                update_passive(
                                                    state,
                                                    info.change_mask().contains(
                                                        pipewire::link::LinkChangeMask::PROPS,
                                                    ),
                                                    info.props()
                                                        .and_then(|p| p.get("link.passive")),
                                                )
                                            })
                                        };
                                        if changed {
                                            recount();
                                        }
                                    }
                                })
                                .register();
                            proxies
                                .borrow_mut()
                                .insert(link_id, Box::new((link_listener, link)));
                            recount();
                        }
                        _ => {}
                    }
                }
            })
            .global_remove({
                let our_links = our_links.clone();
                let pending_links = pending_links.clone();
                let node_is_monitor = node_is_monitor.clone();
                let proxies = proxies.clone();
                let pending_proxies = pending_proxies.clone();
                let recount = recount.clone();
                move |id| {
                    proxies.borrow_mut().remove(&id);
                    pending_proxies.borrow_mut().remove(&id);
                    pending_links.borrow_mut().remove(id);
                    let had_link = our_links.borrow_mut().remove(&id).is_some();
                    let had_node = node_is_monitor.borrow_mut().remove(&id).is_some();
                    if had_link || had_node {
                        recount();
                    }
                }
            })
            .register();

        (
            Some(registry),
            Some(listener),
            Some((proxies, pending_proxies)),
        )
    } else {
        (None, None, None)
    };

    // Block until quit.
    mainloop.run();
    log_dropped_samples(&dropped_samples);

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
        assert!(
            !std::mem::needs_drop::<PcmFrame>(),
            "PcmFrame must be trivially droppable to avoid heap deallocation in PipeWire RT callback"
        );
        let queue = ArrayQueue::new(2);
        for generation in 1..=3 {
            let _ = queue.force_push(PcmFrame::from_slice(generation, &[generation as i16]));
        }
        let first = queue.pop().unwrap();
        assert_eq!(first.generation, 2);
        assert_eq!(first.samples(), &[2]);
        let second = queue.pop().unwrap();
        assert_eq!(second.generation, 3);
        assert_eq!(second.samples(), &[3]);
    }

    #[test]
    fn only_current_session_frames_are_accepted() {
        assert!(frame_is_newer(7, 6));
        assert!(!frame_is_newer(6, 7));
    }

    #[test]
    fn disconnected_generation_blocks_only_its_backlog() {
        assert!(frame_is_output_current(1, 1, 1));
        assert!(!frame_is_output_current(
            1,
            1,
            crate::audio::GENERATION_BLOCKED | 1
        ));
        assert!(!frame_is_output_current(
            2,
            2,
            crate::audio::GENERATION_BLOCKED | 1
        ));
        assert!(frame_is_output_current(2, 2, 2));
        assert!(!frame_is_output_current(1, 2, 2));
    }

    #[test]
    fn output_buffer_is_revalidated_before_writing() {
        let mut pending = vec![10, 11];
        let mut generation = 1;
        sync_output_buffer(
            &mut pending,
            &mut generation,
            crate::audio::GENERATION_BLOCKED | 1,
        );
        assert!(pending.is_empty());
        sync_output_buffer(&mut pending, &mut generation, 2);
        pending.extend_from_slice(&[20, 21]);
        sync_output_buffer(&mut pending, &mut generation, 2);
        assert_eq!(pending, vec![20, 21]);
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
    fn link_state_updates_preserve_passive_classification() {
        let mut state = None;
        assert!(update_passive(&mut state, true, Some("true")));
        assert_eq!(state, Some(true));
        assert!(!update_passive(&mut state, false, None));
        assert_eq!(state, Some(true));
        assert!(!update_passive(&mut state, true, Some("1")));
        assert_eq!(state, Some(true));
        assert!(update_passive(&mut state, true, None));
        assert_eq!(state, Some(false));
    }

    #[test]
    fn unclassified_or_passive_links_are_not_consumers() {
        assert!(!is_active_consumer(None, Some(&false)));
        assert!(!is_active_consumer(Some(false), None));
        assert!(!is_active_consumer(Some(true), Some(&false)));
        assert!(!is_active_consumer(Some(false), Some(&true)));
        assert!(is_active_consumer(Some(false), Some(&false)));
    }

    #[test]
    fn consumer_presence_transitions_invalidate_audio() {
        let generation = AtomicU64::new(0);
        sync_consumer_generation(&generation, 0, 1);
        assert_eq!(generation.load(Ordering::Relaxed), 0);
        sync_consumer_generation(&generation, 1, 0);
        assert_eq!(
            generation.load(Ordering::Relaxed),
            crate::audio::GENERATION_BLOCKED | 1
        );
        sync_consumer_generation(&generation, 0, 1);
        assert_eq!(generation.load(Ordering::Relaxed), 2);
        sync_consumer_generation(&generation, 1, 0);
        assert_eq!(
            generation.load(Ordering::Relaxed),
            crate::audio::GENERATION_BLOCKED | 3
        );
        crate::audio::advance_unblocked_generation(&generation);
        sync_consumer_generation(&generation, 0, 1);
        assert_eq!(generation.load(Ordering::Relaxed), 4);
        sync_consumer_generation(&generation, 1, 2);
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

    #[test]
    fn pending_links_promote_matches_and_drop_unrelated() {
        let mut pending = PendingLinks::default();
        pending.insert(10, 42, 100);
        pending.insert(11, 200, 42);
        pending.insert(12, 300, 400);
        pending.insert(13, 42, 500);
        assert!(pending.update_passive(10, true, None));
        assert!(pending.update_passive(11, true, Some("true")));
        assert!(pending.remove(13));

        let (mut matched, unrelated) = pending.drain_for_node(42);
        matched.sort_by_key(|link| link.link_id);
        assert_eq!(
            matched,
            vec![
                PromotedLink {
                    link_id: 10,
                    other_node: 100,
                    passive: Some(false),
                },
                PromotedLink {
                    link_id: 11,
                    other_node: 200,
                    passive: Some(true),
                },
            ]
        );
        assert_eq!(unrelated, vec![12]);
        assert!(pending.entries.is_empty());
    }

    #[test]
    fn push_samples_bounded_never_reallocates_and_counts_drops() {
        let mut buf = Vec::with_capacity(4);
        let initial_cap = buf.capacity();
        assert_eq!(push_samples_bounded(&mut buf, &[1, 2, 3], 4), 0);
        assert_eq!(buf, vec![1, 2, 3]);
        assert_eq!(push_samples_bounded(&mut buf, &[4, 5, 6], 4), 2);
        assert_eq!(buf, vec![3, 4, 5, 6]);
        assert_eq!(push_samples_bounded(&mut buf, &[10, 11, 12, 13, 14], 4), 5);
        assert_eq!(buf, vec![11, 12, 13, 14]);
        assert_eq!(buf.capacity(), initial_cap);
    }
}
