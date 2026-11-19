use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode};

pub const RC003_VENDOR_ID: u16 = 0x2717;
pub const RC003_PRODUCT_ID: u16 = 0x32b8;
const INPUT_SCAN_INTERVAL: Duration = Duration::from_secs(2);

const KEY_POWER: u16 = 116;
const KEY_F5: u16 = 63;
const KEY_F9: u16 = 67;
const KEY_UP: u16 = 103;
const KEY_DOWN: u16 = 108;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_ENTER: u16 = 28;
const KEY_BACK: u16 = 158;
const KEY_HOME: u16 = 102;
const KEY_MENU: u16 = 127;
const KEY_TV: u16 = 41;
const KEY_VOLUME_UP: u16 = 115;
const KEY_VOLUME_DOWN: u16 = 114;
const KEY_ESC: u16 = 1;
const KEY_BACKSPACE: u16 = 14;
const KEY_F13: u16 = 183;
const KEY_F14: u16 = 184;
const KEY_F15: u16 = 185;
const KEY_RIGHTALT: u16 = 100;

pub fn output_key(code: u16) -> Option<u16> {
    match code {
        KEY_POWER => Some(KEY_ESC),
        KEY_F5 | KEY_F9 => Some(KEY_F9),
        KEY_UP | KEY_DOWN | KEY_LEFT | KEY_RIGHT | KEY_ENTER => Some(code),
        KEY_BACK => Some(KEY_BACKSPACE),
        KEY_HOME => Some(KEY_F13),
        KEY_MENU => Some(KEY_F14),
        KEY_TV => Some(KEY_F15),
        KEY_VOLUME_UP | KEY_VOLUME_DOWN => Some(code),
        _ => None,
    }
}

fn output_key_codes() -> Vec<u16> {
    let mut codes = vec![
        KEY_ESC,
        KEY_F9,
        KEY_UP,
        KEY_DOWN,
        KEY_LEFT,
        KEY_RIGHT,
        KEY_ENTER,
        KEY_BACKSPACE,
        KEY_F13,
        KEY_F14,
        KEY_F15,
        KEY_RIGHTALT,
        KEY_VOLUME_UP,
        KEY_VOLUME_DOWN,
    ];
    codes.sort_unstable();
    codes.dedup();
    codes
}

#[derive(Debug)]
pub struct InputMapper {
    mapping_enabled: bool,
    voice_code: u16,
    voice_down: bool,
    voice_release_at: Option<Instant>,
    release_drain: Duration,
    pressed: BTreeSet<u16>,
}

impl Default for InputMapper {
    fn default() -> Self {
        Self {
            mapping_enabled: true,
            voice_code: KEY_F9,
            voice_down: false,
            voice_release_at: None,
            release_drain: Duration::ZERO,
            pressed: BTreeSet::new(),
        }
    }
}

