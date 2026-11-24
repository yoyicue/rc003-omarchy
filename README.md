# RC003 Omarchy

Xiaomi Bluetooth Remote 2 Pro (RC003) integration for Omarchy, focused on native Superwhisper push-to-talk dictation and complete device-specific key mapping.

## Features

- BlueZ ATVV discovery, capability negotiation, command writes, reconnects, and session handling.
- RC003-specific 16 kHz IMA/DVI ADPCM decoding, including 120-byte raw frames by default, explicitly configured 126-byte headered frames, and fragmented/coalesced notifications.
- Native PipeWire `Audio/Source` named `rc003-omarchy`, visible to Superwhisper without changing system defaults.
- Device-specific evdev/uinput handling for all 13 RC003 keys, with RightAlt/F9 down/up for native Superwhisper dictation and release cleanup on exit/disconnect.
- Strict TOML configuration, `scan`, `doctor`, `test-keys`, `test-audio`, and `test-dictation` commands.
- Preview-first systemd user service management with explicit `--apply`.

The audio bridge does not transcribe or inject text. Superwhisper owns the dictation interface, recognition, and insertion. The input mapper only handles RC003 and preserves hold/release semantics.

## Architecture

```text
RC003 -- BlueZ / ATVV --> ADPCM decoder --> PipeWire source --+
                                                              |
RC003 -- evdev ----------> device-specific mapper --> F9 -----+--> Superwhisper
```

The complete design and validation gates are in [docs/](docs/README.md).

## Install (Prebuilt Binary)

