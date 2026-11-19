//! RC003 integration for Omarchy and Superwhisper.

mod adpcm;
mod atvv;
mod audio;
mod ble;
mod commands;
mod config;
mod consumer;
#[cfg(feature = "dbus")]
mod dbus;
mod input;
mod protocol;
mod pw;
mod service;

use anyhow::Context;
use clap::Parser;
use crossbeam_queue::ArrayQueue;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{atomic::AtomicU64, Arc};
use std::time::Duration;

/// Capacity of the async channel between the ATVV session and the decoder task.
const DECODER_CHANNEL_CAPACITY: usize = 64;
const DEVICE_LOCK_EXCLUSION: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(
    name = "rc003-omarchy",
    about = "Xiaomi RC003 microphone and input bridge for Omarchy"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<commands::Command>,

    /// Configuration file (default: ~/.config/rc003-omarchy/config.toml).
    #[arg(global = true, long)]
    config: Option<PathBuf>,

    /// Filter by Bluetooth address (e.g., AA:BB:CC:DD:EE:FF)
    #[arg(global = true, short, long)]
    device: Option<String>,

    /// BlueZ adapter name (default: auto-detect)
    #[arg(global = true, short, long)]
    adapter: Option<String>,

    /// Audio gain in dB (default: config value or 0)
    #[arg(global = true, short, long)]
    gain: Option<f32>,

    /// Close mic after N seconds without audio frames (device asleep). 0 = disabled.
    #[arg(global = true, long, default_value = "5")]
    frame_timeout: u64,

    /// Re-send MIC_OPEN every N seconds to prevent remote's audio transfer timeout. 0 = disabled.
    #[arg(global = true, long, default_value = "10")]
    keep_alive: u64,

    /// PipeWire node name (also used for the optional D-Bus name).
    #[arg(global = true, short, long)]
    name: Option<String>,

    /// PipeWire node description (shown in audio settings).
    #[arg(global = true, long)]
    description: Option<String>,

    /// Disable D-Bus control interface
    #[cfg(feature = "dbus")]
    #[arg(global = true, long)]
    no_dbus: bool,

    /// Enable automatic mic open/close based on PipeWire consumer presence.
    /// When a PipeWire client connects to the virtual source, the mic opens
    /// automatically. When all clients disconnect, the mic closes immediately.
    #[arg(global = true, long)]
    mic_on_demand: bool,

    /// Disable RC003 evdev/uinput handling (audio only).
    #[arg(global = true, long)]
    no_input: bool,

    /// Increase log verbosity (-v, -vv, -vvv)
    #[arg(global = true, short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

struct PipeWireHandle {
    shutdown_tx: pipewire::channel::Sender<pw::SessionControl>,
    generation: Arc<AtomicU64>,
    tail_expiration: audio::TailDeadline,
    failure_rx: Option<tokio::sync::watch::Receiver<Option<String>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PipeWireHandle {
    fn new(
        pcm_rx: std::sync::Arc<ArrayQueue<pw::PcmFrame>>,
        gain: f32,
        node_name: String,
        node_description: String,
        consumer_tx: Option<tokio::sync::watch::Sender<consumer::ConsumerEvent>>,
    ) -> anyhow::Result<Self> {
        let (shutdown_tx, shutdown_rx) = pipewire::channel::channel::<pw::SessionControl>();
        let (startup_tx, startup_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let (failure_tx, failure_rx) = tokio::sync::watch::channel::<Option<String>>(None);
        let generation = Arc::new(AtomicU64::new(0));
        let thread_generation = generation.clone();
        let tail_expiration = audio::TailDeadline::new();
        let thread_tail_expiration = tail_expiration.clone();
        let join = std::thread::spawn(move || {
            if let Err(error) = pw::run_pw_source(
                pcm_rx,
                thread_generation,
                thread_tail_expiration,
                gain,
                16_000,
                &node_name,
                &node_description,
                shutdown_rx,
                Some(startup_tx.clone()),
                failure_tx.clone(),
                consumer_tx,
            ) {
                let message = error.to_string();
                let _ = startup_tx.send(Err(message.clone()));
                let _ = failure_tx.send(Some(message));
            }
        });
        match startup_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                shutdown_tx,
                generation,
                tail_expiration,
                failure_rx: Some(failure_rx),
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = shutdown_tx.send(pw::SessionControl::Shutdown);
                let _ = join.join();
                anyhow::bail!(error)
            }
            Err(error) => {
                let _ = shutdown_tx.send(pw::SessionControl::Shutdown);
                let _ = join.join();
                anyhow::bail!("PipeWire startup timed out: {error}")
            }
        }
    }
}
impl Drop for PipeWireHandle {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(pw::SessionControl::Shutdown);
        if let Some(join) = self.join.take() {
            if let Err(error) = join.join() {
                tracing::error!("PipeWire thread panicked: {error:?}");
            }
        }
    }
}

