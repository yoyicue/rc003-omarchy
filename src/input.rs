use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode};

pub const RC003_VENDOR_ID: u16 = 0x2717;
pub const RC003_PRODUCT_ID: u16 = 0x32b8;
const INPUT_SCAN_INTERVAL: Duration = Duration::from_secs(2);
const UINPUT_EXIT_DRAIN: Duration = Duration::from_millis(10);
const CMD_SHUTDOWN: u8 = b'S';
const CMD_RELOAD_TARGET: u8 = b'R';

const KEY_POWER: u16 = 116;
const KEY_F5: u16 = 63;
const KEY_F9: u16 = 67;
const KEY_UP: u16 = 103;
const KEY_DOWN: u16 = 108;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_PAGEUP: u16 = 104;
const KEY_PAGEDOWN: u16 = 109;
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

fn is_repeatable_output_key(output: u16) -> bool {
    matches!(
        output,
        KEY_UP
            | KEY_DOWN
            | KEY_LEFT
            | KEY_RIGHT
            | KEY_BACKSPACE
            | KEY_VOLUME_UP
            | KEY_VOLUME_DOWN
            | KEY_PAGEUP
            | KEY_PAGEDOWN
    )
}

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

fn output_key_codes_for(overrides: &HashMap<u16, Option<u16>>) -> Vec<u16> {
    let mut codes = output_key_codes();
    for target in overrides.values().flatten() {
        codes.push(*target);
    }
    codes.sort_unstable();
    codes.dedup();
    codes
}

fn parse_source_key(name: &str) -> Result<u16> {
    match name.to_ascii_lowercase().as_str() {
        "power" => Ok(KEY_POWER),
        "up" => Ok(KEY_UP),
        "down" => Ok(KEY_DOWN),
        "left" => Ok(KEY_LEFT),
        "right" => Ok(KEY_RIGHT),
        "enter" | "ok" => Ok(KEY_ENTER),
        "back" => Ok(KEY_BACK),
        "home" => Ok(KEY_HOME),
        "menu" => Ok(KEY_MENU),
        "tv" => Ok(KEY_TV),
        "volume_up" => Ok(KEY_VOLUME_UP),
        "volume_down" => Ok(KEY_VOLUME_DOWN),
        "voice" | "f5" | "f9" => {
            anyhow::bail!(
                "use input.voice_key to configure the RC003 voice key instead of mapping.keys.{name}"
            )
        }
        _ => anyhow::bail!("unsupported RC003 key name in mapping.keys: {name:?}"),
    }
}

fn parse_target_key(name: &str) -> Result<Option<u16>> {
    let key = match name {
        "None" => return Ok(None),
        "Esc" => KeyCode::KEY_ESC.code(),
        "Backspace" => KeyCode::KEY_BACKSPACE.code(),
        "Tab" => KeyCode::KEY_TAB.code(),
        "Enter" => KeyCode::KEY_ENTER.code(),
        "Space" => KeyCode::KEY_SPACE.code(),
        "Up" => KeyCode::KEY_UP.code(),
        "Down" => KeyCode::KEY_DOWN.code(),
        "Left" => KeyCode::KEY_LEFT.code(),
        "Right" => KeyCode::KEY_RIGHT.code(),
        "Home" => KeyCode::KEY_HOME.code(),
        "End" => KeyCode::KEY_END.code(),
        "PageUp" => KeyCode::KEY_PAGEUP.code(),
        "PageDown" => KeyCode::KEY_PAGEDOWN.code(),
        "Mute" => KeyCode::KEY_MUTE.code(),
        "VolumeDown" => KeyCode::KEY_VOLUMEDOWN.code(),
        "VolumeUp" => KeyCode::KEY_VOLUMEUP.code(),
        "PlayPause" => KeyCode::KEY_PLAYPAUSE.code(),
        "NextSong" => KeyCode::KEY_NEXTSONG.code(),
        "PreviousSong" => KeyCode::KEY_PREVIOUSSONG.code(),
        "F1" => KeyCode::KEY_F1.code(),
        "F2" => KeyCode::KEY_F2.code(),
        "F3" => KeyCode::KEY_F3.code(),
        "F4" => KeyCode::KEY_F4.code(),
        "F5" => KeyCode::KEY_F5.code(),
        "F6" => KeyCode::KEY_F6.code(),
        "F7" => KeyCode::KEY_F7.code(),
        "F8" => KeyCode::KEY_F8.code(),
        "F9" => KeyCode::KEY_F9.code(),
        "F10" => KeyCode::KEY_F10.code(),
        "F11" => KeyCode::KEY_F11.code(),
        "F12" => KeyCode::KEY_F12.code(),
        "F13" => KeyCode::KEY_F13.code(),
        "F14" => KeyCode::KEY_F14.code(),
        "F15" => KeyCode::KEY_F15.code(),
        "F16" => KeyCode::KEY_F16.code(),
        "F17" => KeyCode::KEY_F17.code(),
        "F18" => KeyCode::KEY_F18.code(),
        "F19" => KeyCode::KEY_F19.code(),
        "F20" => KeyCode::KEY_F20.code(),
        "F21" => KeyCode::KEY_F21.code(),
        "F22" => KeyCode::KEY_F22.code(),
        "F23" => KeyCode::KEY_F23.code(),
        "F24" => KeyCode::KEY_F24.code(),
        _ => anyhow::bail!("unsupported output key in mapping.keys: {name:?}"),
    };
    Ok(Some(key))
}