Prebuilt `x86_64` Linux archives are published on [GitHub Releases](https://github.com/yoyicue/rc003-omarchy/releases) (built on Ubuntu 24.04 / glibc 2.39, compatible with Omarchy/Arch and modern Linux distributions with PipeWire and BlueZ).

Runtime dependencies on Omarchy/Arch:

```bash
sudo pacman -S --needed pipewire bluez
```

Download, verify, and unpack the release archive:

```bash
VERSION=v0.1.0
ARCHIVE="rc003-omarchy-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
BASE_URL="https://github.com/yoyicue/rc003-omarchy/releases/download/${VERSION}"

curl -fLO "${BASE_URL}/${ARCHIVE}"
curl -fLO "${BASE_URL}/SHA256SUMS"
sha256sum -c SHA256SUMS

tar -xzf "${ARCHIVE}"
cd "rc003-omarchy-${VERSION}-x86_64-unknown-linux-gnu"
```

First-time system permissions (udev rule + `input` group):

```bash
sudo install -m 0644 udev/70-rc003-omarchy.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=input
sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput
sudo usermod -a -G input "$USER"
# Start a new login session after adding your user to the input group.
```

Optional configuration (skip to use built-in defaults with auto-discovery):

```bash
mkdir -p ~/.config/rc003-omarchy
cp config.example.toml ~/.config/rc003-omarchy/config.toml
```

Install the binary to `~/.local/bin/rc003-omarchy` and enable + start the systemd user service:

```bash
./rc003-omarchy service install --apply
# When upgrading an existing service, add --force:
# ./rc003-omarchy service install --apply --force
```

Or install the CLI binary only (without systemd):

```bash
install -Dm755 rc003-omarchy ~/.local/bin/rc003-omarchy
```

Verify the installation:

```bash
~/.local/bin/rc003-omarchy --version
~/.local/bin/rc003-omarchy service status
~/.local/bin/rc003-omarchy doctor
```

## Build

Requirements on Omarchy/Arch:

```bash
sudo pacman -S --needed clang pipewire bluez
mise use rust@latest
mise exec -- rustup component add rustfmt clippy
```

Build and verify:

```bash
mise exec -- cargo test
mise exec -- cargo clippy --all-targets -- -D warnings
mise exec -- cargo build --release
```

The uinput integration test requires access to `/dev/uinput` and an active `input` group:

```bash
sudo usermod -a -G input "$USER"
# Start a new login session after changing group membership.
mise exec -- cargo test -- --ignored uinput_end_to_end_voice_mapping
```

The included udev rule grants RC003 and uinput access through both `uaccess` and the `input` group. Its `70-` priority adds `uaccess` before systemd's `73-seat-late.rules` processes seat ACLs:

```bash
sudo install -m 0644 udev/70-rc003-omarchy.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=input
sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput
```

## Run

```bash
# Read-only checks
target/release/rc003-omarchy scan
target/release/rc003-omarchy doctor

# Raw key probe without desktop actions
target/release/rc003-omarchy test-keys --seconds 45

# Audio-only or full native dictation validation
target/release/rc003-omarchy test-audio
target/release/rc003-omarchy test-dictation --device AA:BB:CC:DD:EE:FF

# Normal daemon; all run options may follow the run subcommand
target/release/rc003-omarchy run --config ~/.config/rc003-omarchy/config.toml
```

Start with `config.example.toml` and copy it to `~/.config/rc003-omarchy/config.toml`. An empty device address auto-discovers the first ATVV remote.

Select the PipeWire microphone and native hold key in Superwhisper:

```bash
superwhisper audio select rc003-omarchy
superwhisper shortcuts set hold RightAlt
superwhisper shortcuts apply
```

Restore the system default input when the daemon is not running:

```bash
superwhisper audio select ""
```

## User service

```bash
# Preview is the default and does not write files
target/release/rc003-omarchy service show
target/release/rc003-omarchy service install

# Explicitly install and start
target/release/rc003-omarchy service install --apply

# Install without starting
target/release/rc003-omarchy service install --apply --no-start

target/release/rc003-omarchy service status
target/release/rc003-omarchy service uninstall --apply
```

The service uses `newgrp input -c` so the daemon starts with the required input group. The PipeWire virtual source, RC003 input manager (with its resident uinput virtual device), and optional D-Bus interface are created before BLE discovery and stay resident for the entire daemon lifetime (including while the remote is asleep at boot or across BLE re-discovery). PCM frames carry a session generation, and a bounded `ArrayQueue` drops the oldest frame when full. Every AUDIO_START advances the generation, while normal AUDIO_STOP preserves it long enough to drain the tail; every connection exit invalidates queued PCM. The PipeWire real-time callback exclusively owns its buffer. Startup succeeds only after the stream reaches Paused or Streaming. A watch channel retains the latest asynchronous failure and wakes every lifecycle wait, including Bluetooth reconnects; recording failures use the same MIC_CLOSE cleanup path as normal shutdown. The input manager uses event-driven `poll()` + `/dev/input` inotify for zero idle CPU wakeups, grabs fixed-device remotes immediately on wake without waiting for BLE GATT discovery, and updates its target dynamically in auto-discovery mode. The executable is installed to `~/.local/bin/rc003-omarchy` instead of a `target/` path.

## Validation status

- Previous Redmi baseline: 140 unit tests plus the explicit uinput integration test; Clippy passed with warnings denied and the release build succeeded.
- Latest verification baseline: 206 unit tests pass on Redmi and Linux CI, plus the explicit uinput integration test; Clippy (default and `--features dbus`, warnings denied) and the release build pass. Default-mode consumer switching, intentional source teardown, and PipeWire restart recovery while Bluetooth is offline have also been verified on hardware. A fresh physical RC003-to-Superwhisper dictation was transcribed and inserted successfully.
- Real BlueZ negotiation passed with ATVV v1.0, 16 kHz, and 120-byte frames.
- Superwhisper enumerates `RC003 Remote Microphone` as 16 kHz mono S16LE.
- Virtual RC003 input is verified end to end as RightAlt/F9 down/up through evdev/uinput.
- Physical RC003-to-Superwhisper dictation passed with RightAlt: three real recordings were transcribed and inserted by Superwhisper.
- The systemd user service is installed and enabled with `newgrp input -c`; startup, input-group, and Bluetooth wake/reconnect acceptance have all passed.
- Post-review Redmi checks cover SIGTERM cleanup, forced PipeWire startup failure, BlueZ busy classification, `--device` evdev matching, and on-demand recording with a persistent consumer across Bluetooth reconnect.

## References

- `b0o/ATVVoice` (MIT): BlueZ ATVV and native PipeWire implementation.
- `goodtiger/mi-remote-linux` (MIT): RC003 key codes, hardware validation, and mapping behavior.
- `zizhen01/rctool` (GPL-3.0-only): behavior reference only; no source is copied.

See `THIRD_PARTY_NOTICES.md` for attribution.