impl InputMapper {
    #[cfg(test)]
    pub fn with_release_drain(release_drain: Duration) -> Self {
        Self {
            release_drain,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn with_voice_key(voice_code: u16, release_drain: Duration) -> Self {
        Self {
            voice_code,
            release_drain,
            ..Self::default()
        }
    }

    pub fn with_options(voice_code: u16, release_drain: Duration, mapping_enabled: bool) -> Self {
        Self {
            mapping_enabled,
            voice_code,
            release_drain,
            ..Self::default()
        }
    }

    pub fn pending_events(&mut self) -> Vec<InputEvent> {
        if self
            .voice_release_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.voice_release_at = None;
            if self.voice_down {
                self.voice_down = false;
                return vec![InputEvent::new(EventType::KEY.0, self.voice_code, 0)];
            }
        }
        Vec::new()
    }
    pub fn map(&mut self, event: InputEvent) -> Vec<InputEvent> {
        if event.event_type() != EventType::KEY {
            return Vec::new();
        }
        let code = event.code();
        let value = event.value();
        if code == KEY_F5 || code == KEY_F9 {
            return self.map_voice(value);
        }
        if !self.mapping_enabled {
            return Vec::new();
        }
        let Some(output) = output_key(code) else {
            return Vec::new();
        };
        match value {
            0 => {
                self.pressed.remove(&output);
            }
            1 => {
                self.pressed.insert(output);
            }
            2 => {
                if !matches!(
                    code,
                    KEY_UP
                        | KEY_DOWN
                        | KEY_LEFT
                        | KEY_RIGHT
                        | KEY_BACK
                        | KEY_VOLUME_UP
                        | KEY_VOLUME_DOWN
                ) {
                    return Vec::new();
                }
            }
            _ => return Vec::new(),
        }
        vec![InputEvent::new(EventType::KEY.0, output, value)]
    }

    fn map_voice(&mut self, value: i32) -> Vec<InputEvent> {
        match value {
            1 if self.voice_down && self.voice_release_at.is_some() => {
                self.voice_release_at = None;
                Vec::new()
            }
            1 if !self.voice_down => {
                self.voice_release_at = None;
                self.voice_down = true;
                vec![InputEvent::new(EventType::KEY.0, self.voice_code, 1)]
            }
            0 if self.voice_down && self.release_drain.is_zero() => {
                self.voice_down = false;
                vec![InputEvent::new(EventType::KEY.0, self.voice_code, 0)]
            }
            0 if self.voice_down => {
                self.voice_release_at = Some(Instant::now() + self.release_drain);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub fn release_all(&mut self) -> Vec<InputEvent> {
        self.voice_release_at = None;
        let mut events = Vec::new();
        if self.voice_down {
            self.voice_down = false;
            events.push(InputEvent::new(EventType::KEY.0, self.voice_code, 0));
        }
        for code in std::mem::take(&mut self.pressed) {
            events.push(InputEvent::new(EventType::KEY.0, code, 0));
        }
        events
    }
}

fn process_event_batch(
    mapper: &mut InputMapper,
    events: Result<Vec<InputEvent>, std::io::Error>,
) -> Result<Vec<InputEvent>> {
    let mut mapped = mapper.pending_events();
    match events {
        Ok(events) => {
            for event in events {
                if event.event_type() == EventType::KEY {
                    tracing::debug!(
                        code = event.code(),
                        value = event.value(),
                        "RC003 raw input event"
                    );
                }
                mapped.extend(mapper.map(event));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error.into()),
    }
    Ok(mapped)
}

struct ManagedDevice {
    device: Device,
    virtual_device: VirtualDevice,
    mapper: InputMapper,
    release_grab_on_exit: bool,
}

impl ManagedDevice {
    fn open(
        path: &Path,
        release_grab_on_exit: bool,
        release_drain: Duration,
        voice_code: u16,
        mapping_enabled: bool,
        address: Option<&str>,
    ) -> Result<Self> {
        let mut device = Device::open(path).with_context(|| format!("open {}", path.display()))?;
        let id = device.input_id();
        if id.vendor() != RC003_VENDOR_ID || id.product() != RC003_PRODUCT_ID {
            anyhow::bail!("not RC003");
        }
        if let Some(address) = address {
            if !address_matches(device.unique_name().unwrap_or_default(), address) {
                anyhow::bail!("not selected RC003");
            }
        }
        device.set_nonblocking(true)?;
        device
            .grab()
            .with_context(|| format!("grab {}", path.display()))?;

        let mut keys = AttributeSet::<KeyCode>::new();
        for code in output_key_codes() {
            keys.insert(KeyCode::new(code));
        }
        let virtual_device = VirtualDevice::builder()?
            .name("rc003-omarchy")
            .with_keys(&keys)?
            .build()
            .context("create RC003 uinput device")?;
        Ok(Self {
            device,
            virtual_device,
            mapper: InputMapper::with_options(voice_code, release_drain, mapping_enabled),
            release_grab_on_exit,
        })
    }

    fn process_events(&mut self) -> Result<()> {
        let events = self.device.fetch_events().map(|events| events.collect());
        let mapped = process_event_batch(&mut self.mapper, events)?;
        if !mapped.is_empty() {
            tracing::debug!(?mapped, "RC003 mapped input events");
            self.virtual_device.emit(&mapped)?;
        }
        Ok(())
    }

    fn release(&mut self) {
        let events = self.mapper.release_all();
        if !events.is_empty() {
            let _ = self.virtual_device.emit(&events);
        }
        if self.release_grab_on_exit {
            let _ = self.device.ungrab();
        }
    }
}

impl Drop for ManagedDevice {
    fn drop(&mut self) {
        self.release();
    }
}

pub struct InputManagerHandle {
    shutdown_tx: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl InputManagerHandle {
    #[cfg(test)]
    pub fn start() -> Result<Self> {
        Self::start_with_release_grab(true, Duration::ZERO, KEY_F9, true, None)
    }

    pub fn start_with_release_grab(
        release_grab_on_exit: bool,
        release_drain: Duration,
        voice_code: u16,
        mapping_enabled: bool,
        address: Option<String>,
    ) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("rc003-input".to_string())
            .spawn(move || {
                run_input_manager(
                    shutdown_rx,
                    release_grab_on_exit,
                    release_drain,
                    voice_code,
                    mapping_enabled,
                    address,
                )
            })
            .context("start RC003 input thread")?;
        Ok(Self {
            shutdown_tx,
            join: Some(join),
        })
    }
}

impl Drop for InputManagerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
        if let Some(join) = self.join.take() {
            if let Err(error) = join.join() {
                tracing::error!(?error, "RC003 input thread panicked");
            }
        }
    }
}

fn run_input_manager(
    shutdown_rx: mpsc::Receiver<()>,
    release_grab_on_exit: bool,
    release_drain: Duration,
    voice_code: u16,
    mapping_enabled: bool,
    address: Option<String>,
) {
    let mut devices: HashMap<PathBuf, ManagedDevice> = HashMap::new();
    let mut ignored_nodes: HashMap<PathBuf, (u64, u64)> = HashMap::new();
    let mut next_scan = Instant::now();
    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }
        if Instant::now() >= next_scan {
            scan_devices(
                &mut devices,
                release_grab_on_exit,
                release_drain,
                voice_code,
                mapping_enabled,
                &mut ignored_nodes,
                address.as_deref(),
            );
            next_scan = Instant::now() + INPUT_SCAN_INTERVAL;
        }
        devices.retain(|path, device| {
            if !path.exists() {
                return false;
            }
            match device.process_events() {
                Ok(()) => true,
                Err(error)
                    if error
                        .root_cause()
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
                {
                    true
                }
                Err(error) => {
                    tracing::debug!(path = %path.display(), %error, "RC003 input node closed");
                    false
                }
            }
        });
        std::thread::sleep(Duration::from_millis(5));
    }
    for (_, mut device) in devices {
        device.release();
    }
    tracing::info!("RC003 input manager stopped");
}

fn scan_devices(
    devices: &mut HashMap<PathBuf, ManagedDevice>,
    release_grab_on_exit: bool,
    release_drain: Duration,
    voice_code: u16,
    mapping_enabled: bool,
    ignored_nodes: &mut HashMap<PathBuf, (u64, u64)>,
    address: Option<&str>,
) {
    ignored_nodes.retain(|path, identity| node_identity(path) == Some(*identity));
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"))
            || devices.contains_key(&path)
            || ignored_nodes
                .get(&path)
                .is_some_and(|cached| node_identity(&path).as_ref() == Some(cached))
        {
            continue;
        }
        match ManagedDevice::open(
            &path,
            release_grab_on_exit,
            release_drain,
            voice_code,
            mapping_enabled,
            address,
        ) {
            Ok(device) => {
                tracing::info!(path = %path.display(), "RC003 input ready");
                devices.insert(path, device);
            }
            Err(error) if error.to_string() == "not RC003" => {
                if let Some(identity) = node_identity(&path) {
                    ignored_nodes.insert(path, identity);
                }
            }
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "cannot open RC003 input node");
            }
        }
    }
}

