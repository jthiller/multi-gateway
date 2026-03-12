use gateway_rs::{semtech_udp::MacAddress, Keypair};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};

pub(crate) static KEY_SUFFIX: &str = ".key";

/// Convert a MAC address to a key filename (without suffix)
/// e.g., AA:BB:CC:DD:EE:FF:00:11 -> AABBCCDDEEFF0011
pub fn mac_to_key_name(mac: &MacAddress) -> String {
    mac.as_bytes()
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect()
}

pub(crate) struct KeysDir(PathBuf);

use anyhow::{anyhow, bail, Result};

impl KeysDir {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self(path)
    }

    fn read_dir(&self) -> Result<fs::ReadDir> {
        Ok(match fs::read_dir(&self.0) {
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    fs::create_dir_all(&self.0).map_err(|e| {
                        anyhow!("failed to create keys directory {:?}: {e}", self.0)
                    })?;
                    fs::read_dir(&self.0)?
                } else {
                    bail!("failed to read keys directory {:?}: {e}", self.0)
                }
            }
            Ok(d) => d,
        })
    }

    pub fn load(&self) -> Result<Vec<(String, Keypair)>> {
        let dir = self.read_dir()?;
        let mut results = Vec::new();
        for entry in dir {
            let entry = entry?;
            let mut name = entry
                .file_name()
                .into_string()
                .map_err(|e| anyhow!("failed to convert os_string into string: {e:?}"))?;
            let suffix = name.split_off(name.len() - KEY_SUFFIX.len());
            if suffix == KEY_SUFFIX {
                let path = entry.path();
                let Some(path) = path.to_str() else {
                    continue;
                };
                if let Ok(keypair) = Keypair::load_from_file(path) {
                    keypair.public_key();
                    debug!("loaded keypair: {name}, {}", keypair.public_key());
                    results.push((name, keypair));
                }
            }
        }
        Ok(results)
    }

    /// Get an existing keypair for a MAC address, or auto-provision a new one.
    /// Keys are stored with filenames matching the MAC address (e.g., AABBCCDDEEFF0011.key)
    pub fn get_or_create(&self, mac: &MacAddress) -> Result<Keypair> {
        let name = mac_to_key_name(mac);
        let path = self.0.join(format!("{name}{KEY_SUFFIX}"));

        // Try to load existing key
        if path.exists() {
            let Some(path_str) = path.to_str() else {
                bail!("failed to convert path to string: {path:?}")
            };
            let keypair = Keypair::load_from_file(path_str)?;
            debug!(mac = %name, pubkey = %keypair.public_key(), "loaded existing keypair");
            return Ok(keypair);
        }

        // Auto-provision new key
        info!(mac = %name, "auto-provisioning new gateway keypair");
        let keypair = Keypair::new();
        let Some(path_str) = path.to_str() else {
            bail!("failed to convert path to string: {path:?}")
        };
        keypair.save_to_file(path_str)?;
        info!(mac = %name, pubkey = %keypair.public_key(), "created new keypair");
        Ok(keypair)
    }
}
