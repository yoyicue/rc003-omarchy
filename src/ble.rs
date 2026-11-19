//! BLE device discovery and ATVV GATT characteristic resolution.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bluer::{
    gatt::{remote::Characteristic, WriteOp},
    Adapter, AdapterEvent, Address, Device, Uuid,
};
use futures::{Stream, StreamExt};
use zbus::zvariant::OwnedValue;
use zbus::{Connection, MatchRule, MessageStream, Proxy};

use crate::atvv::{
    AudioPacket, BleDevice, BleFut, BleStream, ControlPacket, DeviceConnectionEvent,
    SessionNotification,
};

fn notification_payload(data: Vec<u8>) -> Option<Vec<u8>> {
    (!data.is_empty()).then_some(data)
}

fn advance_generation_on_audio_start(data: &[u8], generation: &AtomicU64) {
    if !data.is_empty() && data[0] == 0x04 {
        generation.fetch_add(1, Ordering::Relaxed);
    }
}

struct AudioNotificationStream {
    inner: Pin<Box<dyn Stream<Item = SessionNotification> + Send>>,
    task: tokio::task::JoinHandle<()>,
}

impl Stream for AudioNotificationStream {
    type Item = SessionNotification;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl Drop for AudioNotificationStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
struct NotificationQueue {
    events: tokio::sync::Mutex<VecDeque<SessionNotification>>,
    closed: AtomicBool,
}

impl NotificationQueue {
    async fn push(&self, event: SessionNotification) {
        let is_audio = matches!(event, SessionNotification::Audio(_));
        let mut events = self.events.lock().await;
        if is_audio && events.len() >= 64 {
            if let Some(index) = events
                .iter()
                .position(|event| matches!(event, SessionNotification::Audio(_)))
            {
                let _ = events.remove(index);
            }
        }
        events.push_back(event);
    }

