use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Serialize;

use crate::ble;
use crate::config::Config;

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Run the bridge (same as invoking without a subcommand).
    Run,
    /// Manage the systemd user service.
    Service {
        #[command(subcommand)]
        command: crate::service::ServiceCommand,
    },
    /// Discover an ATVV-compatible remote without starting the daemon.
    Scan,
    /// Run read-only environment checks.
    Doctor {
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Run only the audio bridge for PipeWire/Superwhisper audio tests.
    TestAudio,
    /// Run the full bridge for an interactive native dictation test.
    TestDictation,
    /// Capture raw RC003 keys without emitting desktop actions.
    TestKeys {
        /// Capture duration in seconds.
        #[arg(long, default_value_t = 45)]
        seconds: u64,
    },
}

pub async fn run(
    command: Command,
    config_path: Option<&Path>,
    config: &Config,
    adapter_override: Option<&str>,
    node_name_override: Option<&str>,
    device_override: Option<&str>,
) -> Result<()> {
    let explicit_device = device_override.filter(|s| !s.is_empty());
    let device_filter = explicit_device
        .or_else(|| (!config.device.address.is_empty()).then_some(config.device.address.as_str()));
    match command {
        Command::Run | Command::TestAudio | Command::TestDictation => Ok(()),
        Command::Service { command } => crate::service::run(command, config_path),
        Command::Scan => scan(adapter_override, explicit_device).await,
        Command::Doctor { json } => {
            doctor(
                json,
                config,
                adapter_override,
                node_name_override,
                device_filter,
            )
            .await
        }
        Command::TestKeys { seconds } => {
            if seconds == 0 {
                anyhow::bail!("--seconds must be greater than zero");
            }
            let device_filter = device_filter.map(ToString::to_string);
            tokio::task::spawn_blocking(move || {
                crate::input::probe_keys(Duration::from_secs(seconds), device_filter.as_deref())
            })
            .await
            .context("join key probe")?
        }
    }
}

async fn adapter(name: Option<&str>) -> Result<bluer::Adapter> {
    let session = bluer::Session::new().await?;
    match name {
        Some(name) => session.adapter(name).map_err(Into::into),
        None => session.default_adapter().await.map_err(Into::into),
    }
}

