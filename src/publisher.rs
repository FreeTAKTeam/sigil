use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, MutexGuard, PoisonError};
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
use rand_core::{OsRng, RngCore};
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
const MAX_BACKOFF: i64 = 300;
const RECEIPT_CAPACITY: usize = 256;
/// Backlog summaries and receipt-eviction warnings are rate-limited to this.
const SUMMARY_INTERVAL: Duration = Duration::from_mins(1);
const LINK_CONNECT_SECS: u64 = 4;
const RESOURCE_TRANSFER_SECS: u64 = 10;
const PATH_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);
/// Above `recover_direct`'s own bound: a path wait of the link timeout, then
/// link activation and transfer within link + resource timeouts.
const RECOVER_TIMEOUT: Duration =
    Duration::from_secs(2 * LINK_CONNECT_SECS + RESOURCE_TRANSFER_SECS + 2);
const DESTINATION_TIMEOUT: Duration = Duration::from_secs(10);
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(5);

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
    queue: Queue,
    backlog: Option<(Backlog, Instant)>,
    _journal_lock: File,
}

/// Proofs reported by the transport. The transport calls `on_receipt` from
/// its packet-drain task; every critical section here is a short scan of at
/// most `RECEIPT_CAPACITY` hashes and never spans an await.
#[derive(Clone, Default)]
struct ReceiptInbox(Arc<std::sync::Mutex<Receipts>>);

#[derive(Default)]
struct Receipts {
    hashes: VecDeque<String>,
    evicted: u64,
    reported: Option<Instant>,
}

impl ReceiptHandler for ReceiptInbox {
    fn on_receipt(&self, receipt: &DeliveryReceipt) {
        self.push(hex::encode(receipt.packet_hash().as_slice()));
    }
}