    async fn next(&self) -> Option<SessionNotification> {
        loop {
            if let Some(event) = self.events.lock().await.pop_front() {
                return Some(event);
            }
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}

struct NotificationPaths {
    ctl: String,
    rx: String,
}

impl NotificationPaths {
    fn new(chars: &AtvvChars) -> Self {
        Self {
            ctl: characteristic_path(&chars.ctl),
            rx: characteristic_path(&chars.rx),
        }
    }
}

fn characteristic_path(characteristic: &Characteristic) -> String {
    format!(
        "/org/bluez/{}/dev_{}/service{:04x}/char{:04x}",
        characteristic.adapter_name(),
        characteristic
            .device_address()
            .to_string()
            .replace(':', "_"),
        characteristic.service_id(),
        characteristic.id()
    )
}

async fn forward_dbus_notifications(
    connection: Connection,
    paths: NotificationPaths,
    mut messages: MessageStream,
    generation: Arc<AtomicU64>,
    queue: Arc<NotificationQueue>,
) -> Result<()> {
    while let Some(message) = messages.next().await {
        let message = message?;
        let header = message.header();
        let Some(path) = header.path() else {
            continue;
        };
        let body = message.body();
        let Ok((interface, changed, _)) =
            body.deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
        else {
            continue;
        };
        if interface != "org.bluez.GattCharacteristic1" {
            continue;
        }
        let Some(value) = changed.get("Value") else {
            continue;
        };
        let Ok(data) = Vec::<u8>::try_from(value.clone()) else {
            continue;
        };
        let Some(data) = notification_payload(data) else {
            continue;
        };
        if path.as_str() == paths.ctl {
            advance_generation_on_audio_start(&data, &generation);
            queue
                .push(SessionNotification::Control(ControlPacket {
                    data,
                    generation: generation.load(Ordering::Relaxed),
                }))
                .await;
        } else if path.as_str() == paths.rx {
            queue
                .push(SessionNotification::Audio(AudioPacket {
                    data,
                    generation: generation.load(Ordering::Relaxed),
                }))
                .await;
        }
    }
    drop(connection);
    Ok(())
}

async fn ordered_notifications(
    chars: &AtvvChars,
    generation: Arc<AtomicU64>,
) -> Result<Pin<Box<dyn Stream<Item = SessionNotification> + Send>>> {
    let paths = NotificationPaths::new(chars);
    let connection = Connection::system().await?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path_namespace("/org/bluez")?
        .build();
    let messages = MessageStream::for_match_rule(rule, &connection, Some(256)).await?;
    for path in [&paths.ctl, &paths.rx] {
        Proxy::new(
            &connection,
            "org.bluez",
            path.as_str(),
            "org.bluez.GattCharacteristic1",
        )
        .await?
        .call_method("StartNotify", &())
        .await?;
    }
    let queue = Arc::new(NotificationQueue::default());
    let task_queue = queue.clone();
    let task = tokio::spawn(async move {
        let _ =
            forward_dbus_notifications(connection, paths, messages, generation, task_queue.clone())
                .await;
        task_queue.close();
    });
    Ok(audio_stream(task, queue))
}

fn audio_stream(
    task: tokio::task::JoinHandle<()>,
    queue: Arc<NotificationQueue>,
) -> Pin<Box<dyn Stream<Item = SessionNotification> + Send>> {
    let inner: Pin<Box<dyn Stream<Item = SessionNotification> + Send>> =
        Box::pin(futures::stream::unfold(queue, |queue| async move {
            queue.next().await.map(|event| (event, queue))
        }));
    Box::pin(AudioNotificationStream { inner, task })
}

fn atvv_write_op() -> WriteOp {
    WriteOp::Command
}

pub fn redact_address(address: Address) -> String {
    let text = address.to_string();
    format!("{}:…:{}", &text[..5], &text[text.len() - 5..])
}

/// ATVV Service UUID: AB5E0001-5A21-4F05-BC7D-AF01F617B664
pub const ATVV_SERVICE: Uuid = Uuid::from_u128(0xab5e0001_5a21_4f05_bc7d_af01f617b664);

/// ATVV TX Characteristic (Host → Remote): AB5E0002
pub const ATVV_CHAR_TX: Uuid = Uuid::from_u128(0xab5e0002_5a21_4f05_bc7d_af01f617b664);

/// ATVV RX Characteristic (Remote → Host, audio): AB5E0003
pub const ATVV_CHAR_RX: Uuid = Uuid::from_u128(0xab5e0003_5a21_4f05_bc7d_af01f617b664);

/// ATVV CTL Characteristic (Remote → Host, control): AB5E0004
pub const ATVV_CHAR_CTL: Uuid = Uuid::from_u128(0xab5e0004_5a21_4f05_bc7d_af01f617b664);

/// Philips vendor write characteristic UUID (ff01) — vendor activation channel.
/// Writing "ntf_enable" here signals the remote to enter ATVV-ready mode.
/// Full UUID as observed on device: 02f00000-0000-0000-0000-00000000ff01
const PHILIPS_VENDOR_FF01: Uuid = Uuid::from_u128(0x02f00000_0000_0000_0000_00000000ff01);

/// Philips vendor notify characteristic UUID (ff02) — must be subscribed before writing ff01.
/// Full UUID as observed on device: 02f00000-0000-0000-0000-00000000ff02
const PHILIPS_VENDOR_FF02: Uuid = Uuid::from_u128(0x02f00000_0000_0000_0000_00000000ff02);

/// Resolved ATVV characteristics for a connected device.
pub struct AtvvChars {
    pub tx: Characteristic,
    pub rx: Characteristic,
    pub ctl: Characteristic,
}

impl std::fmt::Debug for AtvvChars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtvvChars")
            .field("tx", &"<Characteristic>")
            .field("rx", &"<Characteristic>")
            .field("ctl", &"<Characteristic>")
            .finish()
    }
}

/// Real BLE device implementation wrapping bluer types.
/// Borrows the device and characteristics so main.rs retains ownership
/// for reconnect logic and shutdown MIC_CLOSE.
pub struct BluerDevice<'a> {
    pub device: &'a Device,
    pub chars: &'a AtvvChars,
    pub generation: Arc<AtomicU64>,
}

impl<'a> BluerDevice<'a> {
    pub fn new(device: &'a Device, chars: &'a AtvvChars, generation: Arc<AtomicU64>) -> Self {
        Self {
            device,
            chars,
            generation,
        }
    }
}

impl BleDevice for BluerDevice<'_> {
    fn write_command(&self, data: &[u8]) -> BleFut<'_, ()> {
        let data = data.to_vec();
        Box::pin(async move {
            // RC003 accepts ATT Write Command (write-without-response). Using a
            // Write Request here fails with org.bluez.Error.NotSupported on Redmi.
            self.chars
                .tx
                .write_ext(
                    &data,
                    &bluer::gatt::remote::CharacteristicWriteRequest {
                        op_type: atvv_write_op(),
                        ..Default::default()
                    },
                )
                .await?;
            Ok(())
        })
    }

    fn notifications(&self) -> BleFut<'_, BleStream<SessionNotification>> {
        Box::pin(async { ordered_notifications(self.chars, self.generation.clone()).await })
    }

    fn connection_events(&self) -> BleFut<'_, BleStream<DeviceConnectionEvent>> {
        Box::pin(async {
            let stream = self.device.events().await?;
            let mapped = stream.filter_map(|event| async move {
                if let bluer::DeviceEvent::PropertyChanged(bluer::DeviceProperty::Connected(
                    false,
                )) = event
                {
                    Some(DeviceConnectionEvent::Disconnected)
                } else {
                    None
                }
            });
            Ok(Box::pin(mapped) as BleStream<DeviceConnectionEvent>)
        })
    }
}