pub fn check_rc003_access() -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir("/dev/input")? {
        let path = entry?.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"))
        {
            continue;
        }
        let Ok(device) = Device::open(&path) else {
            continue;
        };
        let id = device.input_id();
        if id.vendor() == RC003_VENDOR_ID && id.product() == RC003_PRODUCT_ID {
            paths.push(path.display().to_string());
        }
    }
    Ok(paths)
}

pub fn probe_keys(duration: Duration) -> Result<()> {
    let path = check_rc003_access()?
        .into_iter()
        .next()
        .context("no accessible RC003 event node")?;
    let mut device = Device::open(&path)?;
    device.set_nonblocking(true)?;
    device.grab()?;
    println!(
        "Capturing RC003 for {}s; no actions will be emitted",
        duration.as_secs()
    );
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    if event.event_type() == EventType::KEY {
                        println!("code={} value={}", event.code(), event.value());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error.into()),
        }
    }
    device.ungrab()?;
    Ok(())
}

pub fn voice_key_code(name: &str) -> Result<u16> {
    match name {
        "F9" => Ok(KEY_F9),
        "RightAlt" => Ok(KEY_RIGHTALT),
        _ => anyhow::bail!("unsupported voice key: {name}"),
    }
}

fn node_identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

fn address_matches(unique: &str, address: &str) -> bool {
    unique.eq_ignore_ascii_case(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event(code: u16, value: i32) -> InputEvent {
        InputEvent::new(EventType::KEY.0, code, value)
    }

    #[test]
    fn selected_address_matches_are_case_insensitive() {
        assert!(address_matches("c0:5d:39:fa:2a:1b", "C0:5D:39:FA:2A:1B"));
        assert!(!address_matches("aa:bb:cc:dd:ee:ff", "C0:5D:39:FA:2A:1B"));
    }

    #[test]
    fn rc003_all_13_keys_have_output_mappings() {
        let codes = [
            KEY_POWER,
            KEY_F5,
            KEY_UP,
            KEY_DOWN,
            KEY_LEFT,
            KEY_RIGHT,
            KEY_ENTER,
            KEY_BACK,
            KEY_HOME,
            KEY_MENU,
            KEY_TV,
            KEY_VOLUME_UP,
            KEY_VOLUME_DOWN,
        ];
        assert_eq!(codes.len(), 13);
        for code in codes {
            assert!(output_key(code).is_some(), "unmapped RC003 key {code}");
        }
        assert_eq!(output_key_codes().len(), 14);
    }

    #[test]
    fn voice_key_preserves_hold_and_release() {
        let mut mapper = InputMapper::default();
        assert_eq!(mapper.map(key_event(KEY_F5, 1))[0].code(), KEY_F9);
        assert!(mapper.map(key_event(KEY_F5, 2)).is_empty());
        assert!(mapper.map(key_event(KEY_F5, 1)).is_empty());
        let events = mapper.map(key_event(KEY_F5, 0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), KEY_F9);
        assert_eq!(events[0].value(), 0);
        assert!(mapper.map(key_event(KEY_F5, 0)).is_empty());
    }

    #[test]
    fn voice_key_can_use_right_alt() {
        let mut mapper = InputMapper::with_voice_key(KEY_RIGHTALT, Duration::ZERO);
        assert_eq!(mapper.map(key_event(KEY_F5, 1))[0].code(), KEY_RIGHTALT);
        assert_eq!(mapper.map(key_event(KEY_F5, 0))[0].code(), KEY_RIGHTALT);
    }

    #[test]
    fn mapping_disabled_keeps_voice_but_disables_other_keys() {
        let mut mapper = InputMapper::with_options(KEY_F9, Duration::ZERO, false);
        assert_eq!(mapper.map(key_event(KEY_F5, 1))[0].code(), KEY_F9);
        assert!(mapper.map(key_event(KEY_UP, 1)).is_empty());
    }

    #[test]
    fn ordinary_keys_preserve_down_up_and_allowed_repeat() {
        let mut mapper = InputMapper::default();
        assert_eq!(mapper.map(key_event(KEY_UP, 1))[0].code(), KEY_UP);
        assert_eq!(mapper.map(key_event(KEY_UP, 2))[0].value(), 2);
        assert_eq!(mapper.map(key_event(KEY_UP, 0))[0].value(), 0);
        assert_eq!(mapper.map(key_event(KEY_BACK, 1))[0].code(), KEY_BACKSPACE);
        assert!(mapper.map(key_event(KEY_MENU, 2)).is_empty());
    }

    #[test]
    fn release_all_clears_voice_and_pressed_keys_once() {
        let mut mapper = InputMapper::default();
        mapper.map(key_event(KEY_F5, 1));
        mapper.map(key_event(KEY_UP, 1));
        mapper.map(key_event(KEY_ENTER, 1));
        let events = mapper.release_all();
        assert_eq!(events.len(), 3);
        assert!(events
            .iter()
            .any(|event| event.code() == KEY_F9 && event.value() == 0));
        assert!(mapper.release_all().is_empty());
    }

    #[test]
    #[ignore = "requires /dev/uinput and an active input group"]
    fn uinput_end_to_end_voice_mapping() {
        use evdev::{BusType, InputId};
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug")
            .try_init();

        let mut keys = AttributeSet::<KeyCode>::new();
        for code in [
            KEY_POWER,
            KEY_F5,
            KEY_UP,
            KEY_DOWN,
            KEY_LEFT,
            KEY_RIGHT,
            KEY_ENTER,
            KEY_BACK,
            KEY_HOME,
            KEY_MENU,
            KEY_TV,
            KEY_VOLUME_UP,
            KEY_VOLUME_DOWN,
        ] {
            keys.insert(KeyCode::new(code));
        }
        let mut fake = VirtualDevice::builder()
            .unwrap()
            .name("rc003-test-source")
            .input_id(InputId::new(
                BusType::BUS_BLUETOOTH,
                RC003_VENDOR_ID,
                RC003_PRODUCT_ID,
                0x00a4,
            ))
            .with_keys(&keys)
            .unwrap()
            .build()
            .unwrap();
        let manager = InputManagerHandle::start().unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let output_path = virtual_event_path("rc003-omarchy").unwrap();
        println!("reading mapped output: {}", output_path.display());
        let mut output = Device::open(output_path).unwrap();
        output.set_nonblocking(true).unwrap();
        fake.emit(&[key_event(KEY_F5, 1)]).unwrap();
        fake.emit(&[key_event(KEY_F5, 0)]).unwrap();

        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            match output.fetch_events() {
                Ok(events) => {
                    for event in events {
                        if event.event_type() == EventType::KEY {
                            seen.push((event.code(), event.value()));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("mapped event read failed: {error}"),
            }
        }
        drop(manager);
        assert!(seen.contains(&(KEY_F9, 1)), "missing F9 down: {seen:?}");
        assert!(seen.contains(&(KEY_F9, 0)), "missing F9 up: {seen:?}");
    }

    fn virtual_event_path(name: &str) -> Option<PathBuf> {
        let content = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
        let mut found = None;
        for block in content.split("\n\n") {
            if !block.contains(&format!("N: Name=\"{name}\"")) {
                continue;
            }
            for line in block.lines() {
                if let Some(handlers) = line.strip_prefix("H: Handlers=") {
                    for handler in handlers.split_whitespace() {
                        if handler.starts_with("event") {
                            found = Some(PathBuf::from("/dev/input").join(handler));
                        }
                    }
                }
            }
        }
        found
    }

    #[test]
    fn wouldblock_event_source_still_releases_after_deadline() {
        let mut mapper = InputMapper::with_release_drain(Duration::from_millis(20));
        mapper.map(key_event(KEY_F5, 1));
        mapper.map(key_event(KEY_F5, 0));
        std::thread::sleep(Duration::from_millis(30));
        let events = process_event_batch(
            &mut mapper,
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), KEY_F9);
        assert_eq!(events[0].value(), 0);
    }

    #[test]
    fn repress_cancels_pending_release() {
        let mut mapper = InputMapper::with_release_drain(Duration::from_millis(50));
        mapper.map(key_event(KEY_F5, 1));
        mapper.map(key_event(KEY_F5, 0));
        assert!(mapper.map(key_event(KEY_F5, 1)).is_empty());
        assert!(mapper.pending_events().is_empty());
        std::thread::sleep(Duration::from_millis(60));
        mapper.map(key_event(KEY_F5, 0));
        std::thread::sleep(Duration::from_millis(60));
        let events = mapper.pending_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].value(), 0);
    }

    #[test]
    fn voice_release_drain_delays_only_the_up_event() {
        let mut mapper = InputMapper::with_release_drain(Duration::from_millis(40));
        assert_eq!(mapper.map(key_event(KEY_F5, 1))[0].value(), 1);
        assert!(mapper.map(key_event(KEY_F5, 0)).is_empty());
        assert!(mapper.pending_events().is_empty());
        std::thread::sleep(Duration::from_millis(50));
        let events = mapper.pending_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), KEY_F9);
        assert_eq!(events[0].value(), 0);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut mapper = InputMapper::default();
        assert!(mapper.map(key_event(999, 1)).is_empty());
        assert!(mapper
            .map(InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0))
            .is_empty());
    }
}
