use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::correlation::radio_id;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub processing: Processing,
    pub identity: Identity,
    pub fabric: Fabric,
    pub rch: Rch,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Processing {
    pub parallel_voice_bss: bool,
    pub correlation_timeout_ms: u64,
}

impl Default for Processing {
    fn default() -> Self {
        Self {
            parallel_voice_bss: true,
            correlation_timeout_ms: 5_000,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Identity {
    pub auto_register: bool,
    pub registry_path: PathBuf,
    pub allowlist: BTreeSet<String>,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            auto_register: false,
            registry_path: "data/identities".into(),
            allowlist: BTreeSet::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fabric {
    pub tcp_clients: Vec<String>,
    pub local_storage_path: PathBuf,
}

impl Default for Fabric {
    fn default() -> Self {
        Self {
            tcp_clients: Vec::new(),
            local_storage_path: "data/fabric".into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rch {
    pub enabled: bool,
    pub destination_hash: String,
    pub send_chat: bool,
    pub send_telemetry: bool,
}

impl Default for Rch {
    fn default() -> Self {
        Self {
            enabled: false,
            destination_hash: String::new(),
            send_chat: true,
            send_telemetry: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut config: Self = toml::from_str(&contents).context("read gateway settings")?;
        ensure!(
            (200..=30_000).contains(&config.processing.correlation_timeout_ms),
            "correlation_timeout_ms must be 200–30000"
        );
        config.identity.allowlist = config
            .identity
            .allowlist
            .iter()
            .map(|label| {
                radio_id(label).with_context(|| format!("invalid BSS allowlist entry {label:?}"))
            })
            .collect::<Result<_>>()?;
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        config.identity.registry_path = directory.join(&config.identity.registry_path);
        config.fabric.local_storage_path = directory.join(&config.fabric.local_storage_path);
        if config.rch.enabled {
            ensure!(
                config.rch.send_chat || config.rch.send_telemetry,
                "enable chat or telemetry in [rch]"
            );
            ensure!(
                !config.identity.allowlist.is_empty(),
                "[identity] allowlist is required with RCH enabled"
            );
            ensure!(
                config.rch.destination_hash.len() == 32
                    && config
                        .rch
                        .destination_hash
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit()),
                "invalid RCH LXMF destination hash"
            );
            ensure!(
                !config.fabric.tcp_clients.is_empty(),
                "[fabric] tcp_clients is required with RCH enabled"
            );
            for endpoint in &config.fabric.tcp_clients {
                let (host, port) = endpoint
                    .rsplit_once(':')
                    .context("Fabric endpoint must be host:port")?;
                ensure!(
                    !host.is_empty()
                        && !host.contains(char::is_whitespace)
                        && port.parse::<u16>().is_ok_and(|port| port > 0),
                    "invalid Fabric endpoint"
                );
            }
        }
        Ok(config)
    }
}