/// Returns `true` if the given address should be skipped during discovery.
fn should_skip(addr: Address, filter_addr: Option<Address>, excluded: &[Address]) -> bool {
    if excluded.contains(&addr) {
        return true;
    }
    if let Some(filter) = filter_addr {
        if addr != filter {
            return true;
        }
    }
    false
}

/// Find a bonded device that advertises the ATVV service.
/// If `filter_addr` is Some, only match that specific address.
/// Addresses in `exclude` are skipped (e.g. devices locked by another instance).
pub async fn find_atvv_device(
    adapter: &Adapter,
    filter_addr: Option<Address>,
    exclude: &[Address],
) -> Result<Device> {
    // First check already-known devices
    for addr in adapter.device_addresses().await? {
        if should_skip(addr, filter_addr, exclude) {
            continue;
        }
        let device = adapter.device(addr)?;
        if let Ok(Some(uuids)) = device.uuids().await {
            if uuids.contains(&ATVV_SERVICE) {
                tracing::info!(
                    "Found ATVV device: {} ({})",
                    device.name().await.ok().flatten().unwrap_or_default(),
                    redact_address(addr)
                );
                return Ok(device);
            }
        }
    }

    // Fall back to discovery stream
    tracing::info!("Scanning for ATVV devices...");
    let discover = adapter.discover_devices().await?;
    tokio::pin!(discover);
    while let Some(evt) = discover.next().await {
        if let AdapterEvent::DeviceAdded(addr) = evt {
            if should_skip(addr, filter_addr, exclude) {
                continue;
            }
            let device = adapter.device(addr)?;
            if let Ok(Some(uuids)) = device.uuids().await {
                if uuids.contains(&ATVV_SERVICE) {
                    tracing::info!("Discovered ATVV device: {}", redact_address(addr));
                    return Ok(device);
                }
            }
        }
    }

    anyhow::bail!(
        "BLE discovery stream ended without finding an ATVV device (adapter may have been removed)"
    )
}

/// Resolve the three ATVV GATT characteristics from a connected device.
pub async fn resolve_chars(device: &Device) -> Result<AtvvChars> {
    let mut tx = None;
    let mut rx = None;
    let mut ctl = None;

    for service in device.services().await? {
        if service.uuid().await? != ATVV_SERVICE {
            continue;
        }

        for char in service.characteristics().await? {
            match char.uuid().await? {
                uuid if uuid == ATVV_CHAR_TX => tx = Some(char),
                uuid if uuid == ATVV_CHAR_RX => rx = Some(char),
                uuid if uuid == ATVV_CHAR_CTL => ctl = Some(char),
                _ => {}
            }
        }
        break; // Found the ATVV service; no need to check other services.
    }

    Ok(AtvvChars {
        tx: tx.context("ATVV TX characteristic not found")?,
        rx: rx.context("ATVV RX characteristic not found")?,
        ctl: ctl.context("ATVV CTL characteristic not found")?,
    })
}