/// Sanitize a string for use as a D-Bus name component and PipeWire node suffix.
/// Lowercases, replaces non-alphanumeric chars with hyphens, collapses runs, trims.
#[must_use]
fn sanitize_name(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            result.push(c.to_ascii_lowercase());
        } else if !result.ends_with('-') {
            result.push('-');
        }
    }
    result.trim_matches('-').to_string()
}

struct ShutdownSignal {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    pipewire_failure_rx: Option<tokio::sync::watch::Receiver<Option<String>>>,
    pipewire_failure: Option<String>,
}

impl ShutdownSignal {
    fn new() -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
                pipewire_failure_rx: None,
                pipewire_failure: None,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                pipewire_failure_rx: None,
                pipewire_failure: None,
            })
        }
    }

    fn watch_pipewire(&mut self, receiver: tokio::sync::watch::Receiver<Option<String>>) {
        self.pipewire_failure_rx = Some(receiver);
    }

    fn unwatch_pipewire(&mut self) {
        if self.pipewire_failure.is_none() {
            self.pipewire_failure = self.pipewire_failure();
        }
        self.pipewire_failure_rx = None;
    }

    fn result(&self) -> anyhow::Result<()> {
        if let Some(failure) = self.pipewire_failure() {
            Err(anyhow::anyhow!("PipeWire failure: {failure}"))
        } else {
            Ok(())
        }
    }

    fn pipewire_failure(&self) -> Option<String> {
        self.pipewire_failure.clone().or_else(|| {
            self.pipewire_failure_rx
                .as_ref()
                .and_then(|receiver| receiver.borrow().clone())
        })
    }

    fn record_pipewire_failure(&mut self, failure: String) {
        tracing::error!(%failure, "PipeWire thread failed");
        self.pipewire_failure = Some(failure);
    }

    async fn wait(&mut self) {
        if self.pipewire_failure.is_some() {
            return;
        }
        if let Some(failure) = self.pipewire_failure() {
            self.record_pipewire_failure(failure);
            return;
        }
        let changed = async {
            match self.pipewire_failure_rx.as_mut() {
                Some(receiver) => receiver.changed().await,
                None => std::future::pending().await,
            }
        };
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            result = changed => {
                let failure = self.pipewire_failure().unwrap_or_else(|| {
                    if result.is_err() {
                        "PipeWire thread stopped without an error report".to_string()
                    } else {
                        "PipeWire failure notification did not contain an error".to_string()
                    }
                });
                self.record_pipewire_failure(failure);
            }
        }
        #[cfg(not(unix))]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            result = changed => {
                let failure = self.pipewire_failure().unwrap_or_else(|| {
                    if result.is_err() {
                        "PipeWire thread stopped without an error report".to_string()
                    } else {
                        "PipeWire failure notification did not contain an error".to_string()
                    }
                });
                self.record_pipewire_failure(failure);
            }
        }
    }
}

fn active_exclusions(
    exclusions: &mut HashMap<bluer::Address, tokio::time::Instant>,
    now: tokio::time::Instant,
) -> (Vec<bluer::Address>, Option<tokio::time::Instant>) {
    exclusions.retain(|_, deadline| *deadline > now);
    let addresses = exclusions.keys().copied().collect();
    let next_expiry = exclusions.values().min().copied();
    (addresses, next_expiry)
}

