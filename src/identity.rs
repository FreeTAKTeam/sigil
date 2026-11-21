use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use chrono::{DateTime, Utc};
use rand_core::{OsRng, RngCore};
use rns_transport::destination::{DestinationName, SingleOutputDestination};
use rns_transport::hash::AddressHash;
use rns_transport::identity::PrivateIdentity;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

use crate::bss::Position;
use crate::config;
use crate::correlation::radio_id;

pub const PASSPHRASE_ENV: &str = "R3AKT_IDENTITY_PASSPHRASE";
const KEY_FORMAT: &[u8] = b"R3AKT-ID\x01";

pub struct Registry {
    root: PathBuf,
    cipher: XChaCha20Poly1305,
    allowed: BTreeSet<String>,
    auto_register: bool,
    _lock: File,
}

pub struct Registered {
    pub private_identity: PrivateIdentity,
    pub destination: AddressHash,
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    bss_id: String,
    display_name: String,
    bss_authenticated: bool,
    rns_identity_ref: String,
    lxmf_destination: String,
    registered_at: DateTime<Utc>,
    last_seen: DateTime<Utc>,
    last_position: Option<LastPosition>,
}

#[derive(Serialize, Deserialize)]
struct LastPosition {
    received_at: DateTime<Utc>,
    #[serde(flatten)]
    position: Position,
}

