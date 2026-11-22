use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use lxmf_core::announce::encode_delivery_announce_app_data_with_capabilities;
use lxmf_runtime::propagation_store::{MessageKind, StoreLimits};
use lxmf_runtime::{
    DeliveryRepresentation, EXT_FIELDS_BASE64, EXT_RAW_BYTES_BASE64, InProcessBackend,
    InProcessBackendConfig, LocalPropagation, PropagationRouteLimits,
};
use lxmf_sdk::SendRequest;
use rns_transport::destination::{DestinationName, SingleInputDestination};
use rns_transport::hash::AddressHash;
use rns_transport::iface::tcp_client::TcpClient;
use rns_transport::transport::{DeliveryReceipt, ReceiptHandler, Transport, TransportConfig};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::config::Config;
use crate::correlation::{GatewayEvent, Kind, Unresolved};
use crate::identity::{PASSPHRASE_ENV, Registered, Registry, UNIDENTIFIED, write_atomic};
use crate::intents::{self, Plan};
use crate::output::{Source, log};
use crate::telemetry;

const DELIVERY_TTL: i64 = 7 * 24 * 60 * 60;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(90);
const JOBS_PER_PASS: usize = 32;

struct Sender {
    destination: Arc<Mutex<SingleInputDestination>>,
    hash: String,
    propagation: LocalPropagation,
    name: String,
    last_announce: Option<Instant>,
}

pub struct Publisher<'a> {
    config: &'a Config,
    registry: Registry,
    runtime: Runtime,
    transport: Arc<Transport>,
    target: AddressHash,
    senders: HashMap<String, Sender>,
    receipts: ReceiptInbox,
    active_after: DateTime<Utc>,
    journal: PathBuf,
    _journal_lock: File,
}

#[derive(Clone, Default)]
struct ReceiptInbox(Arc<std::sync::Mutex<VecDeque<String>>>);

impl ReceiptHandler for ReceiptInbox {
    fn on_receipt(&self, receipt: &DeliveryReceipt) {
        if let Ok(mut inbox) = self.0.try_lock() {
            if inbox.len() >= 256 {
                inbox.pop_front();
            }
            inbox.push_back(hex::encode(receipt.packet_hash().as_slice()));
        }
    }
}