pub fn parse_mapping_overrides(
    entries: &BTreeMap<String, String>,
) -> Result<HashMap<u16, Option<u16>>> {
    let mut overrides = HashMap::with_capacity(entries.len());
    for (src_name, dst_name) in entries {
        let src = parse_source_key(src_name)?;
        let dst = parse_target_key(dst_name)?;
        if overrides.insert(src, dst).is_some() {
            anyhow::bail!("duplicate mapping.keys entry for key {src_name:?}");
        }
    }
    Ok(overrides)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddress {
    /// Accept any RC003 event node.
    #[cfg(test)]
    Any,
    /// Do not grab any RC003 event node until a device lock is acquired.
    None,
    /// Only grab RC003 event nodes matching this Bluetooth MAC address.
    Exact(String),
}

impl TargetAddress {
    fn matches(&self, unique_name: &str) -> bool {
        match self {
            #[cfg(test)]
            Self::Any => true,
            Self::None => false,
            Self::Exact(address) => address_matches(unique_name, address),
        }
    }
}

#[derive(Debug, Clone)]
pub struct InputOptions {
    pub release_grab_on_exit: bool,
    pub release_drain: Duration,
    pub voice_code: u16,
    pub mapping_enabled: bool,
    pub target_address: TargetAddress,
    pub key_overrides: HashMap<u16, Option<u16>>,
}

#[derive(Debug)]
pub struct InputMapper {
    mapping_enabled: bool,
    voice_code: u16,
    voice_down: bool,
    voice_release_at: Option<Instant>,
    release_drain: Duration,
    pressed: BTreeSet<u16>,
    key_overrides: HashMap<u16, Option<u16>>,
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
            key_overrides: HashMap::new(),
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

    pub fn with_options(
        voice_code: u16,
        release_drain: Duration,
        mapping_enabled: bool,
        key_overrides: HashMap<u16, Option<u16>>,
    ) -> Self {
        Self {
            mapping_enabled,
            voice_code,
            release_drain,
            key_overrides,
            ..Self::default()
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.voice_release_at
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

    fn resolved_output_key(&self, code: u16) -> Option<u16> {
        match self.key_overrides.get(&code) {
            Some(custom) => *custom,
            None => output_key(code),
        }
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
        let Some(output) = self.resolved_output_key(code) else {
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
                if !is_repeatable_output_key(output) {
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

fn create_virtual_device(key_overrides: &HashMap<u16, Option<u16>>) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in output_key_codes_for(key_overrides) {
        keys.insert(KeyCode::new(code));
    }
    VirtualDevice::builder()?
        .name("rc003-omarchy")
        .with_keys(&keys)?
        .build()
        .context("create RC003 uinput device")
}

/// Emit each key event with its own SYN_REPORT so fast down+up pairs in the
/// same read batch are never collapsed into a single input frame.
fn emit_events(virtual_device: &mut VirtualDevice, events: &[InputEvent]) -> std::io::Result<()> {
    for event in events {
        virtual_device.emit(&[*event])?;
    }
    Ok(())
}

enum OpenNodeError {
    NotRc003,
    NotSelectedRc003,
    OpenFailed(anyhow::Error),
    GrabFailed(anyhow::Error),
}

enum DeviceProcessError {
    InputClosed(anyhow::Error),
    EmitFailed(std::io::Error),
}

struct ManagedDevice {
    device: Device,
    unique_name: String,
    mapper: InputMapper,
    release_grab_on_exit: bool,
    grabbed: bool,
}

impl ManagedDevice {
    fn open(
        path: &Path,
        target: &TargetAddress,
        options: &InputOptions,
    ) -> std::result::Result<Self, OpenNodeError> {
        let mut device = Device::open(path)
            .with_context(|| format!("open {}", path.display()))
            .map_err(OpenNodeError::OpenFailed)?;
        let id = device.input_id();
        if id.vendor() != RC003_VENDOR_ID || id.product() != RC003_PRODUCT_ID {
            return Err(OpenNodeError::NotRc003);
        }
        let unique_name = device.unique_name().unwrap_or_default().to_string();
        if !target.matches(&unique_name) {
            return Err(OpenNodeError::NotSelectedRc003);
        }
        device
            .set_nonblocking(true)
            .with_context(|| format!("set nonblocking {}", path.display()))
            .map_err(OpenNodeError::OpenFailed)?;
        device
            .grab()
            .with_context(|| format!("grab {}", path.display()))
            .map_err(OpenNodeError::GrabFailed)?;

        Ok(Self {
            device,
            unique_name,
            mapper: InputMapper::with_options(
                options.voice_code,
                options.release_drain,
                options.mapping_enabled,
                options.key_overrides.clone(),
            ),
            release_grab_on_exit: options.release_grab_on_exit,
            grabbed: true,
        })
    }

    fn matches_target(&self, target: &TargetAddress) -> bool {
        target.matches(&self.unique_name)
    }

    fn as_raw_fd(&self) -> RawFd {
        self.device.as_raw_fd()
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.mapper.next_deadline()
    }

    fn process_events(
        &mut self,
        virtual_device: &mut VirtualDevice,
    ) -> std::result::Result<(), DeviceProcessError> {
        let events = self.device.fetch_events().map(|events| events.collect());
        let mapped = process_event_batch(&mut self.mapper, events)
            .map_err(DeviceProcessError::InputClosed)?;
        if !mapped.is_empty() {
            tracing::debug!(?mapped, "RC003 mapped input events");
            emit_events(virtual_device, &mapped).map_err(DeviceProcessError::EmitFailed)?;
        }
        Ok(())
    }

    fn flush_pending(&mut self, virtual_device: &mut VirtualDevice) -> std::io::Result<()> {
        let pending = self.mapper.pending_events();
        if !pending.is_empty() {
            tracing::debug!(?pending, "RC003 pending input events");
            emit_events(virtual_device, &pending)?;
        }
        Ok(())
    }

    fn release_keys(&mut self, virtual_device: &mut VirtualDevice) -> bool {
        let events = self.mapper.release_all();
        if events.is_empty() {
            return false;
        }
        if let Err(error) = emit_events(virtual_device, &events) {
            tracing::warn!(%error, "failed to emit RC003 release events");
        }
        true
    }

    fn ungrab_if_configured(&mut self) {
        if self.release_grab_on_exit && self.grabbed {
            let _ = self.device.ungrab();
            self.grabbed = false;
        }
    }
}

impl Drop for ManagedDevice {
    fn drop(&mut self) {
        self.ungrab_if_configured();
    }
}

fn create_control_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` points to a valid 2-element array for `pipe2`.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("create RC003 input control pipe");
    }
    // SAFETY: `pipe2` succeeded and returned two newly owned file descriptors.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read_fd, write_fd))
}

fn write_control_cmd(fd: RawFd, cmd: u8) {
    let byte = [cmd];
    // SAFETY: `fd` is a valid open non-blocking pipe write descriptor.
    let _ = unsafe { libc::write(fd, byte.as_ptr().cast(), byte.len()) };
}

fn create_input_inotify() -> Option<OwnedFd> {
    // SAFETY: `inotify_init1` accepts only flag bits.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "inotify unavailable for /dev/input; falling back to periodic scan"
        );
        return None;
    }
    // SAFETY: `fd` is a valid newly created inotify file descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mask = libc::IN_CREATE | libc::IN_ATTRIB | libc::IN_MOVED_TO | libc::IN_DELETE;
    // SAFETY: `owned` is valid and `c"/dev/input"` is a valid NUL-terminated path.
    let wd = unsafe { libc::inotify_add_watch(owned.as_raw_fd(), c"/dev/input".as_ptr(), mask) };
    if wd < 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "cannot watch /dev/input via inotify; falling back to periodic scan"
        );
        return None;
    }
    Some(owned)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ControlCommands {
    shutdown: bool,
    reload_target: bool,
}

fn read_control_commands(fd: RawFd, revents: libc::c_short) -> ControlCommands {
    let mut cmds = ControlCommands::default();
    if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        cmds.shutdown = true;
    }
    if revents & libc::POLLIN != 0 {
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: `fd` is open and `buf` is writable for `buf.len()` bytes.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                for &byte in &buf[..n as usize] {
                    match byte {
                        CMD_SHUTDOWN => cmds.shutdown = true,
                        CMD_RELOAD_TARGET => cmds.reload_target = true,
                        _ => {}
                    }
                }
                continue;
            }
            if n == 0 {
                cmds.shutdown = true;
                break;
            }
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
    }
    cmds
}

fn poll_control_commands(fd: RawFd) -> ControlCommands {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` points to a valid `libc::pollfd`.
    let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
    if rc > 0 && pfd.revents != 0 {
        read_control_commands(fd, pfd.revents)
    } else {
        ControlCommands::default()
    }
}

fn drain_fd(fd: RawFd) {
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: `fd` is open and `buf` is writable for `buf.len()` bytes.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            continue;
        }
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        break;
    }
}

fn compute_poll_timeout_ms(
    now: Instant,
    deadlines: impl IntoIterator<Item = Instant>,
) -> libc::c_int {
    let Some(earliest) = deadlines.into_iter().min() else {
        return -1;
    };
    if earliest <= now {
        return 0;
    }
    let remaining = earliest.saturating_duration_since(now);
    let millis = remaining.as_millis();
    let rounded = if millis == 0 {
        1
    } else if remaining.subsec_nanos() % 1_000_000 != 0 {
        millis.saturating_add(1)
    } else {
        millis
    };
    rounded.min(libc::c_int::MAX as u128) as libc::c_int
}

pub struct InputManagerHandle {
    control_tx: Option<OwnedFd>,
    target_address: Arc<Mutex<TargetAddress>>,
    join: Option<JoinHandle<()>>,
}

impl InputManagerHandle {
    #[cfg(test)]
    pub fn start() -> Result<Self> {
        Self::start_with_options(InputOptions {
            release_grab_on_exit: true,
            release_drain: Duration::ZERO,
            voice_code: KEY_F9,
            mapping_enabled: true,
            target_address: TargetAddress::Any,
            key_overrides: HashMap::new(),
        })
    }

    pub fn start_with_options(options: InputOptions) -> Result<Self> {
        let (control_rx, control_tx) = create_control_pipe()?;
        let target_address = Arc::new(Mutex::new(options.target_address.clone()));
        let thread_target = target_address.clone();
        let join = std::thread::Builder::new()
            .name("rc003-input".to_string())
            .spawn(move || run_input_manager(control_rx, thread_target, options))
            .context("start RC003 input thread")?;
        Ok(Self {
            control_tx: Some(control_tx),
            target_address,
            join: Some(join),
        })
    }

    pub fn set_target_address(&self, target: TargetAddress) {
        let changed = {
            let mut guard = self
                .target_address
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *guard == target {
                false
            } else {
                *guard = target;
                true
            }
        };
        if changed {
            if let Some(ref tx) = self.control_tx {
                write_control_cmd(tx.as_raw_fd(), CMD_RELOAD_TARGET);
            }
        }
    }
}

impl Drop for InputManagerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.control_tx.take() {
            write_control_cmd(tx.as_raw_fd(), CMD_SHUTDOWN);
            drop(tx);
        }
        if let Some(join) = self.join.take() {
            if let Err(error) = join.join() {
                tracing::error!(?error, "RC003 input thread panicked");
            }
        }
    }
}

fn run_input_manager(
    control_rx: OwnedFd,
    target_address: Arc<Mutex<TargetAddress>>,
    options: InputOptions,
) {
    let mut virtual_device: Option<VirtualDevice> = None;
    let mut uinput_degraded = false;
    let mut next_uinput_retry = Instant::now();
    let mut inotify_fd = create_input_inotify();
    let mut devices: HashMap<PathBuf, ManagedDevice> = HashMap::new();
    let mut ignored_nodes: HashMap<PathBuf, (u64, u64)> = HashMap::new();
    let mut warned_nodes: HashMap<PathBuf, (u64, u64)> = HashMap::new();
    let mut current_target = target_address
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut target_reload_pending = false;
    let mut rescan_needed = true;
    let mut next_rescan_retry: Option<Instant> = None;

    loop {
        let ctrl = poll_control_commands(control_rx.as_raw_fd());
        if ctrl.shutdown {
            break;
        }
        target_reload_pending |= ctrl.reload_target;

        if target_reload_pending {
            target_reload_pending = false;
            current_target = target_address
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let stale_paths: Vec<PathBuf> = devices
                .iter()
                .filter(|(_, device)| !device.matches_target(&current_target))
                .map(|(path, _)| path.clone())
                .collect();
            for path in stale_paths {
                if let Some(mut device) = devices.remove(&path) {
                    if let Some(vdev) = virtual_device.as_mut() {
                        device.release_keys(vdev);
                    }
                    device.ungrab_if_configured();
                    tracing::info!(
                        path = %path.display(),
                        "Released RC003 input node after target change"
                    );
                }
            }
            ignored_nodes.clear();
            warned_nodes.clear();
            next_rescan_retry = None;
            rescan_needed = true;
        }

        let now = Instant::now();
        if virtual_device.is_none() && now >= next_uinput_retry {
            match create_virtual_device(&options.key_overrides) {
                Ok(vdev) => {
                    if uinput_degraded {
                        tracing::info!("RC003 uinput device recovered");
                        uinput_degraded = false;
                    } else {
                        tracing::debug!("RC003 uinput device ready");
                    }
                    virtual_device = Some(vdev);
                    rescan_needed = true;
                }
                Err(error) => {
                    if !uinput_degraded {
                        tracing::warn!(
                            %error,
                            "RC003 input degraded: cannot create /dev/uinput device; \
                             evdev nodes will not be grabbed until /dev/uinput is writable"
                        );
                        uinput_degraded = true;
                    }
                    next_uinput_retry = now + INPUT_SCAN_INTERVAL;
                }
            }
        }

        if virtual_device.is_some()
            && (rescan_needed || next_rescan_retry.is_some_and(|deadline| now >= deadline))
        {
            rescan_needed = false;
            let had_rc003_error = scan_devices(
                &mut devices,
                &current_target,
                &options,
                &mut ignored_nodes,
                &mut warned_nodes,
            );
            next_rescan_retry = if had_rc003_error
                || (inotify_fd.is_none() && !matches!(current_target, TargetAddress::None))
            {
                Some(Instant::now() + INPUT_SCAN_INTERVAL)
            } else {
                None
            };
        }

        if let Some(vdev) = virtual_device.as_mut() {
            for (path, device) in &mut devices {
                if let Err(error) = device.flush_pending(vdev) {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "failed to emit pending RC003 input events"
                    );
                }
            }
        }

        let mut pollfds = Vec::with_capacity(2 + devices.len());
        pollfds.push(libc::pollfd {
            fd: control_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        let inotify_idx = if let Some(ref fd) = inotify_fd {
            let idx = pollfds.len();
            pollfds.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            Some(idx)
        } else {
            None
        };
        let mut polled_paths = Vec::with_capacity(devices.len());
        for (path, device) in &devices {
            polled_paths.push(path.clone());
            pollfds.push(libc::pollfd {
                fd: device.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }

        let mut deadlines: Vec<Instant> = devices
            .values()
            .filter_map(ManagedDevice::next_deadline)
            .collect();
        if virtual_device.is_none() {
            deadlines.push(next_uinput_retry);
        }
        if let Some(retry) = next_rescan_retry {
            deadlines.push(retry);
        } else if inotify_fd.is_none()
            && virtual_device.is_some()
            && !matches!(current_target, TargetAddress::None)
        {
            deadlines.push(Instant::now() + INPUT_SCAN_INTERVAL);
        }

        let timeout_ms = compute_poll_timeout_ms(Instant::now(), deadlines);
        // SAFETY: `pollfds` is a valid contiguous buffer of `libc::pollfd`.
        let ready = unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            tracing::warn!(%error, "poll() failed in RC003 input manager");
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        if pollfds[0].revents != 0 {
            let ctrl = read_control_commands(control_rx.as_raw_fd(), pollfds[0].revents);
            if ctrl.shutdown {
                break;
            }
            if ctrl.reload_target {
                target_reload_pending = true;
            }
        }

        if let Some(idx) = inotify_idx {
            let revents = pollfds[idx].revents;
            if revents & libc::POLLIN != 0 {
                if let Some(ref fd) = inotify_fd {
                    drain_fd(fd.as_raw_fd());
                }
                rescan_needed = true;
            }
            if revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                tracing::warn!("inotify fd closed; falling back to periodic /dev/input scan");
                inotify_fd = None;
                rescan_needed = true;
            }
        }

        let device_offset = if inotify_idx.is_some() { 2 } else { 1 };
        let mut disconnected = Vec::new();
        if let Some(vdev) = virtual_device.as_mut() {
            for (i, path) in polled_paths.into_iter().enumerate() {
                let revents = pollfds[device_offset + i].revents;
                let mut closed = false;
                if revents & libc::POLLIN != 0 {
                    if let Some(device) = devices.get_mut(&path) {
                        match device.process_events(vdev) {
                            Ok(()) => {}
                            Err(DeviceProcessError::InputClosed(error)) => {
                                tracing::debug!(
                                    path = %path.display(),
                                    %error,
                                    "RC003 input node closed"
                                );
                                closed = true;
                            }
                            Err(DeviceProcessError::EmitFailed(error)) => {
                                tracing::warn!(
                                    path = %path.display(),
                                    %error,
                                    "failed to emit RC003 mapped events"
                                );
                            }
                        }
                    }
                }
                if !closed
                    && (revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
                        || (rescan_needed && !path.exists()))
                {
                    tracing::debug!(path = %path.display(), "RC003 input node disconnected");
                    closed = true;
                }
                if closed {
                    disconnected.push(path);
                }
            }
            for path in disconnected {
                if let Some(mut device) = devices.remove(&path) {
                    device.release_keys(vdev);
                    device.ungrab_if_configured();
                }
            }
        }
    }

    let mut released_any = false;
    for (_, mut device) in devices {
        if let Some(vdev) = virtual_device.as_mut() {
            released_any |= device.release_keys(vdev);
        }
        device.ungrab_if_configured();
    }
    if released_any && virtual_device.is_some() {
        // Give libinput/compositor a brief window to read final key-up events
        // before dropping VirtualDevice destroys the uinput node on exit.
        std::thread::sleep(UINPUT_EXIT_DRAIN);
    }
    tracing::info!("RC003 input manager stopped");
}

fn scan_devices(
    devices: &mut HashMap<PathBuf, ManagedDevice>,
    target: &TargetAddress,
    options: &InputOptions,
    ignored_nodes: &mut HashMap<PathBuf, (u64, u64)>,
    warned_nodes: &mut HashMap<PathBuf, (u64, u64)>,
) -> bool {
    if matches!(target, TargetAddress::None) {
        return false;
    }
    ignored_nodes.retain(|path, identity| node_identity(path) == Some(*identity));
    warned_nodes.retain(|path, identity| node_identity(path) == Some(*identity));
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return false;
    };
    let mut had_rc003_error = false;
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
        match ManagedDevice::open(&path, target, options) {
            Ok(device) => {
                warned_nodes.remove(&path);
                tracing::info!(path = %path.display(), "RC003 input ready");
                devices.insert(path, device);
            }
            Err(OpenNodeError::NotRc003 | OpenNodeError::NotSelectedRc003) => {
                if let Some(identity) = node_identity(&path) {
                    ignored_nodes.insert(path, identity);
                }
            }
            Err(OpenNodeError::OpenFailed(error)) => match sysfs_is_rc003(&path) {
                Some(false) => {
                    if let Some(identity) = node_identity(&path) {
                        ignored_nodes.insert(path, identity);
                    }
                }
                Some(true) => {
                    had_rc003_error = true;
                    log_node_warning_once(
                        warned_nodes,
                        &path,
                        &error,
                        "RC003 input degraded: cannot open RC003 event node",
                    );
                }
                None => {
                    tracing::debug!(path = %path.display(), %error, "cannot open input node");
                }
            },
            Err(OpenNodeError::GrabFailed(error)) => {
                had_rc003_error = true;
                log_node_warning_once(
                    warned_nodes,
                    &path,
                    &error,
                    "RC003 input degraded: cannot grab RC003 event node",
                );
            }
        }
    }
    had_rc003_error
}

fn log_node_warning_once(
    warned_nodes: &mut HashMap<PathBuf, (u64, u64)>,
    path: &Path,
    error: &anyhow::Error,
    message: &str,
) {
    let identity = node_identity(path);
    let already_warned = identity
        .as_ref()
        .is_some_and(|id| warned_nodes.get(path) == Some(id));
    if !already_warned {
        tracing::warn!(path = %path.display(), %error, "{message}");
        if let Some(id) = identity {
            warned_nodes.insert(path.to_path_buf(), id);
        }
    }
}

fn parse_sysfs_hex_u16(text: &str) -> Option<u16> {
    let trimmed = text.trim();
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    u16::from_str_radix(hex, 16).ok()
}

fn sysfs_is_rc003(path: &Path) -> Option<bool> {
    let name = path.file_name()?.to_str()?;
    let id_dir = Path::new("/sys/class/input").join(name).join("device/id");
    let vendor = parse_sysfs_hex_u16(&std::fs::read_to_string(id_dir.join("vendor")).ok()?)?;
    let product = parse_sysfs_hex_u16(&std::fs::read_to_string(id_dir.join("product")).ok()?)?;
    Some(vendor == RC003_VENDOR_ID && product == RC003_PRODUCT_ID)
}

pub fn check_rc003_access(address: Option<&str>) -> Result<Vec<String>> {
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
            if let Some(filter) = address {
                if !address_matches(device.unique_name().unwrap_or_default(), filter) {
                    continue;
                }
            }
            paths.push(path.display().to_string());
        }
    }
    Ok(paths)
}

