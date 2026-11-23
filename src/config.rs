use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const MAX_TIMEOUT_SECS: u64 = 3600;

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
#[serde(default, deny_unknown_fields)]
pub struct DeviceConfig {
    pub address: String,
    pub auto_connect: bool,
    pub reconnect_initial_ms: u64,
    pub reconnect_max_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub node_name: String,
    pub description: String,
    pub gain_db: f32,
    pub release_drain_ms: u64,
    pub frame_layout: crate::protocol::types::AudioFrameLayout,
    pub frame_timeout_secs: u64,
    pub keep_alive_secs: u64,
    pub mic_on_demand: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    pub enabled: bool,
    pub voice_key: String,
    pub release_grab_on_exit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MappingConfig {
    pub enabled: bool,
    pub profile: String,
    pub keys: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            auto_connect: true,
            reconnect_initial_ms: 1000,
            reconnect_max_ms: 15000,
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            node_name: "rc003-omarchy".to_string(),
            description: "RC003 Remote Microphone".to_string(),
            gain_db: 0.0,
            release_drain_ms: 0,
            frame_layout: crate::protocol::types::AudioFrameLayout::Raw,
            frame_timeout_secs: 5,
            keep_alive_secs: 10,
            mic_on_demand: false,
        }
    }
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            voice_key: "RightAlt".to_string(),
            release_grab_on_exit: true,
        }
    }
}

impl Default for MappingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            profile: "default".to_string(),
            keys: BTreeMap::new(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            device: DeviceConfig::default(),
            audio: AudioConfig::default(),
            input: InputConfig::default(),
            mapping: MappingConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

pub fn is_valid_bluetooth_address(addr: &str) -> bool {
    let bytes = addr.as_bytes();
    if bytes.len() != 17 {
        return false;
    }
    for (idx, &b) in bytes.iter().enumerate() {
        if idx % 3 == 2 {
            if b != b':' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

pub fn is_valid_node_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
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
        if !self.device.address.is_empty() && !is_valid_bluetooth_address(&self.device.address) {
            anyhow::bail!(
                "device.address must be empty or a valid Bluetooth MAC address (AA:BB:CC:DD:EE:FF)"
            );
        }
        if !is_valid_node_name(&self.audio.node_name) {
            anyhow::bail!(
                "audio.node_name must use lowercase ASCII letters, digits, single hyphens, or underscores"
            );
        }
        if !(-24.0..=24.0).contains(&self.audio.gain_db) {
            anyhow::bail!("audio.gain_db must be between -24 and 24");
        }
        if self.audio.release_drain_ms > 1000 {
            anyhow::bail!("audio.release_drain_ms must be <= 1000");
        }
        if self.audio.frame_timeout_secs > MAX_TIMEOUT_SECS {
            anyhow::bail!("audio.frame_timeout_secs must be <= {MAX_TIMEOUT_SECS}");
        }
        if self.audio.keep_alive_secs > MAX_TIMEOUT_SECS {
            anyhow::bail!("audio.keep_alive_secs must be <= {MAX_TIMEOUT_SECS}");
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
        crate::input::parse_mapping_overrides(&self.mapping.keys)?;
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
    fn frame_layout_defaults_to_raw_and_accepts_headered() {
        assert_eq!(
            Config::default().audio.frame_layout,
            crate::protocol::types::AudioFrameLayout::Raw
        );
        let config = toml::from_str::<Config>(
            "[audio]\nnode_name = \"rc003\"\ndescription = \"\"\ngain_db = 0.0\nrelease_drain_ms = 0\nframe_layout = \"headered\"",
        )
        .unwrap();
        assert_eq!(
            config.audio.frame_layout,
            crate::protocol::types::AudioFrameLayout::Headered
        );
        let legacy = toml::from_str::<Config>(
            "[audio]\nnode_name = \"rc003\"\ndescription = \"\"\ngain_db = 0.0\nrelease_drain_ms = 0",
        )
        .unwrap();
        assert_eq!(
            legacy.audio.frame_layout,
            crate::protocol::types::AudioFrameLayout::Raw
        );
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
frame_layout = "raw"
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
        assert_eq!(config.audio.frame_timeout_secs, 5);
        assert_eq!(config.audio.keep_alive_secs, 10);
        assert!(!config.audio.mic_on_demand);
        assert!(config.mapping.keys.is_empty());
    }

    #[test]
    fn bluetooth_address_validation_checks_mac_format() {
        let mut config = Config::default();
        config.device.address = "AA:BB:CC:DD:EE:FF".to_string();
        assert!(config.validate().is_ok());

        config.device.address = "aa:bb:cc:dd:ee:01".to_string();
        assert!(config.validate().is_ok());

        for bad in [
            "not-a-mac",
            "AA:BB:CC:DD:EE",
            "AA-BB-CC-DD-EE-FF",
            "GG:BB:CC:DD:EE:FF",
            "AA:BB:CC:DD:EE:FF:00",
        ] {
            config.device.address = bad.to_string();
            assert!(config.validate().is_err(), "accepted invalid MAC {bad}");
        }
    }

    #[test]
    fn audio_session_options_and_key_overrides_validate() {
        let text = r#"
version = 1

[audio]
node_name = "rc003-desk"
frame_timeout_secs = 0
keep_alive_secs = 15
mic_on_demand = true

[mapping.keys]
power = "None"
home = "F16"
"#;
        let config = toml::from_str::<Config>(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.audio.node_name, "rc003-desk");
        assert_eq!(config.audio.frame_timeout_secs, 0);
        assert_eq!(config.audio.keep_alive_secs, 15);
        assert!(config.audio.mic_on_demand);
        assert_eq!(
            config.mapping.keys.get("power").map(String::as_str),
            Some("None")
        );
        assert_eq!(
            config.mapping.keys.get("home").map(String::as_str),
            Some("F16")
        );
    }

    #[test]
    fn invalid_node_name_and_excessive_timeouts_are_rejected() {
        let mut config = Config::default();
        config.audio.node_name = "RC003 Remote".to_string();
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.audio.frame_timeout_secs = MAX_TIMEOUT_SECS + 1;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.audio.keep_alive_secs = MAX_TIMEOUT_SECS + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn invalid_mapping_override_is_rejected() {
        let mut config = Config::default();
        config
            .mapping
            .keys
            .insert("voice".to_string(), "F13".to_string());
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config
            .mapping
            .keys
            .insert("home".to_string(), "NotARealKey".to_string());
        assert!(config.validate().is_err());
    }
}