/// Wait for device to be connected, using event stream if possible.
async fn ensure_connected(device: &bluer::Device, retry_delay: Duration) {
    match device.is_connected().await {
        Ok(true) => return,
        Err(e) => tracing::warn!("is_connected() failed, assuming disconnected: {e}"),
        _ => {}
    }
    tracing::info!("Waiting for device to connect...");
    if let Ok(mut events) = device.events().await {
        if matches!(device.is_connected().await, Ok(true)) {
            return;
        }
        while let Some(event) = futures::StreamExt::next(&mut events).await {
            if let bluer::DeviceEvent::PropertyChanged(bluer::DeviceProperty::Connected(true)) =
                event
            {
                return;
            }
        }
    }
    // Fallback: poll
    loop {
        tokio::time::sleep(retry_delay).await;
        match device.is_connected().await {
            Ok(true) => return,
            Err(e) => tracing::warn!("is_connected() poll failed: {e}"),
            _ => {}
        }
    }
}

/// Check if an error indicates the device is locked by another instance.
fn is_device_locked_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<bluer::Error>()
        .is_some_and(|error| matches!(error.kind, bluer::ErrorKind::NotPermitted))
}

/// Timeout waiting for CAPS_RESP during negotiation.
const NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Send GET_CAPS and wait for CAPS_RESP to determine protocol version.
async fn negotiate(
    ble: &(impl atvv::BleDevice + ?Sized),
) -> anyhow::Result<(protocol::types::Capabilities, atvv::BleStreams)> {
    use futures::StreamExt;

    // Subscribe to all streams up front. Only CTL is needed for negotiation,
    // but the others must be acquired now since BLE notification handles are
    // exclusive (can't re-subscribe later).
    let mut ctl_stream = ble.ctl_notifications().await?;
    let rx_stream = ble.rx_notifications().await?;
    let device_events = ble.connection_events().await?;

    ble.write_command(&protocol::get_caps_cmd()).await?;
    tracing::info!("Sent GET_CAPS (v1.0)");

    let deadline = tokio::time::sleep(NEGOTIATE_TIMEOUT);
    tokio::pin!(deadline);

    // BleStream<T> is Pin<Box<dyn Stream>>, already Unpin -- no tokio::pin! needed.
    loop {
        tokio::select! {
            data = ctl_stream.next() => {
                match data {
                    Some(data) => {
                        if let Some(caps) = protocol::parse_caps_resp(&data) {
                            tracing::info!(
                                "CAPS_RESP: version={}, codecs={:?}, model={:?}, frame_size={}",
                                caps.version, caps.codecs, caps.interaction_model, caps.audio_frame_size.0
                            );
                            return Ok((caps, atvv::BleStreams {
                                ctl: ctl_stream,
                                rx: rx_stream,
                                events: device_events,
                            }));
                        }
                        // Not a CAPS_RESP (e.g., stale START_SEARCH) -- keep waiting
                        tracing::debug!("Ignoring non-CAPS_RESP CTL during negotiation: {:02x?}", data);
                    }
                    None => {
                        anyhow::bail!("device disconnected during negotiation");
                    }
                }
            }
            _ = &mut deadline => {
                anyhow::bail!("timeout waiting for CAPS_RESP ({}s)", NEGOTIATE_TIMEOUT.as_secs());
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    let config = config::Config::load(cli.config.as_deref())?;
    if let Some(command) = cli.command.clone() {
        match command {
            commands::Command::Run => {}
            commands::Command::TestAudio => cli.no_input = true,
            commands::Command::TestDictation => {
                println!("Focus a test input field, then hold and release the RC003 voice key.");
            }
            other => return commands::run(other, cli.config.as_deref()).await,
        }
    }

    if cli.device.is_none() && !config.device.address.is_empty() {
        cli.device = Some(config.device.address.clone());
    }
    if cli.name.is_none() {
        cli.name = Some(config.audio.node_name.clone());
    }
    if cli.description.is_none() {
        cli.description = Some(config.audio.description.clone());
    }
    cli.no_input |= !config.input.enabled;

    // Init tracing based on verbosity
    let filter = match cli.verbose {
        0 => config.logging.level.as_str(),
        1 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .init();

    // Validate --name early.
    if let Some(ref name) = cli.name {
        let sanitized = sanitize_name(name);
        if sanitized != *name {
            anyhow::bail!(
                "--name {name:?} contains invalid characters. \
                 Use lowercase alphanumeric, hyphens, and underscores only (e.g. --name {sanitized:?})."
            );
        }
    }

    if cli.mic_on_demand && cli.frame_timeout == 0 {
        tracing::warn!("--mic-on-demand without --frame-timeout: mic may get stuck in Opening state if remote is asleep");
    }

    let mut shutdown = ShutdownSignal::new()?;

    let _input_handle = if cli.no_input {
        None
    } else {
        Some(input::InputManagerHandle::start_with_release_grab(
            config.input.release_grab_on_exit,
            Duration::from_millis(config.audio.release_drain_ms),
            input::voice_key_code(&config.input.voice_key)?,
            config.mapping.enabled,
            cli.device.clone(),
        )?)
    };

    let retry_delay = Duration::from_millis(config.device.reconnect_initial_ms);
    let discovery_retry_delay = retry_delay;
    let reconnect_max_delay = Duration::from_millis(config.device.reconnect_max_ms);
    let mut reconnect_delay = retry_delay;

    // Connect to BlueZ
    let session = bluer::Session::new().await?;
    let adapter = match &cli.adapter {
        Some(name) => session.adapter(name)?,
        None => session.default_adapter().await?,
    };
    tracing::info!("Using adapter: {}", adapter.name());

    let filter_addr: Option<bluer::Address> = cli
        .device
        .map(|s| s.parse().context("failed to parse device address"))
        .transpose()?;

    let timeouts = atvv::SessionTimeouts {
        frame_timeout: std::time::Duration::from_secs(cli.frame_timeout),
        keepalive: std::time::Duration::from_secs(cli.keep_alive),
    };

    // Locked devices are skipped briefly, then retried in case the other owner exited.
    let mut excluded_addrs: HashMap<bluer::Address, tokio::time::Instant> = HashMap::new();

    // Outer loop: discover → connect → session. Restarts on lock errors in auto mode.
    'discover: loop {
        let (excluded_vec, exclusion_deadline) =
            active_exclusions(&mut excluded_addrs, tokio::time::Instant::now());
        let device = loop {
            tokio::select! {
                result = async {
                    let discovery = ble::find_atvv_device(&adapter, filter_addr, &excluded_vec);
                    if let Some(deadline) = exclusion_deadline {
                        match tokio::time::timeout_at(deadline, discovery).await {
                            Ok(result) => result,
                            Err(_) => anyhow::bail!("device lock exclusion expired"),
                        }
                    } else {
                        discovery.await
                    }
                } => {
                    match result {
                        Ok(device) => break device,
                        Err(_) if exclusion_deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now()) => {
                            continue 'discover;
                        }
                        Err(e) => {
                            tracing::info!("No ATVV device found ({e}), retrying in {discovery_retry_delay:?}...");
                            tokio::select! {
                                _ = tokio::time::sleep(discovery_retry_delay) => {}
                                _ = shutdown.wait() => return shutdown.result(),
                            }
                        }
                    }
                }
                _ = shutdown.wait() => {
                    tracing::info!("Shutting down");
                    return shutdown.result();
                }
            }
        };

        tokio::select! {
            _ = ensure_connected(&device, retry_delay) => {}
            _ = shutdown.wait() => {
                tracing::info!("Shutting down");
                return shutdown.result();
            }
        }

        // Resolve GATT characteristics (retries on failure, interruptible)
        let mut chars = loop {
            tokio::select! {
                result = ble::resolve_chars(&device) => {
                    match result {
                        Ok(c) => {
                            tracing::info!("ATVV characteristics resolved");
                            break c;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to resolve characteristics ({e}), retrying in 2s..."
                            );
                            tokio::select! {
                                _ = tokio::time::sleep(retry_delay) => {}
                                _ = shutdown.wait() => return shutdown.result(),
                            }
                        }
                    }
                }
                _ = shutdown.wait() => {
                    tracing::info!("Shutting down");
                    return shutdown.result();
                }
            }
        };

        // Perform vendor handshake if the device requires one (e.g. Philips TV Voice remotes).
        // Keep the returned stream alive: dropping it calls StopNotify on ff02,
        // which resets the remote's ATVV-ready state before GET_CAPS can succeed.
        let mut _vendor_ff02_stream = tokio::select! {
            result = ble::vendor_handshake(&device) => match result {
                Ok(stream) => stream,
                Err(e) => {
                    tracing::warn!("Vendor handshake failed: {e}");
                    None
                }
            },
            _ = shutdown.wait() => return shutdown.result(),
        };

        let node_name = cli.name.clone().expect("node name was configured");
        #[allow(unused_variables)]
        let dbus_name = format!("org.rc003omarchy.{}", sanitize_name(&node_name));
        let node_description = cli
            .description
            .clone()
            .expect("node description was configured");

        let gain = cli.gain.unwrap_or(config.audio.gain_db);

        // Created unconditionally: the session loop sends state updates regardless of D-Bus,
        // and the cost of an unused watch channel is negligible.
        let (state_tx, _state_rx) = tokio::sync::watch::channel(atvv::State::Disconnected);

        // Set up D-Bus control interface (if feature and CLI allow).
        #[cfg(feature = "dbus")]
        let (mut dbus_cmd_rx, _dbus_conn) = if !cli.no_dbus {
            #[cfg(feature = "dbus")]
            let device_addr = device.address().to_string();
            let info = dbus::DaemonInfo {
                device_address: device_addr,
                node_name: node_name.clone(),
            };
            match dbus::serve(_state_rx, info, &dbus_name).await {
                Ok((cmd_rx, conn)) => (Some(cmd_rx), Some(conn)),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("already taken")
                        || msg.contains("NameAlreadyOwned")
                        || msg.contains("exists")
                    {
                        anyhow::bail!(
                            "D-Bus name '{dbus_name}' is already in use. \
                             Another rc003-omarchy instance may be running with the same name. \
                             Use --name <suffix> to differentiate instances, or --no-dbus to disable."
                        );
                    }
                    tracing::warn!("Failed to register D-Bus interface: {e}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let pcm_tx = std::sync::Arc::new(ArrayQueue::new(32));
        let pcm_rx = pcm_tx.clone();
        let (consumer_tx, mut consumer_rx) =
            tokio::sync::watch::channel(consumer::ConsumerEvent::Changed(0));
        let mut _pw_handle = PipeWireHandle::new(
            pcm_rx.clone(),
            gain,
            node_name.clone(),
            node_description.clone(),
            Some(consumer_tx),
        )?;
        let failure_rx = _pw_handle
            .failure_rx
            .take()
            .expect("PipeWire failure receiver is registered once");
        shutdown.watch_pipewire(failure_rx);

        // Inner loop: session → reconnect (same device).
        loop {
            let ble_device = ble::BluerDevice {
                device: &device,
                chars: &chars,
            };

            // Negotiate protocol version
            let (caps, streams) = tokio::select! {
                result = negotiate(&ble_device) => match result {
                    Ok(r) => r,
                    Err(e) => {
                        if is_device_locked_error(&e) {
                            if filter_addr.is_some() {
                                anyhow::bail!(
                                    "Device {} is already in use by another rc003-omarchy instance.",
                                    ble::redact_address(device.address())
                                );
                            } else {
                                tracing::warn!(
                                    "Device {} is locked by another instance, looking for another device...",
                                    ble::redact_address(device.address())
                                );
                                excluded_addrs.insert(
                                    device.address(),
                                    tokio::time::Instant::now() + DEVICE_LOCK_EXCLUSION,
                                );
                                shutdown.unwatch_pipewire();
                                drop(_pw_handle);
                                continue 'discover;
                            }
                        }
                        tracing::error!("Negotiation failed: {e}");
                        tokio::select! {
                            _ = tokio::time::sleep(retry_delay) => {}
                            _ = shutdown.wait() => {
                                tracing::info!("Shutting down");
                                break 'discover;
                            }
                        }
                        continue;
                    }
                },
                _ = shutdown.wait() => {
                    tracing::info!("interrupted during negotiation");
                    break 'discover;
                }
            };
            excluded_addrs.remove(&device.address());
            let _ = state_tx.send(atvv::State::Connected);
            let (mut session_protocol, codec) = match protocol::create_protocol(&caps) {
                Ok((mut p, c)) => {
                    p.set_frame_layout(config.audio.frame_layout);
                    (p, c)
                }
                Err(e) => {
                    tracing::error!("Protocol creation failed: {e}");
                    tokio::select! {
                        _ = tokio::time::sleep(reconnect_delay) => {}
                        _ = shutdown.wait() => break 'discover,
                    }
                    continue;
                }
            };
            if codec.sample_rate() != 16_000 {
                tracing::error!(
                    sample_rate = codec.sample_rate(),
                    "unsupported RC003 sample rate"
                );
                tokio::select! {
                    _ = tokio::time::sleep(reconnect_delay) => {}
                    _ = shutdown.wait() => break 'discover,
                }
                continue;
            }
            tracing::info!(
                "Negotiated protocol: {} ({:?}, {}Hz)",
                session_protocol.version(),
                codec,
                codec.sample_rate()
            );

            let (frame_tx, mut frame_rx) =
                tokio::sync::mpsc::channel::<protocol::types::AudioFrame>(DECODER_CHANNEL_CAPACITY);
            let pcm_tx = pcm_tx.clone();
            // Post-processing task: Protocol already decoded the audio,
            // so we just apply declip/lowpass/gain and forward PCM.
            let decoder_handle = tokio::spawn(async move {
                while let Some(frame) = frame_rx.recv().await {
                    let mut samples = frame.samples;
                    adpcm::declip(&mut samples);
                    adpcm::lowpass(&mut samples);
                    let _ = pcm_tx.force_push(pw::PcmFrame {
                        generation: frame.generation,
                        samples,
                    });
                }
            });

            let session_result = tokio::select! {
                result = atvv::run_session(
                    &ble_device,
                    &mut *session_protocol,
                    streams,
                    atvv::SessionConfig {
                        audio_tx: frame_tx,
                        timeouts: &timeouts,

                        command_rx: {
                            #[cfg(feature = "dbus")]
                            { dbus_cmd_rx.as_mut() }
                            #[cfg(not(feature = "dbus"))]
                            { None }
                        },
                        state_tx: Some(&state_tx),
                        consumer_rx: if cli.mic_on_demand {
                            Some(&mut consumer_rx)
                        } else {
                            None
                        },
                        generation: Some(&_pw_handle.generation),
                        tail_expiration: Some(&_pw_handle.tail_expiration),
                        mic_on_demand: cli.mic_on_demand,
                    },
                ) => result,
                _ = shutdown.wait() => {
                    let mic_close = session_protocol.mic_close_cmd(
                        protocol::types::StreamId::ANY,
                    );
                    let _ = atvv::BleDevice::write_command(&ble_device, &mic_close).await;
                    tracing::info!("Sent MIC_CLOSE, shutting down");
                    break 'discover;
                }
            };

            // frame_tx was moved into run_session and is now dropped, so the
            // decoder task finishes naturally. The PipeWire source stays alive
            // across reconnects; only its PCM producer is replaced.
            let _ = decoder_handle.await;

            match &session_result {
                Ok(()) => {
                    tracing::info!("Session ended");
                    reconnect_delay = retry_delay;
                }
                Err(e) if is_device_locked_error(e) => {
                    if filter_addr.is_some() {
                        // Explicit --device: fatal, user asked for this specific device.
                        anyhow::bail!(
                            "Device {} is already in use by another rc003-omarchy instance.",
                            ble::redact_address(device.address())
                        );
                    } else {
                        // Auto mode: skip this device, try to find another.
                        tracing::warn!(
                            "Device {} is locked by another instance, looking for another device...",
                            ble::redact_address(device.address())
                        );
                        excluded_addrs.insert(
                            device.address(),
                            tokio::time::Instant::now() + DEVICE_LOCK_EXCLUSION,
                        );
                        shutdown.unwatch_pipewire();
                        drop(_pw_handle);
                        continue 'discover;
                    }
                }
                Err(e) => tracing::warn!("Session error: {e}"),
            }

            if !config.device.auto_connect {
                break 'discover;
            }
            tokio::select! {
                _ = tokio::time::sleep(reconnect_delay) => {}
                _ = shutdown.wait() => {
                    tracing::info!("Interrupted during reconnect backoff, shutting down");
                    break 'discover;
                }
            }
            reconnect_delay = std::cmp::min(reconnect_delay * 2, reconnect_max_delay);

            // Update D-Bus state
            let _ = state_tx.send(atvv::State::Disconnected);

            // Wait for device to reconnect (C1: interruptible by ctrl+c)
            tokio::select! {
                _ = ensure_connected(&device, retry_delay) => {
                    tracing::info!("Device reconnected");
                }
                _ = shutdown.wait() => {
                    tracing::info!("Interrupted during reconnection, shutting down");
                    break 'discover;
                }
            }

            // Re-resolve characteristics (handles may change after reconnect)
            // Also re-run vendor handshake since some devices (e.g. Philips) reset their
            // ATVV-ready state on reconnect.
            chars = loop {
                tokio::select! {
                    result = ble::resolve_chars(&device) => {
                        match result {
                            Ok(c) => {
                                tracing::info!("ATVV characteristics re-resolved");
                                // Re-run vendor handshake (e.g. Philips remotes reset state on reconnect)
                                _vendor_ff02_stream = tokio::select! {
                                    result = ble::vendor_handshake(&device) => match result {
                                        Ok(stream) => stream,
                                        Err(e) => {
                                            tracing::warn!("Vendor handshake failed after reconnect: {e}");
                                            None
                                        }
                                    },
                                    _ = shutdown.wait() => break 'discover,
                                };
                                break c;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to re-resolve characteristics ({e}), retrying in 2s..."
                                );
                                tokio::select! {
                                    _ = tokio::time::sleep(retry_delay) => {}
                                    _ = shutdown.wait() => break 'discover,
                                }
                            }
                        }
                    }
                    _ = shutdown.wait() => {
                        tracing::info!("Interrupted during characteristic resolution, shutting down");
                        break 'discover;
                    }
                }
            };
        }
    }

    shutdown.result()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unwatch_ignores_normal_close_and_preserves_real_failure() {
        let mut shutdown = ShutdownSignal::new().unwrap();
        let (failure_tx, failure_rx) = tokio::sync::watch::channel(None);
        shutdown.watch_pipewire(failure_rx);
        shutdown.unwatch_pipewire();
        drop(failure_tx);
        assert!(shutdown.result().is_ok());

        let (failure_tx, failure_rx) = tokio::sync::watch::channel(None);
        shutdown.watch_pipewire(failure_rx);
        failure_tx.send(Some("real failure".to_string())).unwrap();
        shutdown.unwatch_pipewire();
        assert!(shutdown.result().is_err());
    }

    #[tokio::test]
    async fn pipewire_failure_survives_a_consumed_notification() {
        let mut shutdown = ShutdownSignal::new().unwrap();
        let (failure_tx, failure_rx) = tokio::sync::watch::channel(None);
        shutdown.watch_pipewire(failure_rx);
        failure_tx
            .send(Some("injected failure".to_string()))
            .unwrap();
        shutdown
            .pipewire_failure_rx
            .as_mut()
            .unwrap()
            .changed()
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), shutdown.wait())
            .await
            .expect("stored PipeWire failure did not wake the lifecycle");
        assert_eq!(
            shutdown.pipewire_failure().as_deref(),
            Some("injected failure")
        );
    }

    #[tokio::test]
    async fn closed_pipewire_failure_channel_is_an_error() {
        let mut shutdown = ShutdownSignal::new().unwrap();
        let (failure_tx, failure_rx) = tokio::sync::watch::channel(None);
        shutdown.watch_pipewire(failure_rx);
        drop(failure_tx);
        tokio::time::timeout(Duration::from_secs(1), shutdown.wait())
            .await
            .expect("closed PipeWire channel did not wake the lifecycle");
        assert!(shutdown.pipewire_failure().is_some());
    }

    #[test]
    fn locked_device_exclusions_expire() {
        let address: bluer::Address = "C0:5D:39:FA:2A:1B".parse().unwrap();
        let now = tokio::time::Instant::now();
        let mut exclusions = HashMap::from([(address, now + Duration::from_secs(1))]);
        let (active, deadline) = active_exclusions(&mut exclusions, now);
        assert_eq!(active, vec![address]);
        assert!(deadline.is_some());
        let (active, deadline) = active_exclusions(&mut exclusions, now + Duration::from_secs(2));
        assert!(active.is_empty());
        assert!(deadline.is_none());
    }

    #[test]
    fn only_not_permitted_counts_as_device_lock() {
        let locked = bluer::Error {
            kind: bluer::ErrorKind::NotPermitted,
            message: "exclusive acquire failed".to_string(),
        };
        assert!(is_device_locked_error(&anyhow::Error::new(locked)));
        let in_progress = bluer::Error {
            kind: bluer::ErrorKind::InProgress,
            message: "temporary disconnect".to_string(),
        };
        assert!(!is_device_locked_error(&anyhow::Error::new(in_progress)));
        assert!(!is_device_locked_error(&anyhow::Error::msg(
            "exclusive acquire failed"
        )));
    }
}