impl ReceiptInbox {
    fn lock(&self) -> MutexGuard<'_, Receipts> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, hash: String) {
        let evicted = {
            let mut inbox = self.lock();
            let mut evicted = None;
            if inbox.hashes.len() >= RECEIPT_CAPACITY {
                inbox.hashes.pop_front();
                inbox.evicted += 1;
                if inbox
                    .reported
                    .is_none_or(|last| last.elapsed() >= SUMMARY_INTERVAL)
                {
                    inbox.reported = Some(Instant::now());
                    evicted = Some(std::mem::take(&mut inbox.evicted));
                }
            }
            inbox.hashes.push_back(hash);
            evicted
        };
        if let Some(count) = evicted {
            log(format_args!(
                "Fabric receipt inbox full: dropped {count} unclaimed proof(s)"
            ));
        }
    }

    fn contains(&self, expected: &str) -> bool {
        self.lock().hashes.iter().any(|receipt| receipt == expected)
    }

    fn take(&self, expected: &str) -> bool {
        let mut inbox = self.lock();
        inbox
            .hashes
            .iter()
            .position(|receipt| receipt == expected)
            .and_then(|index| inbox.hashes.remove(index))
            .is_some()
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

/// Retry delay after `attempts` tries: `3 * 2^min(n,7)` seconds, +/-20% from
/// `entropy`, never above `MAX_BACKOFF`.
fn backoff(attempts: u32, entropy: u32) -> i64 {
    let base = (3 * 2_i64.pow(attempts.min(7))).min(MAX_BACKOFF);
    let spread = base / 5;
    (base - spread + i64::from(entropy) % (2 * spread + 1)).min(MAX_BACKOFF)
}

fn bounded<F: Future>(
    runtime: &Runtime,
    limit: Duration,
    what: &str,
    future: F,
) -> Result<F::Output> {
    // The timer must be created inside the runtime.
    runtime
        .block_on(async { tokio::time::timeout(limit, future).await })
        .map_err(|_| anyhow!("{what} timed out after {} s", limit.as_secs()))
}

/// What this process knows about one `gateway_*.json` file. Event files are
/// never rewritten and this process holds the journal lock, so a settled
/// file stays settled and a pending one changes only when processed here.
#[derive(Debug, PartialEq, Eq)]
enum Tracked {
    /// A terminal record, or an event this publisher never sends.
    Settled(Outcome),
    /// Re-read only once `retry_at` passes or its proof arrives.
    Pending {
        timestamp: DateTime<Utc>,
        retry_at: i64,
        awaiting: bool,
        /// The proof that makes it due early; cleared once it failed to.
        receipt: Option<String>,
    },
    /// Retried every pass; logged again only when the error changes.
    Broken(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Confirmed,
    Expired,
    Ignored,
    /// Wrong schema, not live, before activation, invalid ID or no sender.
    Ineligible,
}

impl Tracked {
    fn of(timestamp: DateTime<Utc>, delivery: &Delivery) -> Self {
        match delivery.status {
            Status::Queued | Status::AwaitingProof => Self::Pending {
                timestamp,
                retry_at: delivery.retry_at,
                awaiting: delivery.status == Status::AwaitingProof,
                receipt: (delivery.status == Status::AwaitingProof)
                    .then(|| delivery.receipt_hash.clone())
                    .flatten(),
            },
            Status::TransportConfirmed => Self::Settled(Outcome::Confirmed),
            Status::Expired => Self::Settled(Outcome::Expired),
            Status::PolicyRejected | Status::NoMessage | Status::Ignored => {
                Self::Settled(Outcome::Ignored)
            }
        }
    }

    /// The event's timestamp when it should be processed now.
    fn due(&self, now: i64, proven: &impl Fn(&str) -> bool) -> Option<DateTime<Utc>> {
        match self {
            Self::Pending {
                timestamp,
                retry_at,
                receipt,
                ..
            } if *retry_at <= now || receipt.as_deref().is_some_and(proven) => Some(*timestamp),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Backlog {
    queued: usize,
    awaiting: usize,
    confirmed: usize,
    expired: usize,
    ignored: usize,
    unreadable: usize,
}

impl fmt::Display for Backlog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} queued, {} awaiting proof, {} confirmed, {} expired, {} ignored",
            self.queued, self.awaiting, self.confirmed, self.expired, self.ignored
        )?;
        if self.unreadable > 0 {
            write!(f, ", {} unreadable", self.unreadable)?;
        }
        Ok(())
    }
}

struct Job {
    file: String,
    event: GatewayEvent,
    delivery: Delivery,
    record: PathBuf,
}

/// Selects due events from the output directory. Memory is one entry per
/// `gateway_*.json` file currently in the directory: its name plus a small
/// state (a receipt hash at most). Entries for removed files are dropped.
struct Queue {
    journal: PathBuf,
    active_after: DateTime<Utc>,
    unidentified: Option<String>,
    files: HashMap<String, (Tracked, u64)>,
    pass: u64,
    listing_error: Option<String>,
}

impl Queue {
    fn new(journal: PathBuf, active_after: DateTime<Utc>, unidentified: Option<String>) -> Self {
        Self {
            journal,
            active_after,
            unidentified,
            files: HashMap::new(),
            pass: 0,
            listing_error: None,
        }
    }

    /// Up to `JOBS_PER_PASS` due events, oldest first. One unreadable entry
    /// never blocks the others.
    fn due(
        &mut self,
        directory: &Path,
        now: i64,
        proven: impl Fn(&str) -> bool,
    ) -> Result<Vec<Job>> {
        self.pass += 1;
        let mut due = Vec::new();
        let mut listing_error = None;
        for entry in directory.read_dir()? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    let message = error.to_string();
                    if self.listing_error.as_ref() != Some(&message) {
                        log(format_args!(
                            "Skipping unreadable entry in {}: {message}",
                            directory.display()
                        ));
                    }
                    listing_error = Some(message);
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if name
                .strip_prefix("gateway_")
                .and_then(|rest| rest.strip_suffix(".json"))
                .is_none()
            {
                continue;
            }
            let timestamp = match self.files.get_mut(&name) {
                Some((tracked, seen)) if !matches!(tracked, Tracked::Broken(_)) => {
                    *seen = self.pass;
                    tracked.due(now, &proven)
                }
                _ => self.classify(&name, &entry.path(), now, &proven),
            };
            if let Some(timestamp) = timestamp {
                due.push((timestamp, name));
            }
        }
        self.listing_error = listing_error;
        let pass = self.pass;
        self.files.retain(|_, (_, seen)| *seen == pass);
        due.sort_unstable();
        due.truncate(JOBS_PER_PASS);
        Ok(due
            .into_iter()
            .filter_map(|(_, name)| {
                let path = directory.join(&name);
                self.load(&name, &path)
            })
            .collect())
    }

    fn classify(
        &mut self,
        name: &str,
        path: &Path,
        now: i64,
        proven: &impl Fn(&str) -> bool,
    ) -> Option<DateTime<Utc>> {
        let (event, delivery, _) = self.read(name, path)?;
        let tracked = match delivery {
            Some(delivery) => Tracked::of(event.timestamp, &delivery),
            None => Tracked::Settled(Outcome::Ineligible),
        };
        let due = tracked.due(now, proven);
        self.files.insert(name.into(), (tracked, self.pass));
        due
    }

    fn load(&mut self, name: &str, path: &Path) -> Option<Job> {
        let (event, delivery, record) = self.read(name, path)?;
        Some(Job {
            file: name.into(),
            event,
            delivery: delivery?,
            record,
        })
    }

    /// The event and, when it is eligible, its delivery record. Errors are
    /// remembered and logged once per distinct message.
    fn read(
        &mut self,
        name: &str,
        path: &Path,
    ) -> Option<(GatewayEvent, Option<Delivery>, PathBuf)> {
        let event = match File::open(path)
            .map_err(|error| {
                format!(
                    "Skipping unreadable gateway event {}: {error}",
                    path.display()
                )
            })
            .and_then(|file| {
                serde_json::from_reader::<_, GatewayEvent>(std::io::BufReader::new(file)).map_err(
                    |error| format!("Skipping invalid gateway event {}: {error}", path.display()),
                )
            }) {
            Ok(event) => event,
            Err(message) => return self.broken(name, message),
        };
        let record = self.journal.join(format!("{}.json", event.id));
        if !self.eligible(&event) {
            return Some((event, None, record));
        }
        let delivery = match fs::read(&record) {
            Ok(json) => match serde_json::from_slice::<Delivery>(&json) {
                Ok(delivery) => delivery,
                Err(error) => {
                    return self.broken(
                        name,
                        format!(
                            "Preserving invalid Fabric delivery record {}: {error}",
                            record.display()
                        ),
                    );
                }
            },
            Err(error) if error.kind() == ErrorKind::NotFound => Delivery::default(),
            Err(error) => {
                return self.broken(
                    name,
                    format!(
                        "Skipping unreadable Fabric delivery record {}: {error}",
                        record.display()
                    ),
                );
            }
        };
        Some((event, Some(delivery), record))
    }

    fn broken<T>(&mut self, name: &str, message: String) -> Option<T> {
        if !matches!(self.files.get(name), Some((Tracked::Broken(old), _)) if *old == message) {
            log(&message);
        }
        self.files
            .insert(name.into(), (Tracked::Broken(message), self.pass));
        None
    }

    /// Never changes for this process: the event file, activation time and
    /// configuration are all fixed.
    fn eligible(&self, event: &GatewayEvent) -> bool {
        event.schema == "sdr.gateway.v1"
            && matches!(event.source, Source::Live)
            && event.timestamp >= self.active_after
            && valid_event_id(&event.id)
            && sender_for(&event.kind, self.unidentified.as_deref()).is_some()
    }

    fn update(&mut self, name: &str, timestamp: DateTime<Utc>, delivery: &Delivery) {
        let pass = self.pass;
        self.files
            .insert(name.into(), (Tracked::of(timestamp, delivery), pass));
    }

    /// Stops an arrived proof from making this event due before `retry_at`.
    fn defer(&mut self, name: &str) {
        if let Some((Tracked::Pending { receipt, .. }, _)) = self.files.get_mut(name) {
            *receipt = None;
        }
    }

    fn forget(&mut self, name: &str) {
        self.files.remove(name);
    }

    fn backlog(&self) -> Backlog {
        let mut backlog = Backlog::default();
        for (tracked, _) in self.files.values() {
            match tracked {
                Tracked::Pending {
                    awaiting: false, ..
                } => backlog.queued += 1,
                Tracked::Pending { awaiting: true, .. } => backlog.awaiting += 1,
                Tracked::Settled(Outcome::Confirmed) => backlog.confirmed += 1,
                Tracked::Settled(Outcome::Expired) => backlog.expired += 1,
                Tracked::Settled(Outcome::Ignored) => backlog.ignored += 1,
                Tracked::Settled(Outcome::Ineligible) => {}
                Tracked::Broken(_) => backlog.unreadable += 1,
            }
        }
        backlog
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
            queue: Queue::new(
                journal,
                active_after,
                config.identity.unidentified().map(str::to_owned),
            ),
            backlog: None,
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
        let mut last_error = None;
        while !stopped.load(Ordering::Relaxed) {
            if next_discovery <= Instant::now() {
                if let Err(error) = bounded(
                    &self.runtime,
                    PATH_REQUEST_TIMEOUT,
                    "RCH path request",
                    self.transport.request_path(&self.target, None, None),
                ) {
                    log(format_args!("Fabric: {error:#}"));
                }
                next_discovery = Instant::now() + Duration::from_secs(15);
            }
            self.announce_due();
            match self.scan(events, stopped) {
                Ok(()) => last_error = None,
                Err(error) => {
                    let message = format!("{error:#}");
                    if last_error.as_ref() != Some(&message) {
                        log(format_args!("Fabric queue: {message}"));
                        last_error = Some(message);
                    }
                }
            }
            thread::park_timeout(POLL_INTERVAL);
        }
    }

    fn scan(&mut self, directory: &Path, stopped: &AtomicBool) -> Result<()> {
        let receipts = self.receipts.clone();
        let jobs = self.queue.due(directory, Utc::now().timestamp(), |hash| {
            receipts.contains(hash)
        })?;
        for job in jobs {
            if stopped.load(Ordering::Relaxed) {
                break;
            }
            self.process(job);
        }
        let backlog = self.queue.backlog();
        if self
            .backlog
            .is_none_or(|(last, at)| last != backlog && at.elapsed() >= SUMMARY_INTERVAL)
        {
            log(format_args!("Fabric backlog: {backlog}"));
            self.backlog = Some((backlog, Instant::now()));
        }
        Ok(())
    }

    fn process(&mut self, job: Job) {
        let Job {
            file,
            event,
            mut delivery,
            record,
        } = job;
        let now = Utc::now().timestamp();
        let proof_only = delivery.retry_at > now;
        let outcome = if proof_only {
            // Not due for another attempt: selected because its proof arrived.
            let confirmed = self.confirm(&event, &mut delivery, now);
            (confirmed.is_err() || !delivery.status.pending()).then_some(confirmed)
        } else {
            delivery.attempts = delivery.attempts.saturating_add(1);
            delivery.retry_at = now + backoff(delivery.attempts, OsRng.next_u32());
            if event.timestamp.timestamp() + DELIVERY_TTL <= now {
                delivery.status = Status::Expired;
                None
            } else {
                Some(self.send(&event, &mut delivery, now))
            }
        };
        match outcome {
            Some(Ok(())) => delivery.last_error = None,
            Some(Err(error)) => {
                let message = format!("{error:#}");
                log(format_args!("Fabric deferred {}: {message}", event.id));
                delivery.last_error = Some(message);
            }
            None => {}
        }
        let written = serde_json::to_vec_pretty(&delivery)
            .map_err(anyhow::Error::from)
            .and_then(|json| write_atomic(&record, &json, true));
        if let Err(error) = written {
            log(format_args!(
                "Fabric could not record {}: {error:#}",
                record.display()
            ));
            self.queue.forget(&file);
        } else {
            self.queue.update(&file, event.timestamp, &delivery);
            if proof_only && delivery.status.pending() {
                // The proof did not settle it; wait for `retry_at`, whose
                // attempt checks the inbox again before dispatching.
                self.queue.defer(&file);
            }
        }
    }

    /// Settles an `AwaitingProof` record whose proof has arrived, without
    /// dispatching it again.
    fn confirm(&self, event: &GatewayEvent, delivery: &mut Delivery, now: i64) -> Result<()> {
        let (id, _, _) = sender_for(&event.kind, self.config.identity.unidentified())
            .context("event has no radio")?;
        let sender = self.sender(id)?;
        if let Some(entry) = Self::stored(sender, &event.id)? {
            self.settle(sender, &entry, delivery, now)?;
        }
        Ok(())
    }

    fn sender(&self, id: &str) -> Result<&Sender> {
        self.senders
            .get(id)
            .with_context(|| format!("no Fabric sender for {id:?}"))
    }

    /// The signed message Fabric already holds for an event.
    fn stored(
        sender: &Sender,
        event_id: &str,
    ) -> Result<Option<lxmf_runtime::propagation_store::Status>> {
        let mut known = sender.propagation.application_messages(event_id)?;
        ensure!(
            known.len() <= 1,
            "multiple signed messages already exist for event {event_id}"
        );
        Ok(known.pop())
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
        let sender = self.sender(id)?;
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
                Some(telemetry::fields(*position, event.timestamp)?),
                MessageKind::Telemetry {
                    key: event.id.clone(),
                },
            ),
            _ => {
                delivery.status = Status::NoMessage;
                return Ok(());
            }
        };
        let entry = if let Some(existing) = Self::stored(sender, &event.id)? {
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
            let prepared = bounded(
                &self.runtime,
                PREPARE_TIMEOUT,
                "message preparation",
                sender.propagation.prepare(&request),
            )??;
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
            last_known: self
                .registry
                .last_fix(id)?
                .map(|last| (last.position, last.received_at)),
            heading_is_compass: self.config.kodiak.bss_heading_is_compass,
            marker_max_fix_age_s: self.config.kodiak.marker_max_fix_age_s,
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
        if self.settle(sender, entry, delivery, now)? {
            return Ok(());
        }
        // A proof arriving later settles the record on a later pass.
        let report = bounded(
            &self.runtime,
            RECOVER_TIMEOUT,
            "direct delivery",
            sender.propagation.recover_direct(
                &entry.transient_id,
                now,
                delivery.retry_at,
                PropagationRouteLimits::default(),
                |_, _| true,
            ),
        )??;
        if let Some(report) = report {
            delivery.status = Status::AwaitingProof;
            delivery.receipt_hash.clone_from(&report.receipt_hash);
            if report.representation == DeliveryRepresentation::Resource {
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

    /// Settles a record from Fabric's terminal state or an arrived proof.
    fn settle(
        &self,
        sender: &Sender,
        entry: &lxmf_runtime::propagation_store::Status,
        delivery: &mut Delivery,
        now: i64,
    ) -> Result<bool> {
        delivery.message_id = Some(entry.message_id.clone());
        if entry.state.terminal() {
            use lxmf_runtime::propagation_store::State;
            delivery.status = match entry.state {
                State::Delivered | State::Acknowledged => Status::TransportConfirmed,
                _ => Status::Expired,
            };
            return Ok(true);
        }
        if let Some(hash) = &delivery.receipt_hash
            && self.receipts.take(hash)
        {
            sender
                .propagation
                .delivered(&entry.transient_id, &entry.message_id, false, now)?;
            delivery.status = Status::TransportConfirmed;
            return Ok(true);
        }
        Ok(false)
    }

    fn insert_sender(&mut self, id: &str, name: String, registered: Registered) -> Result<()> {
        let (destination, source) = bounded(
            &self.runtime,
            DESTINATION_TIMEOUT,
            "destination registration",
            async {
                let destination = self
                    .transport
                    .add_destination(
                        registered.private_identity.clone(),
                        DestinationName::new("lxmf", "delivery"),
                    )
                    .await;
                let source = destination.lock().await.desc.address_hash;
                (destination, source)
            },
        )?;
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
        config.link_connect_timeout = Duration::from_secs(LINK_CONNECT_SECS);
        config.link_connect_attempts = 1;
        config.resource_transfer_timeout = Duration::from_secs(RESOURCE_TRANSFER_SECS);
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
        let (runtime, transport) = (&self.runtime, &self.transport);
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
            let announced = bounded(runtime, ANNOUNCE_TIMEOUT, "announce", async {
                transport
                    .set_destination_announce_app_data(&sender.destination, Some(data.clone()))
                    .await;
                transport
                    .send_announce(&sender.destination, Some(&data))
                    .await;
            });
            if let Err(error) = announced {
                log(format_args!("Fabric {:?}: {error:#}", sender.name));
            }
            // A failed announce is retried after the interval, not every pass.
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
            assert_eq!(decoded.telemetry["location"]["latitude"], 40.0);
            assert_eq!(decoded.telemetry["location"]["accuracy"], 0.0);
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

    #[test]
    fn an_arrived_proof_confirms_on_the_next_pass_without_redispatch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let address = TcpListener::bind("127.0.0.1:0")?.local_addr()?.to_string();
        let peer_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let (peer, mut inbox, target) = peer_runtime.block_on(async {
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
        config.identity.allowlist = ["R4".into()].into();
        config.fabric.local_storage_path = dir.path().join("fabric");
        config.fabric.tcp_clients = vec![address];
        config.rch.enabled = true;
        config.rch.destination_hash = hash.to_hex_string();
        let mut publisher = Publisher::open_with_passphrase(&config, "loopback-test-passphrase")?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while publisher
            .runtime
            .block_on(publisher.transport.destination_identity(&hash))
            .is_none()
        {
            ensure!(Instant::now() < deadline, "gateway did not discover peer");
            peer_runtime.block_on(peer.send_announce(&target, None));
            thread::sleep(Duration::from_millis(150));
        }
        let output = Output::new(dir.path().join("events"), 435_995_000, false)?;
        output.save_event(Utc::now(), "gateway", &chat("local-R4-proof", Utc::now()))?;
        let stopped = AtomicBool::new(false);
        let mut next_message =
            |wait| peer_runtime.block_on(async { tokio::time::timeout(wait, inbox.recv()).await });
        let record = publisher.queue.journal.join("local-R4-proof.json");
        let read = || -> Result<Delivery> { Ok(serde_json::from_slice(&fs::read(&record)?)?) };

        publisher.scan(output.directory(), &stopped)?;
        next_message(Duration::from_secs(3))??;
        let sent = read()?;
        assert!(sent.status == Status::AwaitingProof && sent.attempts == 1);
        let receipt: [u8; 32] = hex::decode(sent.receipt_hash.as_deref().context("no receipt")?)?
            .try_into()
            .map_err(|_| anyhow!("receipt hash length"))?;
        // The proof arrives between passes; the record is not yet due again.
        publisher
            .receipts
            .on_receipt(&DeliveryReceipt::new(receipt));
        publisher.scan(output.directory(), &stopped)?;
        let confirmed = read()?;
        assert!(confirmed.status == Status::TransportConfirmed);
        assert_eq!(
            (confirmed.attempts, confirmed.retry_at),
            (sent.attempts, sent.retry_at)
        );
        assert!(confirmed.retry_at > Utc::now().timestamp());
        assert!(
            next_message(Duration::from_secs(1)).is_err(),
            "a confirmed event must not be sent again"
        );
        assert_eq!(publisher.queue.backlog().confirmed, 1);
        Ok(())
    }

    fn chat(id: &str, timestamp: DateTime<Utc>) -> GatewayEvent {
        GatewayEvent {
            schema: "sdr.gateway.v1".into(),
            id: id.into(),
            timestamp,
            frequency_hz: 435_995_000,
            source: Source::Live,
            kind: Kind::Transcript {
                radio_id: Some("R4".into()),
                display_name: Some("R4".into()),
                bss_event_id: None,
                unresolved_reason: None,
                started_at: timestamp,
                text: "Test".into(),
                position: None,
                kodiak: None,
            },
        }
    }

    fn queue() -> Result<(tempfile::TempDir, PathBuf, Queue)> {
        let dir = tempfile::tempdir()?;
        let events = dir.path().join("events");
        let journal = dir.path().join("journal");
        fs::create_dir_all(&events)?;
        fs::create_dir_all(&journal)?;
        let epoch = DateTime::from_timestamp(0, 0).context("epoch")?;
        Ok((dir, events, Queue::new(journal, epoch, None)))
    }

    fn write_event(events: &Path, file: &str, event: &GatewayEvent) -> Result<()> {
        Ok(fs::write(events.join(file), serde_json::to_vec(event)?)?)
    }

    fn write_record(queue: &Queue, id: &str, delivery: &Delivery) -> Result<()> {
        let path = queue.journal.join(format!("{id}.json"));
        Ok(fs::write(path, serde_json::to_vec(delivery)?)?)
    }

    fn state<'q>(queue: &'q Queue, file: &str) -> &'q Tracked {
        &queue.files[&format!("gateway_{file}.json")].0
    }

    #[test]
    fn due_events_are_selected_oldest_first() -> Result<()> {
        let (_dir, events, mut queue) = queue()?;
        let start = Utc::now().timestamp() - 3600;
        let total = JOBS_PER_PASS + 8;
        for index in 0..total {
            // Later file names hold older events, against directory order.
            let at = DateTime::from_timestamp(start - i64::try_from(index)?, 0).context("time")?;
            let event = chat(&format!("e{index}"), at);
            write_event(&events, &format!("gateway_{index:03}.json"), &event)?;
        }
        let jobs = queue.due(&events, Utc::now().timestamp(), |_| false)?;
        let expected: Vec<_> = (0..total)
            .rev()
            .take(JOBS_PER_PASS)
            .map(|index| format!("e{index}"))
            .collect();
        let selected: Vec<_> = jobs.into_iter().map(|job| job.event.id).collect();
        assert_eq!(selected, expected);
        Ok(())
    }

    #[test]
    fn unreadable_events_and_records_do_not_block_others() -> Result<()> {
        let (_dir, events, mut queue) = queue()?;
        let at = Utc::now();
        fs::write(events.join("gateway_a_corrupt.json"), b"{not json")?;
        fs::create_dir(events.join("gateway_b_directory.json"))?;
        write_event(&events, "gateway_c.json", &chat("invalid-record", at))?;
        fs::write(queue.journal.join("invalid-record.json"), b"[]")?;
        write_event(&events, "gateway_d.json", &chat("unreadable-record", at))?;
        fs::create_dir(queue.journal.join("unreadable-record.json"))?;
        write_event(&events, "gateway_e.json", &chat("good", at))?;
        for _ in 0..2 {
            let jobs = queue.due(&events, at.timestamp(), |_| false)?;
            let selected: Vec<_> = jobs.iter().map(|job| job.event.id.as_str()).collect();
            assert_eq!(selected, ["good"]);
        }
        let backlog = queue.backlog();
        assert_eq!((backlog.unreadable, backlog.queued), (4, 1));
        Ok(())
    }

    #[test]
    fn settled_and_deferred_events_are_not_reread() -> Result<()> {
        let (_dir, events, mut queue) = queue()?;
        let at = Utc::now();
        let now = at.timestamp();
        let receipt = "ab".repeat(32);
        write_event(&events, "gateway_confirmed.json", &chat("confirmed", at))?;
        let confirmed = Delivery {
            status: Status::TransportConfirmed,
            ..Delivery::default()
        };
        write_record(&queue, "confirmed", &confirmed)?;
        let mut replay = chat("replay", at);
        replay.source = Source::Wav;
        write_event(&events, "gateway_replay.json", &replay)?;
        write_event(&events, "gateway_waiting.json", &chat("waiting", at))?;
        let waiting = Delivery {
            status: Status::AwaitingProof,
            attempts: 1,
            retry_at: now + 60,
            receipt_hash: Some(receipt.clone()),
            ..Delivery::default()
        };
        write_record(&queue, "waiting", &waiting)?;
        assert!(queue.due(&events, now, |_| false)?.is_empty());
        for file in ["confirmed", "replay", "waiting"] {
            fs::write(events.join(format!("gateway_{file}.json")), b"overwritten")?;
        }
        assert!(queue.due(&events, now, |_| false)?.is_empty());
        assert_eq!(
            state(&queue, "confirmed"),
            &Tracked::Settled(Outcome::Confirmed)
        );
        assert_eq!(
            state(&queue, "replay"),
            &Tracked::Settled(Outcome::Ineligible)
        );
        assert!(matches!(state(&queue, "waiting"), Tracked::Pending { .. }));
        // Its proof makes the deferred event due, so only now is it read again.
        assert!(queue.due(&events, now, |hash| hash == receipt)?.is_empty());
        assert!(matches!(state(&queue, "waiting"), Tracked::Broken(_)));
        fs::remove_file(events.join("gateway_confirmed.json"))?;
        queue.due(&events, now, |_| false)?;
        assert!(!queue.files.contains_key("gateway_confirmed.json"));
        Ok(())
    }

    #[test]
    fn backoff_jitter_stays_within_a_fifth_and_under_the_cap() {
        for attempts in 1..12 {
            let base = (3 * 2_i64.pow(attempts.min(7))).min(MAX_BACKOFF);
            let delays: Vec<_> = (0..=200)
                .map(|step| backoff(attempts, step * 21_474_836))
                .chain([backoff(attempts, u32::MAX)])
                .collect();
            for &delay in &delays {
                assert!(
                    delay * 5 >= base * 4 && delay * 5 <= base * 6,
                    "{delay} vs {base}"
                );
                assert!(delay <= MAX_BACKOFF);
            }
            assert!(delays.iter().min() < delays.iter().max());
        }
    }

    #[test]
    fn receipt_inbox_recovers_from_poison_and_evicts_the_oldest() -> Result<()> {
        let inbox = ReceiptInbox::default();
        let poisoner = inbox.clone();
        let outcome = thread::spawn(move || {
            let _guard = poisoner.0.lock();
            panic!("poison the receipt inbox");
        })
        .join();
        assert!(outcome.is_err() && inbox.0.is_poisoned());
        let receipt = |index: usize| -> Result<[u8; 32]> {
            let mut hash = [0_u8; 32];
            hash[..2].copy_from_slice(&u16::try_from(index)?.to_be_bytes());
            Ok(hash)
        };
        inbox.on_receipt(&DeliveryReceipt::new(receipt(0)?));
        assert!(inbox.take(&hex::encode(receipt(0)?)));
        assert!(!inbox.take(&hex::encode(receipt(0)?)));
        for index in 0..=RECEIPT_CAPACITY {
            inbox.on_receipt(&DeliveryReceipt::new(receipt(index)?));
        }
        assert_eq!(inbox.lock().hashes.len(), RECEIPT_CAPACITY);
        assert!(!inbox.contains(&hex::encode(receipt(0)?)));
        assert!(inbox.contains(&hex::encode(receipt(1)?)));
        assert!(inbox.contains(&hex::encode(receipt(RECEIPT_CAPACITY)?)));
        Ok(())
    }
}