/// Attempt Philips vendor handshake if the device requires it.
///
/// Some Philips TV Voice remotes (e.g. URMT26RST004) require a proprietary
/// vendor handshake before they will respond to ATVV protocol commands.
///
/// The handshake sequence (reverse-engineered from device traffic):
///  1. Subscribe to ff02 notifications (remote sends "ntf_enable" as a greeting)
///  2. Write "ntf_enable" to ff01 (signals readiness to the remote)
///  3. Wait ~500ms for remote to enter ATVV-ready state
///
/// After this sequence, the remote accepts ATT subscriptions on ATVV characteristics
/// and responds to GET_CAPS commands.
///
/// Returns `Ok(true)` if the handshake was performed, `Ok(false)` if not a Philips device.
pub async fn vendor_handshake(device: &Device) -> Result<Option<crate::atvv::BleStream<Vec<u8>>>> {
    let mut ff01_char = None;
    let mut ff02_char = None;

    for service in device.services().await? {
        if service.uuid().await? == ATVV_SERVICE {
            continue;
        }
        for char in service.characteristics().await? {
            match char.uuid().await? {
                uuid if uuid == PHILIPS_VENDOR_FF01 => ff01_char = Some(char),
                uuid if uuid == PHILIPS_VENDOR_FF02 => ff02_char = Some(char),
                _ => {}
            }
        }
        if ff01_char.is_some() {
            break;
        }
    }

    let ff01 = match ff01_char {
        Some(c) => c,
        None => return Ok(None), // Not a Philips device
    };

    tracing::info!("Detected Philips vendor service. Performing vendor handshake...");

    // Step 1: Subscribe to ff02 (required before remote accepts ff01 write).
    // The remote sends "ntf_enable" on ff02 as a greeting when subscribed.
    // We return this stream to the caller so it stays subscribed for the session —
    // dropping it (StopNotify) resets the remote's ATVV-ready state.
    let ff02_stream: Option<crate::atvv::BleStream<Vec<u8>>> = if let Some(ff02) = ff02_char {
        match ff02.notify().await {
            Ok(stream) => {
                tracing::debug!("Subscribed to vendor ff02 notifications");
                Some(Box::pin(stream))
            }
            Err(e) => {
                tracing::warn!("Vendor ff02 StartNotify failed (continuing): {e}");
                None
            }
        }
    } else {
        tracing::warn!("Vendor ff02 not found; handshake may be incomplete");
        None
    };

    // Step 2: Write "ntf_enable" to ff01 to signal readiness.
    ff01.write_ext(
        b"ntf_enable",
        &bluer::gatt::remote::CharacteristicWriteRequest {
            op_type: WriteOp::Request,
            ..Default::default()
        },
    )
    .await
    .context("Philips vendor handshake: write to ff01 failed")?;
    tracing::debug!("Vendor handshake: wrote ntf_enable to ff01");

    // Step 3: Wait for remote to process and enter ATVV-ready state.
    tokio::time::sleep(Duration::from_millis(500)).await;

    tracing::info!("Vendor handshake complete");
    // Return the ff02 stream so the caller can hold it alive for the session.
    // Dropping it would call StopNotify and reset the remote's ATVV-ready state.
    Ok(ff02_stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_event(data: Vec<u8>, generation: u64) -> SessionNotification {
        SessionNotification::Control(ControlPacket { data, generation })
    }

    async fn drain_audio_before_start<F>(
        mut try_recv: F,
        generation: &AtomicU64,
        queue: &NotificationQueue,
    ) -> bool
    where
        F: FnMut() -> std::io::Result<Vec<u8>>,
    {
        loop {
            match try_recv() {
                Ok(data) if !data.is_empty() => {
                    queue
                        .push(SessionNotification::Audio(AudioPacket {
                            data,
                            generation: generation.load(Ordering::Relaxed),
                        }))
                        .await;
                }
                Ok(_) => return false,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
    }

    #[test]
    fn rc003_uses_write_without_response() {
        assert!(matches!(atvv_write_op(), WriteOp::Command));
    }

    #[test]
    fn zero_length_notification_is_eof() {
        assert_eq!(notification_payload(Vec::new()), None);
        assert_eq!(notification_payload(vec![1, 2]), Some(vec![1, 2]));
    }

    #[tokio::test]
    async fn dropping_audio_stream_aborts_reader_task() {
        let inner = Box::pin(futures::stream::empty());
        let task = tokio::spawn(std::future::pending());
        let stream = AudioNotificationStream { inner, task };
        let task = stream.task.abort_handle();
        drop(stream);
        tokio::task::yield_now().await;
        assert!(task.is_finished());
    }

    #[tokio::test]
    async fn full_queue_drops_oldest_packet_without_blocking() {
        let queue = NotificationQueue::default();
        for _ in 0..64 {
            queue
                .push(SessionNotification::Audio(AudioPacket {
                    data: vec![1],
                    generation: 1,
                }))
                .await;
        }
        queue
            .push(SessionNotification::Audio(AudioPacket {
                data: vec![2],
                generation: 2,
            }))
            .await;
        for _ in 0..63 {
            let Some(SessionNotification::Audio(packet)) = queue.next().await else {
                panic!("expected audio");
            };
            assert_eq!(packet.generation, 1);
        }
        let Some(SessionNotification::Audio(packet)) = queue.next().await else {
            panic!("expected audio");
        };
        assert_eq!(packet.data, vec![2]);
        assert_eq!(packet.generation, 2);
    }

    #[tokio::test]
    async fn saturation_discards_old_generation_packets() {
        let queue = NotificationQueue::default();
        for _ in 0..64 {
            queue
                .push(SessionNotification::Audio(AudioPacket {
                    data: vec![0],
                    generation: 0,
                }))
                .await;
        }
        for _ in 0..5 {
            queue
                .push(SessionNotification::Audio(AudioPacket {
                    data: vec![1],
                    generation: 1,
                }))
                .await;
        }
        let mut old = 0;
        let mut new = 0;
        for _ in 0..64 {
            let Some(SessionNotification::Audio(packet)) = queue.next().await else {
                panic!("expected audio");
            };
            match packet.generation {
                0 => old += 1,
                1 => new += 1,
                generation => panic!("unexpected generation {generation}"),
            }
        }
        assert_eq!((old, new), (59, 5));
    }

    #[tokio::test]
    async fn control_events_survive_audio_saturation() {
        let queue = NotificationQueue::default();
        for _ in 0..64 {
            queue
                .push(SessionNotification::Audio(AudioPacket {
                    data: vec![0],
                    generation: 0,
                }))
                .await;
        }
        queue.push(control_event(vec![0x04], 0)).await;
        queue
            .push(SessionNotification::Audio(AudioPacket {
                data: vec![1],
                generation: 1,
            }))
            .await;
        for _ in 0..63 {
            assert!(matches!(
                queue.next().await,
                Some(SessionNotification::Audio(_))
            ));
        }
        assert!(matches!(
            queue.next().await,
            Some(SessionNotification::Control(_))
        ));
        assert!(matches!(
            queue.next().await,
            Some(SessionNotification::Audio(_))
        ));
    }

    #[tokio::test]
    async fn pre_start_backlog_keeps_old_generation_and_order() {
        let queue = NotificationQueue::default();
        let generation = AtomicU64::new(0);
        let mut packets = VecDeque::from([Ok(vec![1]), Ok(vec![2])]);
        assert!(
            drain_audio_before_start(
                || {
                    packets.pop_front().unwrap_or_else(|| {
                        Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
                    })
                },
                &generation,
                &queue,
            )
            .await
        );
        advance_generation_on_audio_start(&[0x04], &generation);
        queue.push(control_event(vec![0x04], 0)).await;
        for expected in [1, 2] {
            let Some(SessionNotification::Audio(packet)) = queue.next().await else {
                panic!("expected audio");
            };
            assert_eq!(packet.data, vec![expected]);
            assert_eq!(packet.generation, 0);
        }
        assert!(matches!(
            queue.next().await,
            Some(SessionNotification::Control(_))
        ));
    }

    #[tokio::test]
    async fn pre_stop_tail_is_delivered_before_control() {
        let queue = NotificationQueue::default();
        let generation = AtomicU64::new(1);
        let mut packets = VecDeque::from([Ok(vec![1]), Ok(vec![2]), Ok(vec![3]), Ok(vec![4])]);
        assert!(
            drain_audio_before_start(
                || {
                    packets.pop_front().unwrap_or_else(|| {
                        Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
                    })
                },
                &generation,
                &queue,
            )
            .await
        );
        queue.push(control_event(vec![0x00], 1)).await;
        for expected in [1, 2, 3, 4] {
            let Some(SessionNotification::Audio(packet)) = queue.next().await else {
                panic!("expected audio");
            };
            assert_eq!(packet.data, vec![expected]);
            assert_eq!(packet.generation, 1);
        }
        assert!(matches!(
            queue.next().await,
            Some(SessionNotification::Control(_))
        ));
    }

    #[tokio::test]
    async fn closed_notification_queue_returns_none() {
        let queue = NotificationQueue::default();
        queue.close();
        assert!(queue.next().await.is_none());
    }

    #[test]
    fn audio_start_advances_generation_before_audio() {
        let generation = AtomicU64::new(0);
        advance_generation_on_audio_start(&[0x04, 0x03, 0x02, 0x00], &generation);
        assert_eq!(generation.load(Ordering::Relaxed), 1);
        advance_generation_on_audio_start(&[0x00, 0x00], &generation);
        assert_eq!(generation.load(Ordering::Relaxed), 1);
        advance_generation_on_audio_start(&[0x04], &generation);
        assert_eq!(generation.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn logs_only_contain_redacted_addresses() {
        let address: Address = "C0:5D:39:FA:2A:1B".parse().unwrap();
        let redacted = redact_address(address);
        assert_eq!(redacted, "C0:5D:…:2A:1B");
        assert!(!redacted.contains("39:FA"));
    }
}