impl ReceiptInbox {
    fn take(&self, expected: &str) -> bool {
        self.0.lock().ok().is_some_and(|mut inbox| {
            inbox
                .iter()
                .position(|receipt| receipt == expected)
                .and_then(|index| inbox.remove(index))
                .is_some()
        })
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Delivery {
    status: Status,
    attempts: u32,
    retry_at: i64,
    source_destination: Option<String>,
    message_id: Option<String>,
    receipt_hash: Option<String>,
    last_error: Option<String>,
    /// Why an event was ignored locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Status {
    #[default]
    Queued,
    AwaitingProof,
    TransportConfirmed,
    PolicyRejected,
    NoMessage,
    /// Kodiak rejected the command, or a required position or heading was missing.
    Ignored,
    Expired,
}

impl Status {
    fn pending(&self) -> bool {
        matches!(self, Self::Queued | Self::AwaitingProof)
    }
}

impl<'a> Publisher<'a> {
    pub fn open(config: &'a Config) -> Result<Self> {
        ensure!(config.rch.enabled, "RCH publishing is disabled");
        let passphrase = Zeroizing::new(
            std::env::var(PASSPHRASE_ENV)
                .with_context(|| format!("set {PASSPHRASE_ENV} before enabling RCH publishing"))?,
        );
        Self::open_with_passphrase(config, &passphrase)
    }

    fn open_with_passphrase(config: &'a Config, passphrase: &str) -> Result<Self> {
        let registry = Registry::open(&config.identity, passphrase)?;
        let node_identity = registry.transport_identity()?;
        let target = AddressHash::new_from_hex_string(&config.rch.destination_hash)
            .map_err(|_| anyhow!("invalid RCH LXMF delivery hash"))?;
        let journal = config
            .fabric
            .local_storage_path
            .join(target.to_hex_string());
        fs::create_dir_all(&journal).context("create Fabric delivery journal")?;
        let journal_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(journal.join("publisher.lock"))?;
        journal_lock
            .try_lock()
            .context("Fabric delivery journal is already in use")?;
        let cutover = journal.join("enabled.json");
        let active_after = match fs::read(&cutover) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("invalid Fabric activation record")?
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let now = Utc::now();
                write_atomic(&cutover, &serde_json::to_vec(&now)?, false)?;
                now
            }
            Err(error) => return Err(error.into()),
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let receipts = ReceiptInbox::default();
        let transport = runtime.block_on(async {
            let transport = Arc::new(Transport::new(TransportConfig::new(
                "r3akt-sigil",
                &node_identity,
                false,
            )));
            transport
                .set_receipt_handler(Box::new(receipts.clone()))
                .await;
            for interface in &config.fabric.tcp_clients {
                transport
                    .add_interface(TcpClient::new(interface.clone()), TcpClient::spawn)
                    .await;
            }
            transport
        });
        let mut publisher = Self {
            config,
            registry,
            runtime,
            transport,
            target,
            senders: HashMap::new(),
            receipts,
            active_after,
            journal,
            _journal_lock: journal_lock,
        };
        for (id, display_name, last_seen) in publisher.registry.known_radios()? {
            if let Some(registered) =
                publisher
                    .registry
                    .observe(&id, &display_name, last_seen, None)?
            {
                publisher.insert_sender(&id, display_name, registered)?;
            }
        }
        if let Some(name) = config.identity.unidentified() {
            let registered = publisher.registry.unidentified_identity()?;
            log(format_args!(
                "Unidentified speech publishes as {name:?} ({})",
                registered.destination.to_hex_string()
            ));
            publisher.insert_sender(UNIDENTIFIED, name.to_owned(), registered)?;
        }
        Ok(publisher)
    }

    pub fn run(&mut self, events: &Path, stopped: &AtomicBool) {
        let mut next_discovery = Instant::now();
        while !stopped.load(Ordering::Relaxed) {
            if next_discovery <= Instant::now() {
                self.runtime
                    .block_on(self.transport.request_path(&self.target, None, None));
                next_discovery = Instant::now() + Duration::from_secs(15);
            }
            self.announce_due();
            if let Err(error) = self.scan(events, stopped) {
                log(format_args!("Fabric queue: {error:#}"));
            }
            thread::park_timeout(POLL_INTERVAL);
        }
    }

    fn scan(&mut self, directory: &Path, stopped: &AtomicBool) -> Result<()> {
        let now = Utc::now().timestamp();
        let mut jobs = Vec::new();
        for entry in directory.read_dir()? {
            let entry = entry?;
            let filename = entry.file_name();
            let name = filename.to_string_lossy();
            if !name.starts_with("gateway_") || !name.ends_with(".json") {
                continue;
            }
            let event = match serde_json::from_reader::<_, GatewayEvent>(File::open(entry.path())?)
            {
                Ok(event) => event,
                Err(error) => {
                    log(format_args!(
                        "Skipping invalid gateway event {}: {error}",
                        entry.path().display()
                    ));
                    continue;
                }
            };
            if event.schema != "sdr.gateway.v1"
                || !matches!(event.source, Source::Live)
                || event.timestamp < self.active_after
                || !valid_event_id(&event.id)
                || sender_for(&event.kind, self.config.identity.unidentified()).is_none()
            {
                continue;
            }
            let record = self.journal.join(format!("{}.json", event.id));
            let delivery = match fs::read(&record) {
                Ok(json) => match serde_json::from_slice::<Delivery>(&json) {
                    Ok(delivery) => delivery,
                    Err(error) => {
                        log(format_args!(
                            "Preserving invalid Fabric delivery record {}: {error}",
                            record.display()
                        ));
                        continue;
                    }
                },
                Err(error) if error.kind() == ErrorKind::NotFound => Delivery::default(),
                Err(error) => return Err(error.into()),
            };
            if delivery.status.pending() && delivery.retry_at <= now {
                jobs.push((event, delivery, record));
                if jobs.len() >= JOBS_PER_PASS {
                    break;
                }
            }
        }
        jobs.sort_unstable_by_key(|(event, _, _)| event.timestamp);
        for (event, mut delivery, receipt_path) in jobs {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            let now = Utc::now().timestamp();
            delivery.attempts = delivery.attempts.saturating_add(1);
            delivery.retry_at = now + (2_i64.pow(delivery.attempts.min(7)) * 3).min(300);
            if event.timestamp.timestamp() + DELIVERY_TTL <= now {
                delivery.status = Status::Expired;
            } else if let Err(error) = self.send(&event, &mut delivery, now) {
                let message = format!("{error:#}");
                log(format_args!("Fabric deferred {}: {message}", event.id));
                delivery.last_error = Some(message);
            } else {
                delivery.last_error = None;
            }
            write_atomic(&receipt_path, &serde_json::to_vec_pretty(&delivery)?, true)?;
        }
        Ok(())
    }

    fn send(&mut self, event: &GatewayEvent, delivery: &mut Delivery, now: i64) -> Result<()> {
        let config = self.config;
        let (id, name, position) = sender_for(&event.kind, config.identity.unidentified())
            .context("event has no radio")?;
        if id != UNIDENTIFIED {
            self.register_radio(id, name, event.timestamp, position, delivery)?;
            if delivery.status == Status::PolicyRejected {
                return Ok(());
            }
        }
        self.announce_due();
        let sender = &self.senders[id];
        delivery.source_destination = Some(sender.hash.clone());
        let (content, fields, kind) = match &event.kind {
            Kind::Transcript {
                text,
                kodiak,
                position,
                ..
            } => match self.plan_transcript(event, id, name, text, kodiak.as_deref(), *position)? {
                Plan::Chat(body) => (body, None, MessageKind::Chat),
                Plan::Command { body, fields } => (body, Some(fields), MessageKind::Application),
                Plan::Ignore(reason) => {
                    log(format_args!("Ignored {} from {id}: {reason}", event.id));
                    delivery.status = Status::Ignored;
                    delivery.detail = Some(reason);
                    return Ok(());
                }
                Plan::Nothing => {
                    delivery.status = Status::NoMessage;
                    return Ok(());
                }
            },
            Kind::Position { position, .. } if self.config.rch.send_telemetry => (
                String::new(),
                Some(telemetry::fields(name, *position, event.timestamp)?),
                MessageKind::Telemetry {
                    key: event.id.clone(),
                },
            ),
            _ => {
                delivery.status = Status::NoMessage;
                return Ok(());
            }
        };
        let mut known = sender.propagation.application_messages(&event.id)?;
        ensure!(
            known.len() <= 1,
            "multiple signed messages already exist for event {}",
            event.id
        );
        let entry = if let Some(existing) = known.pop() {
            existing
        } else {
            let mut request =
                SendRequest::new(&sender.hash, self.target.to_hex_string(), json!({}))
                    .with_correlation_id(&event.id)
                    .with_delivery_method("direct");
            request
                .extensions
                .insert(EXT_RAW_BYTES_BASE64.into(), json!(STANDARD.encode(content)));
            if let Some(fields) = fields {
                request
                    .extensions
                    .insert(EXT_FIELDS_BASE64.into(), json!(STANDARD.encode(fields)));
            }
            let prepared = self
                .runtime
                .block_on(sender.propagation.prepare(&request))?;
            sender.propagation.deposit_own(
                prepared,
                kind,
                now,
                Some(event.timestamp.timestamp() + DELIVERY_TTL),
            )?
        };
        self.dispatch_verified(sender, &entry, delivery, now, id)
    }

    /// Resolves a BSS radio's identity and keeps its sender current.
    fn register_radio(
        &mut self,
        id: &str,
        name: &str,
        seen: DateTime<Utc>,
        position: Option<crate::bss::Position>,
        delivery: &mut Delivery,
    ) -> Result<()> {
        let Some(registered) = self.registry.observe(id, name, seen, position)? else {
            delivery.status = Status::PolicyRejected;
            log(format_args!(
                "BSS radio {id:?} was rejected by the identity policy"
            ));
            return Ok(());
        };
        if let Some(sender) = self.senders.get_mut(id) {
            ensure!(
                sender.hash == registered.destination.to_hex_string(),
                "stored radio sender identity changed"
            );
            if sender.name != name {
                name.clone_into(&mut sender.name);
                sender.last_announce = None;
            }
        } else {
            self.insert_sender(id, name.into(), registered)?;
        }
        Ok(())
    }

    fn plan_transcript(
        &self,
        event: &GatewayEvent,
        id: &str,
        name: &str,
        text: &str,
        kodiak: Option<&kodiak_core::KodiakResult>,
        current: Option<crate::bss::Position>,
    ) -> Result<Plan> {
        let facts = intents::Context {
            radio_name: name,
            event_id: &event.id,
            frequency_hz: event.frequency_hz,
            timestamp: event.timestamp,
            current,
            last_known: self.registry.last_position(id)?,
            heading_is_compass: self.config.kodiak.bss_heading_is_compass,
            send_chat: self.config.rch.send_chat,
            send_intents: self.config.rch.send_intents,
        };
        intents::plan(text, kodiak, &facts)
    }

    fn dispatch_verified(
        &self,
        sender: &Sender,
        entry: &lxmf_runtime::propagation_store::Status,
        delivery: &mut Delivery,
        now: i64,
        id: &str,
    ) -> Result<()> {
        delivery.message_id = Some(entry.message_id.clone());
        if entry.state.terminal() {
            use lxmf_runtime::propagation_store::State;
            delivery.status = match entry.state {
                State::Delivered | State::Acknowledged => Status::TransportConfirmed,
                _ => Status::Expired,
            };
            return Ok(());
        }
        if let Some(hash) = &delivery.receipt_hash
            && self.receipts.take(hash)
        {
            sender
                .propagation
                .delivered(&entry.transient_id, &entry.message_id, false, now)?;
            delivery.status = Status::TransportConfirmed;
            return Ok(());
        }
        let report = self.runtime.block_on(sender.propagation.recover_direct(
            &entry.transient_id,
            now,
            delivery.retry_at,
            PropagationRouteLimits::default(),
            |_, _| true,
        ))?;
        if let Some(report) = report {
            delivery.status = Status::AwaitingProof;
            delivery.receipt_hash.clone_from(&report.receipt_hash);
            if report.representation == DeliveryRepresentation::Resource
                || report
                    .receipt_hash
                    .as_ref()
                    .is_some_and(|hash| self.wait_for_proof(hash))
            {
                sender
                    .propagation
                    .delivered(&entry.transient_id, &entry.message_id, false, now)?;
                delivery.status = Status::TransportConfirmed;
            }
            log(format_args!(
                "Fabric dispatched {} from {id} ({})",
                report.message_id.0, sender.hash
            ));
        }
        Ok(())
    }

    fn wait_for_proof(&self, expected: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.receipts.take(expected) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn insert_sender(&mut self, id: &str, name: String, registered: Registered) -> Result<()> {
        let destination = self.runtime.block_on(self.transport.add_destination(
            registered.private_identity.clone(),
            DestinationName::new("lxmf", "delivery"),
        ));
        let source = self
            .runtime
            .block_on(async { destination.lock().await.desc.address_hash });
        ensure!(
            source == registered.destination,
            "Fabric registered a different delivery destination"
        );
        let mut config = InProcessBackendConfig::new(
            format!("r3akt-sigil:{id}"),
            self.runtime.handle().clone(),
            self.transport.clone(),
            registered.private_identity,
            source,
        );
        config.link_connect_timeout = Duration::from_secs(4);
        config.link_connect_attempts = 1;
        config.resource_transfer_timeout = Duration::from_secs(10);
        let propagation = InProcessBackend::new(config).local_propagation(
            self.config
                .fabric
                .local_storage_path
                .join(format!("radio-{id}.sqlite3")),
            StoreLimits::default(),
        )?;
        self.senders.insert(
            id.into(),
            Sender {
                destination,
                hash: source.to_hex_string(),
                propagation,
                name,
                last_announce: None,
            },
        );
        Ok(())
    }

    fn announce_due(&mut self) {
        for sender in self.senders.values_mut() {
            if sender
                .last_announce
                .is_some_and(|last| last.elapsed() < ANNOUNCE_INTERVAL)
            {
                continue;
            }
            let data = match encode_delivery_announce_app_data_with_capabilities(
                &sender.name,
                None,
                &[],
            ) {
                Ok(data) => data,
                Err(error) => {
                    log(format_args!(
                        "Fabric could not encode an announce for {:?}: {error:?}",
                        sender.name
                    ));
                    continue;
                }
            };
            self.runtime.block_on(async {
                self.transport
                    .set_destination_announce_app_data(&sender.destination, Some(data.clone()))
                    .await;
                self.transport
                    .send_announce(&sender.destination, Some(&data))
                    .await;
            });
            sender.last_announce = Some(Instant::now());
        }
    }
}

fn valid_event_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        && !matches!(id, "." | ".." | "enabled")
}

/// The sender for an event: its BSS radio, or the unidentified-speech identity
/// for a transcript that carried no BSS ID when that identity is enabled.
/// Ambiguous or incomplete attributions are never published.
fn sender_for<'e>(
    kind: &'e Kind,
    unidentified: Option<&'e str>,
) -> Option<(&'e str, &'e str, Option<crate::bss::Position>)> {
    radio_for(kind).or_else(|| match kind {
        Kind::Transcript {
            radio_id: None,
            unresolved_reason: Some(Unresolved::MissingIdentifier),
            ..
        } => unidentified.map(|name| (UNIDENTIFIED, name, None)),
        _ => None,
    })
}