impl Registry {
    pub fn open(config: &config::Identity, passphrase: &str) -> Result<Self> {
        ensure!(
            passphrase.len() >= 16,
            "{PASSPHRASE_ENV} must contain at least 16 bytes"
        );
        let root = config.registry_path.clone();
        fs::create_dir_all(&root).context("create identity registry")?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("registry.lock"))?;
        lock.try_lock()
            .context("identity registry is already in use")?;
        let salt_path = root.join("salt.bin");
        let salt = match fs::read(&salt_path) {
            Ok(salt) => salt,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                for entry in root.read_dir()? {
                    let entry = entry?;
                    ensure!(
                        entry
                            .path()
                            .extension()
                            .is_none_or(|extension| extension != "identity" && extension != "json"),
                        "identity registry is missing its salt; restore the registry backup"
                    );
                }
                let mut salt = [0; 16];
                OsRng
                    .try_fill_bytes(&mut salt)
                    .map_err(|_| anyhow::anyhow!("identity salt RNG failed"))?;
                write_atomic(&salt_path, &salt, false)?;
                salt.to_vec()
            }
            Err(error) => return Err(error).context("read identity registry salt"),
        };
        ensure!(salt.len() == 16, "invalid registry salt");
        let mut key = Zeroizing::new([0; 32]);
        let params = Params::new(19_456, 2, 1, Some(32))
            .map_err(|_| anyhow::anyhow!("invalid identity derivation parameters"))?;
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase.as_bytes(), &salt, key.as_mut())
            .map_err(|_| anyhow::anyhow!("could not derive identity registry key"))?;
        let cipher = XChaCha20Poly1305::new((&*key).into());
        Ok(Self {
            root,
            cipher,
            allowed: config.allowlist.clone(),
            auto_register: config.auto_register,
            _lock: lock,
        })
    }

    pub fn transport_identity(&self) -> Result<PrivateIdentity> {
        self.identity("transport", true)?
            .map(|(key, _)| key)
            .context("transport identity is missing")
    }

    pub fn known_radios(&self) -> Result<Vec<(String, String, DateTime<Utc>)>> {
        let mut known = Vec::new();
        for id in &self.allowed {
            match fs::read(self.root.join(format!("radio-{id}.json"))) {
                Ok(bytes) => {
                    let data: Metadata = serde_json::from_slice(&bytes)
                        .context("invalid radio registry metadata")?;
                    known.push((id.clone(), data.display_name, data.last_seen));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(known)
    }

    pub fn observe(
        &self,
        id: &str,
        name: &str,
        seen: DateTime<Utc>,
        position: Option<Position>,
    ) -> Result<Option<Registered>> {
        if radio_id(id).as_deref() != Some(id)
            || radio_id(name).as_deref() != Some(id)
            || !self.allowed.contains(id)
        {
            return Ok(None);
        }
        let slot = format!("radio-{id}");
        let path = self.root.join(format!("{slot}.json"));
        let existing = match fs::read(&path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<Metadata>(&bytes)
                    .context("invalid radio registry metadata")?,
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let Some((private_identity, registered_at)) =
            self.identity(&slot, self.auto_register && existing.is_none())?
        else {
            ensure!(
                existing.is_none(),
                "radio {id} has metadata but its identity is missing; restore its backup"
            );
            return Ok(None);
        };
        let destination = SingleOutputDestination::new(
            *private_identity.as_identity(),
            DestinationName::new("lxmf", "delivery"),
        )
        .desc
        .address_hash;
        let hash = destination.to_hex_string();
        let mut metadata = if let Some(existing) = existing {
            ensure!(
                existing.bss_id == id
                    && existing.lxmf_destination == hash
                    && existing.registered_at == registered_at,
                "radio {id} metadata does not match its identity"
            );
            existing
        } else {
            Metadata {
                bss_id: id.to_owned(),
                display_name: name.to_owned(),
                bss_authenticated: false,
                rns_identity_ref: format!("{slot}.identity"),
                lxmf_destination: hash,
                registered_at,
                last_seen: seen,
                last_position: None,
            }
        };
        if seen >= metadata.last_seen {
            metadata.last_seen = seen;
            name.clone_into(&mut metadata.display_name);
        }
        if let Some(position) = position
            && metadata
                .last_position
                .as_ref()
                .is_none_or(|last| seen >= last.received_at)
        {
            metadata.last_position = Some(LastPosition {
                received_at: seen,
                position,
            });
        }
        write_atomic(&path, &serde_json::to_vec_pretty(&metadata)?, true)?;
        Ok(Some(Registered {
            private_identity,
            destination,
        }))
    }

    fn identity(
        &self,
        slot: &str,
        create: bool,
    ) -> Result<Option<(PrivateIdentity, DateTime<Utc>)>> {
        let path = self.root.join(format!("{slot}.identity"));
        let aad = [KEY_FORMAT, slot.as_bytes()].concat();
        match fs::read(&path) {
            Ok(bytes) => {
                let payload = bytes
                    .strip_prefix(KEY_FORMAT)
                    .context("unexpected identity format")?;
                let (nonce, encrypted) = payload
                    .split_first_chunk::<24>()
                    .context("truncated identity file")?;
                let clear = Zeroizing::new(self.cipher.decrypt(XNonce::from_slice(nonce), Payload { msg: encrypted, aad: &aad })
                    .map_err(|_| anyhow::anyhow!("could not authenticate {slot} identity; check passphrase or restore backup"))?);
                let (seconds, key) = clear
                    .split_first_chunk::<8>()
                    .context("truncated identity timestamp")?;
                let created = DateTime::from_timestamp(i64::from_be_bytes(*seconds), 0)
                    .context("invalid identity timestamp")?;
                let key = PrivateIdentity::from_private_key_bytes(key)
                    .map_err(|_| anyhow::anyhow!("invalid private identity"))?;
                Ok(Some((key, created)))
            }
            Err(error) if error.kind() == ErrorKind::NotFound && create => {
                let identity = PrivateIdentity::new_from_rand(OsRng);
                let seconds = Utc::now().timestamp();
                let mut nonce = [0; 24];
                OsRng
                    .try_fill_bytes(&mut nonce)
                    .map_err(|_| anyhow::anyhow!("identity nonce RNG failed"))?;
                let mut clear = Zeroizing::new([0; 72]);
                clear[..8].copy_from_slice(&seconds.to_be_bytes());
                let bytes = Zeroizing::new(identity.to_private_key_bytes());
                clear[8..].copy_from_slice(bytes.as_ref());
                let encrypted = self
                    .cipher
                    .encrypt(
                        XNonce::from_slice(&nonce),
                        Payload {
                            msg: clear.as_ref(),
                            aad: &aad,
                        },
                    )
                    .map_err(|_| anyhow::anyhow!("could not encrypt identity"))?;
                write_atomic(&path, &[KEY_FORMAT, &nonce, &encrypted].concat(), false)?;
                let created =
                    DateTime::from_timestamp(seconds, 0).context("invalid current time")?;
                Ok(Some((identity, created)))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => bail!("could not read {slot} identity: {error}"),
        }
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    let parent = path.parent().context("atomic output needs a directory")?;
    let mut file = NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    if replace {
        file.persist(path)?;
    } else {
        file.persist_noclobber(path)?;
    }
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_identities_survive_restart_backup_and_corruption() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("main");
        let mut config = config::Identity {
            auto_register: true,
            registry_path: root.clone(),
            allowlist: ["R4".into(), "R5".into()].into(),
        };
        let password = "registry-test-long-passphrase";
        let time = Utc::now();
        let registry = Registry::open(&config, password)?;
        assert!(Registry::open(&config, password).is_err());
        assert!(
            registry
                .observe("UNKNOWN", "UNKNOWN", time, None)?
                .is_none()
        );
        assert!(registry.observe("R4", "R5", time, None)?.is_none());
        let r4 = registry
            .observe("R4", "r4", time, None)?
            .unwrap()
            .destination;
        let r5 = registry
            .observe("R5", "R5", time, None)?
            .unwrap()
            .destination;
        assert_ne!(r4, r5);
        drop(registry);
        config.auto_register = false;
        let registry = Registry::open(&config, password)?;
        assert_eq!(
            registry
                .observe("R4", "r4", time, None)?
                .unwrap()
                .destination,
            r4
        );
        assert_eq!(
            registry
                .observe("R5", "R5", time, None)?
                .unwrap()
                .destination,
            r5
        );
        drop(registry);
        let backup = temp.path().join("restored");
        fs::create_dir(&backup)?;
        for entry in root.read_dir()? {
            let entry = entry?;
            if entry.file_name() != "registry.lock" {
                fs::copy(entry.path(), backup.join(entry.file_name()))?;
            }
        }
        config.registry_path = backup.clone();
        let registry = Registry::open(&config, password)?;
        assert_eq!(
            registry
                .observe("R4", "R4", time, None)?
                .unwrap()
                .destination,
            r4
        );
        assert_eq!(
            registry
                .observe("R5", "R5", time, None)?
                .unwrap()
                .destination,
            r5
        );
        let path = backup.join("radio-R4.identity");
        let mut data = fs::read(&path)?;
        *data.last_mut().unwrap() ^= 1;
        fs::write(&path, &data)?;
        assert!(registry.observe("R4", "R4", time, None).is_err());
        assert_eq!(fs::read(path)?, data);
        assert_eq!(
            registry
                .observe("R5", "R5", time, None)?
                .unwrap()
                .destination,
            r5
        );
        Ok(())
    }
}
