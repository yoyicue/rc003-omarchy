# RC003 Omarchy

Xiaomi Bluetooth Remote 2 Pro (RC003) integration for Omarchy, focused on native Superwhisper push-to-talk dictation and complete device-specific key mapping.

## Features

- BlueZ ATVV discovery, capability negotiation, command writes, reconnects, and session handling.
- RC003-specific 16 kHz IMA/DVI ADPCM decoding, including 120-byte raw frames, 126-byte headered frames, and fragmented/coalesced notifications.
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

The included udev rule grants RC003 and uinput access through both `uaccess` and the `input` group:

```bash
sudo install -m 0644 udev/99-rc003-omarchy.rules /etc/udev/rules.d/
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

The service uses `newgrp input -c` so the daemon starts with the required input group. PCM frames carry a session generation, and only frame metadata decides what to play or discard. A bounded `ArrayQueue` drops the oldest frame when full, preserving continuous real-time audio without a shared receiver mutex. Session generations are assigned in `run_session` when each frame is received. Every AUDIO_START advances the session generation so consecutive dictations cannot replay the previous utterance's tail. The executable is installed to `~/.local/bin/rc003-omarchy` instead of a `target/` path, and the PipeWire source stays alive across BLE reconnects. Real dictation has passed on Redmi; the service is installed and enabled.

## Validation status

- 125 unit tests pass, plus the explicit uinput integration test.
- Clippy passes with warnings denied; release build succeeds.
- Real BlueZ negotiation passed with ATVV v1.0, 16 kHz, and 120-byte frames.
- Superwhisper enumerates `RC003 Remote Microphone` as 16 kHz mono S16LE.
- Virtual RC003 input is verified end to end as RightAlt/F9 down/up through evdev/uinput.
- Physical RC003-to-Superwhisper dictation passed with RightAlt: three real recordings were transcribed and inserted by Superwhisper.
- The systemd user service is installed and enabled with `newgrp input -c`; startup, input-group, and Bluetooth wake/reconnect acceptance have all passed.

## References

- `b0o/ATVVoice` (MIT): BlueZ ATVV and native PipeWire implementation.
- `goodtiger/mi-remote-linux` (MIT): RC003 key codes, hardware validation, and mapping behavior.
- `zizhen01/rctool` (GPL-3.0-only): behavior reference only; no source is copied.

See `THIRD_PARTY_NOTICES.md` for attribution.
