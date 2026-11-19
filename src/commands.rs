use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Serialize;

use crate::ble;

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
    Scan {
        /// BlueZ adapter name.
        #[arg(short, long)]
        adapter: Option<String>,
    },
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

pub async fn run(command: Command, config: Option<&Path>) -> Result<()> {
    match command {
        Command::Run | Command::TestAudio | Command::TestDictation => Ok(()),
        Command::Service { command } => crate::service::run(command, config),
        Command::Scan { adapter } => scan(adapter).await,
        Command::Doctor { json } => doctor(json).await,
        Command::TestKeys { seconds } => {
            if seconds == 0 {
                anyhow::bail!("--seconds must be greater than zero");
            }
            tokio::task::spawn_blocking(move || {
                crate::input::probe_keys(Duration::from_secs(seconds))
            })
            .await
            .context("join key probe")?
        }
    }
}

async fn adapter(name: Option<String>) -> Result<bluer::Adapter> {
    let session = bluer::Session::new().await?;
    match name {
        Some(name) => session.adapter(&name).map_err(Into::into),
        None => session.default_adapter().await.map_err(Into::into),
    }
}

async fn scan(adapter_name: Option<String>) -> Result<()> {
    let adapter = adapter(adapter_name).await?;
    let device = tokio::time::timeout(
        Duration::from_secs(10),
        ble::find_atvv_device(&adapter, None, &[]),
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

async fn doctor(json: bool) -> Result<()> {
    let mut checks = Vec::new();
    match adapter(None).await {
        Ok(adapter) => {
            let mut found = None;
            for address in adapter.device_addresses().await? {
                let device = adapter.device(address)?;
                if let Ok(Some(uuids)) = device.uuids().await {
                    if uuids.contains(&ble::ATVV_SERVICE) {
                        found = Some(device.name().await.ok().flatten().unwrap_or_default());
                        break;
                    }
                }
            }
            match found {
                Some(name) => checks.push(Check::new(
                    "bluetooth",
                    "pass",
                    format!("ATVV remote: {name}"),
                )),
                None => checks.push(Check::new(
                    "bluetooth",
                    "warn",
                    "adapter ready; no paired ATVV remote found",
                )),
            }
        }
        Err(error) => checks.push(Check::new("bluetooth", "fail", error.to_string())),
    }

    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/uinput")
    {
        Ok(_) => checks.push(Check::new("uinput", "pass", "/dev/uinput is writable")),
        Err(error) => checks.push(Check::new("uinput", "fail", error.to_string())),
    }

    match crate::input::check_rc003_access() {
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

    match std::process::Command::new("pactl")
        .args(["list", "short", "sources"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            if text.contains("rc003-omarchy") {
                checks.push(Check::new("pipewire", "pass", "RC003 source is present"));
            } else {
                checks.push(Check::new(
                    "pipewire",
                    "warn",
                    "RC003 source is not running",
                ));
            }
        }
        Ok(output) => checks.push(Check::new(
            "pipewire",
            "fail",
            String::from_utf8_lossy(&output.stderr),
        )),
        Err(error) => checks.push(Check::new("pipewire", "fail", error.to_string())),
    }

    match std::process::Command::new("superwhisper")
        .args(["audio", "show"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            let effective = value
                .get("effectiveDevice")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            if effective == "rc003-omarchy" {
                checks.push(Check::new(
                    "superwhisper",
                    "pass",
                    "RC003 microphone selected",
                ));
            } else {
                checks.push(Check::new(
                    "superwhisper",
                    "warn",
                    format!("active input: {effective}"),
                ));
            }
        }
        Ok(output) => checks.push(Check::new(
            "superwhisper",
            "fail",
            String::from_utf8_lossy(&output.stderr),
        )),
        Err(error) => checks.push(Check::new("superwhisper", "fail", error.to_string())),
    }

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