pub fn probe_keys(duration: Duration, address: Option<&str>) -> Result<()> {
    let path = check_rc003_access(address)?
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
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let timeout_ms = compute_poll_timeout_ms(now, [deadline]);
        let mut pfd = libc::pollfd {
            fd: device.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` points to a valid `libc::pollfd`.
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if ready == 0 {
            continue;
        }
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    if event.event_type() == EventType::KEY {
                        println!("code={} value={}", event.code(), event.value());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
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
        let mut mapper = InputMapper::with_options(KEY_F9, Duration::ZERO, false, HashMap::new());
        assert_eq!(mapper.map(key_event(KEY_F5, 1))[0].code(), KEY_F9);
        assert!(mapper.map(key_event(KEY_UP, 1)).is_empty());
    }

    #[test]
    fn custom_mapping_overrides_work_and_support_disabling_keys() {
        let mut entries = BTreeMap::new();
        entries.insert("power".to_string(), "None".to_string());
        entries.insert("home".to_string(), "F16".to_string());
        entries.insert("tv".to_string(), "PlayPause".to_string());
        let overrides = parse_mapping_overrides(&entries).unwrap();

        let all_codes = output_key_codes_for(&overrides);
        assert!(all_codes.contains(&KeyCode::KEY_F16.code()));
        assert!(all_codes.contains(&KeyCode::KEY_PLAYPAUSE.code()));

        let mut mapper = InputMapper::with_options(KEY_RIGHTALT, Duration::ZERO, true, overrides);
        assert!(mapper.map(key_event(KEY_POWER, 1)).is_empty());
        assert_eq!(
            mapper.map(key_event(KEY_HOME, 1))[0].code(),
            KeyCode::KEY_F16.code()
        );
        assert_eq!(
            mapper.map(key_event(KEY_TV, 1))[0].code(),
            KeyCode::KEY_PLAYPAUSE.code()
        );
        assert_eq!(mapper.map(key_event(KEY_F5, 1))[0].code(), KEY_RIGHTALT);

        let released = mapper.release_all();
        assert_eq!(released.len(), 3);
        assert!(released
            .iter()
            .any(|ev| ev.code() == KeyCode::KEY_F16.code() && ev.value() == 0));
    }

    #[test]
    fn mapping_override_validation_rejects_voice_duplicates_and_unknowns() {
        let mut voice = BTreeMap::new();
        voice.insert("voice".to_string(), "F13".to_string());
        assert!(parse_mapping_overrides(&voice).is_err());

        let mut dup = BTreeMap::new();
        dup.insert("enter".to_string(), "Enter".to_string());
        dup.insert("ok".to_string(), "Space".to_string());
        assert!(parse_mapping_overrides(&dup).is_err());

        let mut bad_src = BTreeMap::new();
        bad_src.insert("unknown".to_string(), "F13".to_string());
        assert!(parse_mapping_overrides(&bad_src).is_err());

        let mut bad_dst = BTreeMap::new();
        bad_dst.insert("home".to_string(), "SuperLaser".to_string());
        assert!(parse_mapping_overrides(&bad_dst).is_err());
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
    fn repeat_follows_mapped_target_key_not_source_key() {
        let mut entries = BTreeMap::new();
        entries.insert("up".to_string(), "PlayPause".to_string());
        entries.insert("menu".to_string(), "PageDown".to_string());
        let overrides = parse_mapping_overrides(&entries).unwrap();
        let mut mapper = InputMapper::with_options(KEY_F9, Duration::ZERO, true, overrides);

        assert_eq!(
            mapper.map(key_event(KEY_UP, 1))[0].code(),
            KeyCode::KEY_PLAYPAUSE.code()
        );
        assert!(mapper.map(key_event(KEY_UP, 2)).is_empty());

        assert_eq!(mapper.map(key_event(KEY_MENU, 1))[0].code(), KEY_PAGEDOWN);
        let repeat = mapper.map(key_event(KEY_MENU, 2));
        assert_eq!(repeat.len(), 1);
        assert_eq!(repeat[0].code(), KEY_PAGEDOWN);
        assert_eq!(repeat[0].value(), 2);
    }

    #[test]
    fn target_address_matches_none_any_and_exact() {
        assert!(!TargetAddress::None.matches("c0:5d:39:fa:2a:1b"));
        assert!(TargetAddress::Any.matches("c0:5d:39:fa:2a:1b"));
        let exact = TargetAddress::Exact("C0:5D:39:FA:2A:1B".to_string());
        assert!(exact.matches("c0:5d:39:fa:2a:1b"));
        assert!(!exact.matches("aa:bb:cc:dd:ee:ff"));
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
    fn poll_timeout_blocks_indefinitely_when_idle_and_tracks_earliest_deadline() {
        let now = Instant::now();
        assert_eq!(compute_poll_timeout_ms(now, std::iter::empty()), -1);
        assert_eq!(compute_poll_timeout_ms(now, [now]), 0);
        assert_eq!(
            compute_poll_timeout_ms(
                now,
                [
                    now + Duration::from_millis(200),
                    now + Duration::from_millis(35)
                ]
            ),
            35
        );
        assert_eq!(
            compute_poll_timeout_ms(now, [now + Duration::from_micros(250)]),
            1
        );
    }

    #[test]
    fn sysfs_hex_parser_handles_plain_and_prefixed_ids() {
        assert_eq!(parse_sysfs_hex_u16("2717\n"), Some(RC003_VENDOR_ID));
        assert_eq!(parse_sysfs_hex_u16("0x32b8\n"), Some(RC003_PRODUCT_ID));
        assert_eq!(parse_sysfs_hex_u16("not-hex"), None);
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
        assert!(mapper.next_deadline().is_some());
        std::thread::sleep(Duration::from_millis(30));
        let events = process_event_batch(
            &mut mapper,
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code(), KEY_F9);
        assert_eq!(events[0].value(), 0);
        assert!(mapper.next_deadline().is_none());
    }

    #[test]
    fn repress_cancels_pending_release() {
        let mut mapper = InputMapper::with_release_drain(Duration::from_millis(50));
        mapper.map(key_event(KEY_F5, 1));
        mapper.map(key_event(KEY_F5, 0));
        assert!(mapper.next_deadline().is_some());
        assert!(mapper.map(key_event(KEY_F5, 1)).is_empty());
        assert!(mapper.next_deadline().is_none());
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