async fn scan(adapter_name: Option<&str>, device_filter: Option<&str>) -> Result<()> {
    let filter_addr = device_filter
        .map(|addr| {
            addr.parse::<bluer::Address>()
                .context("parse --device address")
        })
        .transpose()?;
    let adapter = adapter(adapter_name).await?;
    let device = tokio::time::timeout(
        Duration::from_secs(10),
        ble::find_atvv_device(&adapter, filter_addr, &[]),
    )
    .await
    .context("timed out discovering an ATVV remote")??;
    let name = device.name().await.ok().flatten().unwrap_or_default();
    println!("{name} ({})", device.address());
    Ok(())
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

impl Check {
    fn new(name: &'static str, status: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct HoldShortcutInfo {
    configured: String,
    system: Vec<String>,
}

fn pactl_has_source(output: &str, node_name: &str) -> bool {
    output
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(node_name))
}

fn normalize_key_name(name: &str) -> String {
    let stripped = name
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_ascii_lowercase();
    let stripped = stripped.strip_prefix("key_").unwrap_or(&stripped);
    stripped
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn shortcut_matches_voice_key(actual: &str, expected_voice_key: &str) -> bool {
    normalize_key_name(actual) == normalize_key_name(expected_voice_key)
}

fn extract_hold_from_json(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    for key in ["hold", "holdKey", "holdShortcut", "effectiveHold"] {
        if let Some(v) = obj.get(key) {
            if let Some(s) = v.as_str() {
                if !s.trim().is_empty() {
                    return Some(s.trim().to_string());
                }
            } else if let Some(inner) = v.as_object() {
                for subkey in ["key", "effectiveKey", "shortcut", "value"] {
                    if let Some(s) = inner.get(subkey).and_then(|v| v.as_str()) {
                        if !s.trim().is_empty() {
                            return Some(s.trim().to_string());
                        }
                    }
                }
            }
        }
    }
    if let Some(shortcuts) = obj.get("shortcuts") {
        return extract_hold_from_json(shortcuts);
    }
    None
}

fn extract_system_holds_from_json(value: &serde_json::Value) -> Vec<String> {
    value
        .get("system")
        .and_then(|system| system.get("hold"))
        .and_then(|hold| hold.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_superwhisper_hold_shortcut(stdout: &[u8]) -> Option<HoldShortcutInfo> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(stdout) {
        if let Some(configured) = extract_hold_from_json(&value) {
            return Some(HoldShortcutInfo {
                configured,
                system: extract_system_holds_from_json(&value),
            });
        }
    }
    let text = std::str::from_utf8(stdout).ok()?;
    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        let rest = if let Some(after) = lower.strip_prefix("hold") {
            let offset = trimmed.len() - after.len();
            &trimmed[offset..]
        } else {
            continue;
        };
        let Some(first) = rest.chars().next() else {
            continue;
        };
        if !matches!(first, ':' | '=' | ' ' | '\t') {
            continue;
        }
        let value = rest.trim_start_matches([':', '=', ' ', '\t']).trim();
        if let Some(token) = value.split_whitespace().next() {
            let cleaned = token.trim_matches(['"', '\'', ',']);
            if !cleaned.is_empty() {
                return Some(HoldShortcutInfo {
                    configured: cleaned.to_string(),
                    system: Vec::new(),
                });
            }
        }
    }
    None
}

async fn check_bluetooth(
    adapter_name: Option<&str>,
    configured_addr: Option<bluer::Address>,
) -> Check {
    match adapter(adapter_name).await {
        Ok(adapter) => {
            let addresses = match adapter.device_addresses().await {
                Ok(addresses) => addresses,
                Err(error) => return Check::new("bluetooth", "fail", error.to_string()),
            };
            let mut matched = None;
            let mut any_atvv = None;
            for address in addresses {
                let Ok(device) = adapter.device(address) else {
                    continue;
                };
                if let Ok(Some(uuids)) = device.uuids().await {
                    if uuids.contains(&ble::ATVV_SERVICE) {
                        let name = device.name().await.ok().flatten().unwrap_or_default();
                        if configured_addr == Some(address) {
                            matched = Some(name);
                            break;
                        }
                        if any_atvv.is_none() {
                            any_atvv = Some(name);
                        }
                    }
                }
            }
            if let Some(target) = configured_addr {
                match matched {
                    Some(name) => Check::new(
                        "bluetooth",
                        "pass",
                        format!("ATVV remote: {name} ({})", ble::redact_address(target)),
                    ),
                    None => Check::new(
                        "bluetooth",
                        "warn",
                        format!(
                            "configured ATVV remote ({}) not found on adapter",
                            ble::redact_address(target)
                        ),
                    ),
                }
            } else {
                match any_atvv {
                    Some(name) => Check::new("bluetooth", "pass", format!("ATVV remote: {name}")),
                    None => Check::new(
                        "bluetooth",
                        "warn",
                        "adapter ready; no paired ATVV remote found",
                    ),
                }
            }
        }
        Err(error) => Check::new("bluetooth", "fail", error.to_string()),
    }
}

fn check_pipewire(node_name: &str) -> Check {
    match std::process::Command::new("pactl")
        .args(["list", "short", "sources"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            if pactl_has_source(&text, node_name) {
                Check::new(
                    "pipewire",
                    "pass",
                    format!("PipeWire source {node_name:?} is present"),
                )
            } else {
                Check::new(
                    "pipewire",
                    "warn",
                    format!("PipeWire source {node_name:?} is not running"),
                )
            }
        }
        Ok(output) => Check::new(
            "pipewire",
            "fail",
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ),
        Err(error) => Check::new("pipewire", "fail", error.to_string()),
    }
}

fn check_superwhisper_audio(node_name: &str) -> Check {
    match std::process::Command::new("superwhisper")
        .args(["audio", "show"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let value: serde_json::Value = match serde_json::from_slice(&output.stdout) {
                Ok(value) => value,
                Err(error) => {
                    return Check::new(
                        "superwhisper",
                        "fail",
                        format!("failed to parse `superwhisper audio show`: {error}"),
                    );
                }
            };
            let effective = value
                .get("effectiveDevice")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            if effective == node_name {
                Check::new(
                    "superwhisper",
                    "pass",
                    format!("microphone {node_name:?} selected"),
                )
            } else {
                Check::new(
                    "superwhisper",
                    "warn",
                    format!("active input: {effective} (expected {node_name})"),
                )
            }
        }
        Ok(output) => Check::new(
            "superwhisper",
            "fail",
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ),
        Err(error) => Check::new("superwhisper", "fail", error.to_string()),
    }
}

fn evaluate_superwhisper_shortcut(parsed: Option<HoldShortcutInfo>, expected: &str) -> Check {
    match parsed {
        Some(info) if shortcut_matches_voice_key(&info.configured, expected) => {
            if !info.system.is_empty()
                && !info
                    .system
                    .iter()
                    .any(|key| shortcut_matches_voice_key(key, expected))
            {
                Check::new(
                    "shortcut",
                    "warn",
                    format!(
                        "Superwhisper hold shortcut is {expected}, but system bindings are {:?} \
                         (run: superwhisper shortcuts apply)",
                        info.system
                    ),
                )
            } else {
                Check::new(
                    "shortcut",
                    "pass",
                    format!("Superwhisper hold shortcut matches config ({expected})"),
                )
            }
        }
        Some(info) => Check::new(
            "shortcut",
            "warn",
            format!(
                "Superwhisper hold shortcut is {}, expected {expected} \
                 (run: superwhisper shortcuts set hold {expected} && superwhisper shortcuts apply)",
                info.configured
            ),
        ),
        None => Check::new(
            "shortcut",
            "warn",
            format!(
                "could not parse hold shortcut from `superwhisper shortcuts show` (expected {expected})"
            ),
        ),
    }
}

fn check_superwhisper_shortcut(config: &Config) -> Check {
    if !config.input.enabled {
        return Check::new(
            "shortcut",
            "pass",
            "RC003 input disabled in config; hold shortcut check skipped",
        );
    }
    let expected = &config.input.voice_key;
    match std::process::Command::new("superwhisper")
        .args(["shortcuts", "show"])
        .output()
    {
        Ok(output) if output.status.success() => evaluate_superwhisper_shortcut(
            parse_superwhisper_hold_shortcut(&output.stdout),
            expected,
        ),
        Ok(output) => Check::new(
            "shortcut",
            "warn",
            format!(
                "`superwhisper shortcuts show` unavailable: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ),
        Err(error) => Check::new("shortcut", "warn", error.to_string()),
    }
}

async fn doctor(
    json: bool,
    config: &Config,
    adapter_override: Option<&str>,
    node_name_override: Option<&str>,
    device_filter: Option<&str>,
) -> Result<()> {
    let node_name = node_name_override.unwrap_or(&config.audio.node_name);
    let configured_addr = device_filter
        .map(|addr| {
            addr.parse::<bluer::Address>()
                .context("parse configured device.address")
        })
        .transpose()?;

    let mut checks = Vec::new();
    checks.push(check_bluetooth(adapter_override, configured_addr).await);

    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/uinput")
    {
        Ok(_) => checks.push(Check::new("uinput", "pass", "/dev/uinput is writable")),
        Err(error) => checks.push(Check::new("uinput", "fail", error.to_string())),
    }

    match crate::input::check_rc003_access(device_filter) {
        Ok(paths) if !paths.is_empty() => {
            checks.push(Check::new("input", "pass", paths.join(", ")))
        }
        Ok(_) => checks.push(Check::new(
            "input",
            "warn",
            "no accessible RC003 event node",
        )),
        Err(error) => checks.push(Check::new("input", "fail", error.to_string())),
    }

    checks.push(check_pipewire(node_name));
    checks.push(check_superwhisper_audio(node_name));
    checks.push(check_superwhisper_shortcut(config));

    if json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        for check in &checks {
            println!("{:<13} {:<5} {}", check.name, check.status, check.detail);
        }
    }
    if checks.iter().any(|check| check.status == "fail") {
        anyhow::bail!("one or more environment checks failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pactl_source_matching_is_exact() {
        let sample = "42\trc003-omarchy-alt\tPipeWire\ts16le 1ch 16000Hz\tSUSPENDED\n\
                      43\trc003-omarchy\tPipeWire\ts16le 1ch 16000Hz\tRUNNING\n";
        assert!(pactl_has_source(sample, "rc003-omarchy"));
        assert!(pactl_has_source(sample, "rc003-omarchy-alt"));
        assert!(!pactl_has_source(sample, "rc003"));
    }

    #[test]
    fn superwhisper_hold_shortcut_parses_live_json_and_text() {
        let live_json = br#"{
            "shortcuts": { "hold": "RightAlt", "toggle": "Ctrl+Alt+Space" },
            "system": { "hold": ["RightAlt"] }
        }"#;
        let info = parse_superwhisper_hold_shortcut(live_json).unwrap();
        assert_eq!(info.configured, "RightAlt");
        assert_eq!(info.system, vec!["RightAlt"]);
        assert_eq!(
            evaluate_superwhisper_shortcut(Some(info), "RightAlt").status,
            "pass"
        );

        let unapplied = br#"{
            "shortcuts": { "hold": "F9" },
            "system": { "hold": ["RightAlt"] }
        }"#;
        let info = parse_superwhisper_hold_shortcut(unapplied).unwrap();
        let check = evaluate_superwhisper_shortcut(Some(info), "F9");
        assert_eq!(check.status, "warn");
        assert!(check.detail.contains("superwhisper shortcuts apply"));

        assert_eq!(
            parse_superwhisper_hold_shortcut(b"toggle: None\nhold: RightAlt\n")
                .map(|i| i.configured)
                .as_deref(),
            Some("RightAlt")
        );
        assert_eq!(
            parse_superwhisper_hold_shortcut(b"hold = F9\n")
                .map(|i| i.configured)
                .as_deref(),
            Some("F9")
        );
        assert_eq!(parse_superwhisper_hold_shortcut(b"unrelated output"), None);
    }

    #[test]
    fn shortcut_matching_normalizes_key_names() {
        assert!(shortcut_matches_voice_key("RightAlt", "RightAlt"));
        assert!(shortcut_matches_voice_key("right_alt", "RightAlt"));
        assert!(shortcut_matches_voice_key("KEY_RIGHTALT", "RightAlt"));
        assert!(shortcut_matches_voice_key("f9", "F9"));
        assert!(!shortcut_matches_voice_key("F9", "RightAlt"));
    }
}
