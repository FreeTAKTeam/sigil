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
    pub kodiak: Kodiak,
    pub receiver: Receiver,
}

/// Where the gateway itself is.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Receiver {
    /// Position of the SDR antenna. When set, the log shows each BSS
    /// packet's distance from the antenna. Events are unchanged.
    pub antenna: Option<Antenna>,
}

/// A WGS84 position in decimal degrees (south and west negative).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Antenna {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Kodiak {
    /// Spellings of the activation word as the recognizer writes them.
    pub wake_words: Vec<String>,
    /// Treat the BSS heading as a verified compass heading. BSS radios may
    /// report GPS course over ground instead, so this is off by default and
    /// clock-direction contact reports are then ignored.
    pub bss_heading_is_compass: bool,
    /// Oldest stored fix a contact marker may use when the transmission
    /// itself carried no position, in seconds.
    pub marker_max_fix_age_s: u64,
    /// Which intent classifier reads commands.
    pub classifier: Classifier,
    /// laya checkpoint directory, relative to the configuration file.
    pub laya_checkpoint: PathBuf,
    /// Minimum calibrated probability for laya to confirm or add an intent.
    pub laya_threshold: f32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Classifier {
    /// Rule-based vocabulary matching.
    #[default]
    Deterministic,
    /// laya typed decisions checked against the rules (`--features laya`).
    Laya,
}

impl Default for Kodiak {
    fn default() -> Self {
        Self {
            wake_words: kodiak_core::DEFAULT_WAKE_FORMS
                .iter()
                .map(|form| (*form).to_owned())
                .collect(),
            bss_heading_is_compass: false,
            marker_max_fix_age_s: 300,
            classifier: Classifier::Deterministic,
            laya_checkpoint: PathBuf::from("models/laya/english"),
            laya_threshold: 0.85,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Processing {
    pub parallel_voice_bss: bool,
    pub correlation_timeout_ms: u64,
    /// Longest gap between the end of speech and the trailing BSS packet.
    pub trailing_id_gap_ms: u64,
}

impl Default for Processing {
    fn default() -> Self {
        Self {
            parallel_voice_bss: true,
            correlation_timeout_ms: crate::correlation::Timing::DEFAULT_WINDOW_MS,
            trailing_id_gap_ms: crate::correlation::Timing::DEFAULT_TRAILING_GAP_MS,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Identity {
    pub auto_register: bool,
    pub registry_path: PathBuf,
    pub allowlist: BTreeSet<String>,
    /// Publish speech that carried no BSS ID from a dedicated gateway
    /// identity instead of keeping it local.
    pub unidentified_sender: bool,
    /// Display name announced for that identity.
    pub unidentified_name: String,
}

impl Identity {
    /// The display name to publish unidentified speech under, if enabled.
    pub fn unidentified(&self) -> Option<&str> {
        self.unidentified_sender
            .then_some(self.unidentified_name.as_str())
    }
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            auto_register: false,
            registry_path: "data/identities".into(),
            allowlist: BTreeSet::new(),
            unidentified_sender: false,
            unidentified_name: "Unidentified radio".into(),
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent publishing switches"
)]
pub struct Rch {
    pub enabled: bool,
    pub destination_hash: String,
    pub send_chat: bool,
    /// Start each chat message with the radio name, frequency and time.
    pub chat_header: bool,
    pub send_telemetry: bool,
    /// Publish Kodiak MECP reports, contact markers and RCH commands.
    pub send_intents: bool,
}

impl Default for Rch {
    fn default() -> Self {
        Self {
            enabled: false,
            destination_hash: String::new(),
            send_chat: true,
            chat_header: false,
            send_telemetry: true,
            send_intents: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&contents, path.parent().unwrap_or_else(|| Path::new(".")))
    }

    /// Reads and validates settings text. Relative paths in it are resolved
    /// against `directory`.
    pub fn parse(contents: &str, directory: &Path) -> Result<Self> {
        let mut config: Self = toml::from_str(contents).context("read gateway settings")?;
        ensure!(
            (200..=30_000).contains(&config.processing.correlation_timeout_ms),
            "correlation_timeout_ms must be 200–30000"
        );
        ensure!(
            config.processing.trailing_id_gap_ms <= config.processing.correlation_timeout_ms,
            "trailing_id_gap_ms must not exceed correlation_timeout_ms"
        );
        config.identity.allowlist = config
            .identity
            .allowlist
            .iter()
            .map(|label| {
                radio_id(label).with_context(|| format!("invalid BSS allowlist entry {label:?}"))
            })
            .collect::<Result<_>>()?;
        let name = config.identity.unidentified_name.trim();
        ensure!(
            !name.is_empty() && name.len() <= 64 && !name.contains(char::is_control),
            "unidentified_name must be 1-64 printable characters"
        );
        if let Some(Antenna {
            latitude,
            longitude,
        }) = config.receiver.antenna
        {
            ensure!(
                latitude.is_finite()
                    && longitude.is_finite()
                    && (-90.0..=90.0).contains(&latitude)
                    && (-180.0..=180.0).contains(&longitude),
                "[receiver] antenna must be latitude -90..90 and longitude -180..180"
            );
        }
        config.identity.registry_path = directory.join(&config.identity.registry_path);
        config.fabric.local_storage_path = directory.join(&config.fabric.local_storage_path);
        config.kodiak.laya_checkpoint = directory.join(&config.kodiak.laya_checkpoint);
        ensure!(
            config.kodiak.laya_threshold > 0.5 && config.kodiak.laya_threshold <= 1.0,
            "laya_threshold must be above 0.5 and at most 1"
        );
        if config.rch.enabled {
            ensure!(
                config.rch.send_chat || config.rch.send_telemetry || config.rch.send_intents,
                "enable chat, telemetry or intents in [rch]"
            );
            ensure!(
                !config.identity.allowlist.is_empty(),
                "[identity] allowlist is required with RCH enabled"
            );
            ensure!(
                is_destination_hash(&config.rch.destination_hash),
                "invalid RCH LXMF destination hash"
            );
            ensure!(
                !config.fabric.tcp_clients.is_empty(),
                "[fabric] tcp_clients is required with RCH enabled"
            );
            for endpoint in &config.fabric.tcp_clients {
                check_endpoint(endpoint)?;
            }
        }
        Ok(config)
    }
}

/// Whether `text` is an LXMF destination hash: 32 hexadecimal characters.
pub fn is_destination_hash(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Checks a Fabric TCP endpoint in `host:port` form.
pub fn check_endpoint(endpoint: &str) -> Result<()> {
    let (host, port) = endpoint
        .rsplit_once(':')
        .context("Fabric endpoint must be host:port")?;
    ensure!(
        !host.is_empty()
            && !host.contains(char::is_whitespace)
            && port.parse::<u16>().is_ok_and(|port| port > 0),
        "invalid Fabric endpoint"
    );
    Ok(())
}