fn radio_for(kind: &Kind) -> Option<(&str, &str, Option<crate::bss::Position>)> {
    match kind {
        Kind::RadioSeen {
            radio_id,
            display_name,
        } => Some((radio_id, display_name, None)),
        Kind::Transcript {
            radio_id: Some(radio_id),
            display_name: Some(display_name),
            unresolved_reason: None,
            position,
            ..
        } => Some((radio_id, display_name, *position)),
        Kind::Position {
            radio_id,
            display_name,
            position,
        } => Some((radio_id, display_name, Some(*position))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use lxmf_core::{WireMessage, constants::FIELD_TELEMETRY, identity::Identity};
    use rand_core::OsRng;
    use rns_transport::identity::PrivateIdentity;
    use rns_transport::iface::tcp_server::TcpServer;
    use rns_transport::transport::ReceivedPayloadMode;

    use super::*;
    use crate::bss::Position;
    use crate::output::Output;

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "Keep the loopback setup, actual send and peer assertions together"
    )]
    fn shared_transport_delivers_signed_chat_and_telemetry_from_two_identities() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let address = TcpListener::bind("127.0.0.1:0")?.local_addr()?.to_string();
        let peer_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let (peer, inbox, target) = peer_runtime.block_on(async {
            let private = PrivateIdentity::new_from_rand(OsRng);
            let transport =
                Transport::new(TransportConfig::new("gateway-test-peer", &private, false));
            let (server, ready) =
                TcpServer::new(address.clone(), transport.iface_manager()).with_startup_result();
            transport.add_interface(server, TcpServer::spawn).await;
            tokio::time::timeout(Duration::from_secs(5), ready)
                .await??
                .map_err(|err| anyhow!(err))?;
            let inbox = transport.received_data_events();
            let target = transport
                .add_destination(private, DestinationName::new("lxmf", "delivery"))
                .await;
            Ok::<_, anyhow::Error>((transport, inbox, target))
        })?;
        let hash = peer_runtime.block_on(async { target.lock().await.desc.address_hash });
        let mut config = Config::default();
        config.identity.auto_register = true;
        config.identity.registry_path = dir.path().join("identity");
        config.identity.allowlist = ["R4".into(), "R5".into()].into();
        config.identity.unidentified_sender = true;
        config.fabric.local_storage_path = dir.path().join("fabric");
        config.fabric.tcp_clients = vec![address];
        config.rch.enabled = true;
        config.rch.destination_hash = hash.to_hex_string();
        let secret = "loopback-test-long-passphrase";
        let mut publisher = Publisher::open_with_passphrase(&config, secret)?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while publisher
            .runtime
            .block_on(publisher.transport.destination_identity(&hash))
            .is_none()
        {
            ensure!(
                Instant::now() < deadline,
                "gateway did not discover loopback recipient"
            );
            peer_runtime.block_on(peer.send_announce(&target, None));
            thread::sleep(Duration::from_millis(150));
        }
        let output = Output::new(dir.path().join("events"), 435_995_000, false)?;
        for name in ["R4", "R5"] {
            let mut event = GatewayEvent {
                schema: "sdr.gateway.v1".into(),
                id: format!("local-{name}-chat"),
                timestamp: Utc::now(),
                frequency_hz: output.frequency_hz(),
                source: Source::Live,
                kind: Kind::Transcript {
                    radio_id: Some(name.into()),
                    display_name: Some(name.into()),
                    bss_event_id: Some(format!("local-{name}-packet")),
                    unresolved_reason: None,
                    started_at: Utc::now(),
                    text: format!("Test from {name}"),
                    position: None,
                    kodiak: None,
                },
            };
            output.save_event(event.timestamp, "gateway", &event)?;
            event.id = format!("local-{name}-position");
            event.kind = Kind::Position {
                radio_id: name.into(),
                display_name: name.into(),
                position: Position {
                    latitude: 40.0,
                    longitude: -75.0,
                    altitude_m: None,
                    speed_kmh: None,
                    heading_deg: Some(354),
                },
            };
            output.save_event(event.timestamp, "gateway", &event)?;
        }
        // A Kodiak RCH command from R4 is forwarded as a field command; a
        // rejected Kodiak command from R5 is never sent.
        let engine = kodiak_core::Engine::new(kodiak_intent::DeterministicClassifier::new());
        for (name, spoken) in [
            ("R4", "Kodiak list users"),
            ("R5", "Kodiak launch the missiles"),
        ] {
            let result = engine.process_text(&kodiak_core::TextInput::new(spoken));
            let event = GatewayEvent {
                schema: "sdr.gateway.v1".into(),
                id: format!("local-{name}-kodiak"),
                timestamp: Utc::now(),
                frequency_hz: output.frequency_hz(),
                source: Source::Live,
                kind: Kind::Transcript {
                    radio_id: Some(name.into()),
                    display_name: Some(name.into()),
                    bss_event_id: Some(format!("local-{name}-packet")),
                    unresolved_reason: None,
                    started_at: Utc::now(),
                    text: spoken.into(),
                    position: None,
                    kodiak: Some(Box::new(result)),
                },
            };
            output.save_event(event.timestamp, "gateway", &event)?;
        }
        // Speech without any BSS ID goes out from the unidentified identity.
        let event = GatewayEvent {
            schema: "sdr.gateway.v1".into(),
            id: "local-unidentified-chat".into(),
            timestamp: Utc::now(),
            frequency_hz: output.frequency_hz(),
            source: Source::Live,
            kind: Kind::Transcript {
                radio_id: None,
                display_name: None,
                bss_event_id: None,
                unresolved_reason: Some(Unresolved::MissingIdentifier),
                started_at: Utc::now(),
                text: "Unidentified speaker".into(),
                position: None,
                kodiak: None,
            },
        };
        output.save_event(event.timestamp, "gateway", &event)?;
        publisher.scan(output.directory(), &AtomicBool::new(false))?;
        let mut inbox = inbox;
        let mut received = Vec::new();
        for _ in 0..6 {
            let data = peer_runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(3), inbox.recv()).await
            })??;
            let wire = if data.payload_mode == ReceivedPayloadMode::DestinationStripped {
                [data.destination.as_slice(), data.data.as_slice()].concat()
            } else {
                data.data.as_slice().to_vec()
            };
            received.push(WireMessage::unpack(&wire)?);
        }
        let mut known = Vec::new();
        for name in ["R4", "R5"] {
            let registered = publisher
                .registry
                .observe(name, name, Utc::now(), None)?
                .unwrap();
            let native = registered.private_identity.as_identity();
            let identity =
                Identity::new_from_slices(native.public_key_bytes(), native.verifying_key_bytes());
            let messages: Vec<_> = received
                .iter()
                .filter(|message| message.source.as_slice() == registered.destination.as_slice())
                .collect();
            assert_eq!(messages.len(), if name == "R4" { 3 } else { 2 });
            if name == "R4" {
                let command = messages
                    .iter()
                    .find_map(|message| {
                        message
                            .payload
                            .fields
                            .as_ref()?
                            .as_map()?
                            .iter()
                            .find(|(key, _)| key.as_u64() == Some(9))
                            .map(|(_, value)| value.clone())
                    })
                    .context("Kodiak command must be LXMF field 0x09")?;
                let command = &command.as_array().context("field 9 is an array")?[0];
                let lookup = |key: &str| {
                    command
                        .as_map()
                        .and_then(|map| map.iter().find(|(k, _)| k.as_str() == Some(key)))
                        .and_then(|(_, value)| value.as_str().map(str::to_owned))
                };
                assert_eq!(lookup("command_type").as_deref(), Some("mission.users"));
                assert_eq!(lookup("command_id").as_deref(), Some("local-R4-kodiak"));
            }
            assert!(
                messages
                    .iter()
                    .all(|message| message.verify(&identity).unwrap())
            );
            assert!(messages.iter().any(|message| {
                message.payload.content.as_ref().is_some_and(|content| {
                    content.ends_with(format!("Test from {name}").as_bytes())
                })
            }));
            let packed = messages
                .iter()
                .find_map(|message| {
                    message
                        .payload
                        .fields
                        .as_ref()?
                        .as_map()?
                        .iter()
                        .find(|(key, _)| key.as_u64() == Some(u64::from(FIELD_TELEMETRY)))?
                        .1
                        .as_slice()
                })
                .context("telemetry must be binary field 0x02")?;
            let decoded = r3akt_map_core::telemetry::decode_field(&json!(STANDARD.encode(packed)))
                .map_err(|error| anyhow!(error))?;
            assert_eq!(decoded.telemetry["information"]["contents"], name);
            assert_eq!(decoded.telemetry["location"]["latitude"], 40.0);
            assert!(decoded.telemetry["location"].get("accuracy").is_none());
            known.push((name, registered.destination));
        }
        assert_ne!(known[0].1, known[1].1);
        let unidentified = publisher.registry.unidentified_identity()?;
        assert!(
            known
                .iter()
                .all(|(_, radio)| *radio != unidentified.destination)
        );
        assert!(received.iter().any(|message| {
            message.source.as_slice() == unidentified.destination.as_slice()
                && message
                    .payload
                    .content
                    .as_ref()
                    .is_some_and(|content| content.ends_with(b"Unidentified speaker"))
        }));
        drop(publisher);
        let restarted = Publisher::open_with_passphrase(&config, secret)?;
        for (id, destination) in known {
            assert_eq!(restarted.senders[id].hash, destination.to_hex_string());
        }
        Ok(())
    }
}
