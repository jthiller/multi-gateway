use http::Uri;
use serde::{Deserialize, Serialize};
use std::default::Default;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Serialize)]
pub struct Settings {
    #[serde(default)]
    pub gwmp: GwmpServerSettings,
    #[serde(default = "default_keys_dir")]
    pub keys_dir: PathBuf,
    pub router: RouterSettings,
    /// REST API listen address (e.g., "127.0.0.1:4468")
    #[serde(default = "default_api_addr")]
    pub api_addr: String,
    /// LoRaWAN region (e.g., "US915", "EU868", "AU915")
    /// Defaults to empty which uses the unknown region
    #[serde(default)]
    pub region: String,
    /// API key for read-only endpoints (list/get gateways, metrics).
    /// If set, clients must send `X-API-Key: <key>` header.
    #[serde(default)]
    pub read_api_key: Option<String>,
    /// API key for write endpoints (signing).
    /// If set, clients must send `X-API-Key: <key>` header.
    #[serde(default)]
    pub write_api_key: Option<String>,
    #[serde(default)]
    pub log: LogSettings,
}

fn default_api_addr() -> String {
    "127.0.0.1:4468".to_string()
}

/// Log configuration settings
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LogSettings {
    /// Log level (trace, debug, info, warn, error)
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Include timestamps in log output
    #[serde(default)]
    pub timestamp: bool,
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Settings for the packet router gRPC connection
#[derive(Debug, Deserialize, Serialize)]
pub struct RouterSettings {
    /// gRPC endpoint URI for the packet router
    #[serde(with = "http_serde::uri")]
    pub uri: Uri,
    /// Maximum number of packets to queue (default: 100)
    #[serde(default = "default_queue")]
    pub queue: u16,
}

fn default_queue() -> u16 {
    100
}

#[derive(Debug, Deserialize, Serialize)]
/// Settings for the GWMP Server
pub struct GwmpServerSettings {
    /// Address used for GWMP
    pub addr: String,
    /// Port used for GWMP
    pub port: u16,
}

fn default_keys_dir() -> PathBuf {
    PathBuf::from("/var/run/helium-multi-gateway/keys")
}

impl Default for GwmpServerSettings {
    fn default() -> Self {
        GwmpServerSettings {
            addr: "0.0.0.0".to_string(),
            port: 1680,
        }
    }
}

impl Settings {
    pub fn load<P: AsRef<std::path::Path>>(path: P) -> Result<Self, config::ConfigError> {
        let config_builder = config::Config::builder()
            .add_source(config::File::from(path.as_ref()))
            .build()?;
        config_builder.try_deserialize()
    }
}
