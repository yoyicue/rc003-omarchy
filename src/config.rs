use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub device: DeviceConfig,
    pub audio: AudioConfig,
    pub input: InputConfig,
    pub mapping: MappingConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub address: String,
    pub auto_connect: bool,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    pub node_name: String,
    pub description: String,
    pub gain_db: f32,
    pub release_drain_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputConfig {
    pub enabled: bool,
    pub voice_key: String,
    pub release_grab_on_exit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingConfig {
    pub enabled: bool,
    pub profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            device: DeviceConfig {
                address: String::new(),
                auto_connect: true,
                reconnect_initial_ms: 1000,
                reconnect_max_ms: 15000,
            },
            audio: AudioConfig {
                node_name: "rc003-omarchy".to_string(),
                description: "RC003 Remote Microphone".to_string(),
                gain_db: 0.0,
                release_drain_ms: 0,
            },
            input: InputConfig {
                enabled: true,
                voice_key: "RightAlt".to_string(),
                release_grab_on_exit: true,
            },
            mapping: MappingConfig {
                enabled: true,
                profile: "default".to_string(),
            },
            logging: LoggingConfig {
                level: "info".to_string(),
            },
        }
    }
}

impl Config {
    pub fn default_path() -> PathBuf {
        if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(path).join("rc003-omarchy/config.toml");
        }
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".config/rc003-omarchy/config.toml")
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let explicit = path.is_some();
        let path = path.map(PathBuf::from).unwrap_or_else(Self::default_path);
        if explicit && !path.exists() {
            anyhow::bail!("explicit config file does not exist: {}", path.display());
        }
        let config = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("read config {}", path.display()))?;
            toml::from_str::<Self>(&text)
                .with_context(|| format!("parse config {}", path.display()))?
        } else {
            Self::default()
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            anyhow::bail!("unsupported config version {}", self.version);
        }
        if self.audio.node_name.is_empty() {
            anyhow::bail!("audio.node_name cannot be empty");
        }
        if !(-24.0..=24.0).contains(&self.audio.gain_db) {
            anyhow::bail!("audio.gain_db must be between -24 and 24");
        }
        if self.audio.release_drain_ms > 1000 {
            anyhow::bail!("audio.release_drain_ms must be <= 1000");
        }
        if self.device.reconnect_initial_ms == 0
            || self.device.reconnect_max_ms < self.device.reconnect_initial_ms
        {
            anyhow::bail!("invalid reconnect delays");
        }
        if !matches!(self.input.voice_key.as_str(), "F9" | "RightAlt") {
            anyhow::bail!("input.voice_key must be F9 or RightAlt");
        }
        if self.mapping.profile != "default" {
            anyhow::bail!("only the default mapping profile is currently supported");
        }
        if !matches!(
            self.logging.level.as_str(),
            "error" | "warn" | "info" | "debug" | "trace"
        ) {
            anyhow::bail!("invalid logging.level");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = toml::from_str::<Config>("unknown = true").unwrap_err();
        assert!(error.to_string().contains("unknown"));
    }

    #[test]
    fn invalid_gain_is_rejected() {
        let mut config = Config::default();
        config.audio.gain_db = 25.0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn explicit_missing_path_is_an_error() {
        let path = Path::new("/definitely/not/a/config.toml");
        assert!(Config::load(Some(path)).is_err());
    }

    #[test]
    fn documented_example_is_valid() {
        let text = r#"
version = 1
[device]
address = ""
auto_connect = true
reconnect_initial_ms = 1000
reconnect_max_ms = 15000
[audio]
node_name = "rc003-omarchy"
description = "RC003 Remote Microphone"
gain_db = 0.0
release_drain_ms = 0
[input]
enabled = true
voice_key = "F9"
release_grab_on_exit = true
[mapping]
enabled = true
profile = "default"
[logging]
level = "info"
"#;
        let config = toml::from_str::<Config>(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.audio.node_name, "rc003-omarchy");
    }
}
