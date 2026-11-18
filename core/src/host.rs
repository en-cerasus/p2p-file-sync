use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, ServerName},
    RootCertStore,
};
use serde::Deserialize;

use crate::{
    auth::{PeerSigningKey, Role, Roster},
    blob::BlobStore,
    direct::{self, DirectError, PeerEndpoint},
    generation::{
        BaselineError, CheckpointDataError, GenerationBaseline, GenerationTransition,
        TransitionError, MAX_TRANSITION_BYTES,
    },
    names::collision_key,
    projection::Projection,
    publish::{
        publish_file, publish_tree, refresh_tree, relocate_file, resolve_file_conflict,
        revise_file, PublishError, TreeSummary,
    },
    replica::{EntryKind, Event, EventId, PeerId},
    store::{Store, StoreError},
    sync::{Destination, FileResult},
    tls::{CertificatePin, Client, Credentials, Server, TlsError},
};

const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;
const MAX_MEMBERS: usize = 1024;
const MAX_CHECKPOINT_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    share: String,
    generation: u64,
    members: BTreeMap<String, ConfigRole>,
    address: SocketAddr,
    server_name: String,
    server_pin: String,
    #[serde(default)]
    fallback_peers: Vec<EndpointConfig>,
    ca_der: PathBuf,
    certificate_der: PathBuf,
    private_key_der: PathBuf,
    event_key: Option<PathBuf>,
    migration: Option<MigrationConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointConfig {
    address: SocketAddr,
    server_name: String,
    server_pin: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationConfig {
    previous_server: PathBuf,
    administrator_public_key: PathBuf,
    bundle: PathBuf,
}

impl Config {
    fn endpoints(&self) -> Result<Vec<PeerEndpoint>, HostError> {
        if self.fallback_peers.len() > 8 {
            return Err(HostError::Configuration);
        }
        let primary = (
            self.address,
            self.server_name.as_str(),
            self.server_pin.as_str(),
        );
        std::iter::once(primary)
            .chain(self.fallback_peers.iter().map(|peer| {
                (
                    peer.address,
                    peer.server_name.as_str(),
                    peer.server_pin.as_str(),
                )
            }))
            .map(|(address, server_name, certificate)| {
                Ok(PeerEndpoint {
                    address,
                    server_name: ServerName::try_from(server_name.to_owned())
                        .map_err(|_| HostError::Configuration)?,
                    certificate: hex(certificate)?,
                })
            })
            .collect()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SenderConfig {
    share: String,
    generation: u64,
    members: BTreeMap<String, ConfigRole>,
    bind: SocketAddr,
    client_pin: Option<String>,
    client_pins: Option<Vec<String>>,
    ca_der: PathBuf,
    certificate_der: PathBuf,
    private_key_der: PathBuf,
    event_key: PathBuf,
    migration: Option<MigrationConfig>,
}

pub struct Sender {
    address: SocketAddr,
    client_pins: Vec<CertificatePin>,
    server: Server,
    roster: Roster,
    signer: PeerSigningKey,
    store: Store,
    completed: BlobStore,
    write_lock: PathBuf,
    private_paths: Vec<PathBuf>,
    baseline_digest: Option<[u8; 32]>,
    generation_marker: PathBuf,
}

impl Sender {
    pub fn open(configuration: &Path, state_root: &Path) -> Result<Self, HostError> {
        let config: SenderConfig =
            serde_json::from_slice(&read_bounded(configuration, MAX_CONFIG_BYTES)?)
                .map_err(|_| HostError::Configuration)?;
        let roster = build_roster(&config.share, config.generation, config.members)?;
        let pins = match (config.client_pin, config.client_pins) {
            (Some(pin), None) => vec![pin],
            (None, Some(pins)) if !pins.is_empty() && pins.len() <= MAX_MEMBERS => pins,
            _ => return Err(HostError::Configuration),
        };
        let mut client_pins = pins
            .iter()
            .map(|pin| hex(pin))
            .collect::<Result<Vec<_>, _>>()?;
        client_pins.sort_unstable();
        if client_pins.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(HostError::Configuration);
        }
        let (roots, credentials) = read_credentials(
            configuration,
            &config.ca_der,
            &config.certificate_der,
            &config.private_key_der,
        )?;
        let material = read_bounded(
            &configuration
                .parent()
                .ok_or(HostError::Configuration)?
                .join(&config.event_key),
            32,
        )?;
        let seed: &[u8; 32] = material
            .as_slice()
            .try_into()
            .map_err(|_| HostError::Configuration)?;
        let signer = PeerSigningKey::from_bytes(seed);
        roster
            .check_writer(&signer.verifying_key().to_bytes())
            .map_err(|_| HostError::Configuration)?;
        let share_root = state_root.join(config.share.to_ascii_lowercase());
        let generation_marker = share_root.join("active-generation");
        let root = share_root.join(config.generation.to_string());
        fs::create_dir_all(&root).map_err(HostError::Io)?;
        let completed = BlobStore::open(root.join("files")).map_err(HostError::Io)?;
        let config_directory = configuration.parent().ok_or(HostError::Configuration)?;
        let (store, baseline_digest) = open_host_store(
            &root,
            &completed,
            &roster,
            config_directory,
            config.migration.as_ref(),
        )?;
        let server = Server::with_baseline(
            roots,
            credentials,
            roster.membership().share,
            roster.membership().generation,
            baseline_digest,
        )
        .map_err(HostError::Tls)?;
        let private_paths = [
            configuration.to_owned(),
            share_root,
            config_directory.join(config.private_key_der),
            config_directory.join(&config.event_key),
        ]
        .into_iter()
        .chain(
            config
                .migration
                .as_ref()
                .into_iter()
                .map(|migration| config_directory.join(&migration.bundle)),
        )
        .map(|path| fs::canonicalize(path).map_err(HostError::Io))
        .collect::<Result<_, _>>()?;
        let mut sender = Self {
            address: config.bind,
            client_pins,
            server,
            roster,
            signer,
            store,
            completed,
            write_lock: root.join("write.lock"),
            private_paths,
            baseline_digest,
            generation_marker,
        };
        activate_generation(
            &sender.generation_marker,
            config.generation,
            baseline_digest,
        )?;
        let _lock = sender.lock_state()?;
        sender.sweep_archives()?;
        Ok(sender)
    }

    fn lock_state(&self) -> Result<File, HostError> {
        let file = self.open_state_lock()?;
        file.lock().map_err(HostError::Io)?;
        self.check_generation()?;
        Ok(file)
    }

    fn check_generation(&self) -> Result<(), HostError> {
        check_generation(
            &self.generation_marker,
            self.roster.membership().generation,
            self.baseline_digest,
        )
    }

    fn open_state_lock(&self) -> Result<File, HostError> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.write_lock)
            .map_err(HostError::Io)
    }

    fn sweep_archives(&mut self) -> Result<(), HostError> {
        self.store
            .sweep_sender_archives(&self.completed, std::time::SystemTime::now())
            .map_err(HostError::Store)
    }

    pub fn publish(&mut self, source: &Path) -> Result<EventId, HostError> {
        let _lock = self.lock_state()?;
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(HostError::Configuration)?;
        publish_file(
            source,
            &mut self.store,
            &self.roster,
            &self.signer,
            &self.completed,
            None,
            name,
        )
        .map_err(HostError::Publish)
    }

    pub fn publish_tree(&mut self, root: &Path) -> Result<TreeSummary, HostError> {
        let _lock = self.lock_state()?;
        publish_tree(
            root,
            &mut self.store,
            &self.roster,
            &self.signer,
            &self.completed,
        )
        .map_err(HostError::Publish)
    }

    /// Keep application state and private credential material outside an
    /// automatically republished source tree.
    pub fn validate_source_root(&self, root: &Path) -> Result<PathBuf, HostError> {
        let root = fs::canonicalize(root).map_err(HostError::Io)?;
        if !root.is_dir()
            || self.private_paths.iter().any(|private| {
                private.starts_with(&root) || (private.is_dir() && root.starts_with(private))
            })
        {
            return Err(HostError::Configuration);
        }
        Ok(root)
    }

    pub fn refresh_tree(&mut self, root: &Path) -> Result<TreeSummary, HostError> {
        let _lock = self.lock_state()?;
        self.sweep_archives()?;
        refresh_tree(
            root,
            &mut self.store,
            &self.roster,
            &self.signer,
            &self.completed,
        )
        .map_err(HostError::Publish)
    }

    /// Issue a change for a single existing, non-conflicted shared file.
    /// The shared path uses slash-separated protocol components.
    pub fn revise(
        &mut self,
        shared_path: &str,
        source: Option<&Path>,
    ) -> Result<EventId, HostError> {
        let _lock = self.lock_state()?;
        let base = self.shared_head(shared_path, false)?;
        revise_file(
            source,
            &mut self.store,
            &self.roster,
            &self.signer,
            &self.completed,
            base,
        )
        .map_err(HostError::Publish)
    }

    pub fn relocate(&mut self, shared_path: &str, destination: &str) -> Result<EventId, HostError> {
        let _lock = self.lock_state()?;
        let base = self.shared_head(shared_path, false)?;
        let (parent, name) = match destination.rsplit_once('/') {
            Some((parent, name)) => {
                let event = self.shared_head(parent, true)?;
                let parent = self
                    .store
                    .replica()
                    .get(event)
                    .ok_or(HostError::Publish(PublishError::InvalidParent))?
                    .object;
                (Some(parent), name)
            }
            None => (None, destination),
        };
        relocate_file(
            &mut self.store,
            &self.roster,
            &self.signer,
            base,
            parent,
            name,
        )
        .map_err(HostError::Publish)
    }

    pub fn conflicts(&mut self) -> Result<Vec<Vec<Event>>, HostError> {
        self.store.refresh().map_err(HostError::Store)?;
        let replica = self.store.replica();
        Ok(replica
            .objects()
            .filter(|&object| replica.has_conflict(object))
            .map(|object| replica.heads(object).cloned().collect())
            .collect())
    }

    pub fn resolve_conflict(
        &mut self,
        choice: EventId,
        reviewed: BTreeSet<EventId>,
    ) -> Result<EventId, HostError> {
        let _lock = self.lock_state()?;
        resolve_file_conflict(
            &mut self.store,
            &self.roster,
            &self.signer,
            &self.completed,
            choice,
            reviewed,
        )
        .map_err(HostError::Publish)
    }

    fn shared_head(&mut self, shared_path: &str, directory: bool) -> Result<EventId, HostError> {
        let target: Vec<_> = shared_path
            .split('/')
            .map(collision_key)
            .collect::<Result<_, _>>()
            .map_err(|error| HostError::Publish(PublishError::InvalidName(error)))?;
        self.store.refresh().map_err(HostError::Store)?;

        {
            let replica = self.store.replica();
            let projection = Projection::new(replica);
            replica
                .objects()
                .find_map(|object| {
                    let mut heads = replica.heads(object);
                    let first = heads.next()?;
                    if heads.next().is_some()
                        || !first.change.entry().is_some_and(|entry| {
                            matches!(entry.kind, EntryKind::Directory) == directory
                        })
                    {
                        return None;
                    }
                    let path = projection.path(object).ok()?;
                    if path.len() == target.len()
                        && path
                            .iter()
                            .zip(&target)
                            .all(|(name, key)| collision_key(name).is_ok_and(|value| value == *key))
                    {
                        Some(first.id)
                    } else {
                        None
                    }
                })
                .ok_or(HostError::Publish(PublishError::StaleBase))
        }
    }

    pub fn bind(&self) -> Result<TcpListener, HostError> {
        self.check_generation()?;
        TcpListener::bind(self.address).map_err(HostError::Io)
    }

    /// Import signed peer changes and verified content into this sender's cache.
    pub fn catch_up(&mut self, configuration: &Path) -> Result<usize, HostError> {
        let config: Config =
            serde_json::from_slice(&read_bounded(configuration, MAX_CONFIG_BYTES)?)
                .map_err(|_| HostError::Configuration)?;
        let peers = config.endpoints()?;
        let scope = build_roster(&config.share, config.generation, config.members)?;
        if scope.membership() != self.roster.membership() {
            return Err(HostError::Configuration);
        }
        let (roots, credentials) = read_credentials(
            configuration,
            &config.ca_der,
            &config.certificate_der,
            &config.private_key_der,
        )?;
        let client = Client::with_baseline(
            roots,
            credentials,
            self.roster.membership().share,
            self.roster.membership().generation,
            self.baseline_digest,
        )
        .map_err(HostError::Tls)?;
        let _lock = self.lock_state()?;
        let chunks =
            BlobStore::open(self.write_lock.with_file_name("chunks")).map_err(HostError::Io)?;
        self.store.refresh().map_err(HostError::Store)?;
        let before = self.store.event_order().len();
        let mut first_error = None;
        let mut succeeded = false;
        for peer in &peers {
            match direct::pull_serving_cache(
                &client,
                peer,
                Duration::from_secs(15),
                &mut self.store,
                &self.roster,
                &chunks,
                &self.completed,
            ) {
                Ok(_) => succeeded = true,
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if !succeeded {
            return Err(first_error.map_or(HostError::Configuration, HostError::Direct));
        }
        Ok(self.store.event_order().len().saturating_sub(before))
    }

    /// Export migration data. Write the signed transition last so an
    /// interrupted export cannot be mistaken for a completed bundle.
    pub fn prepare_generation(
        &mut self,
        next_configuration: &Path,
        administrator_key: &Path,
        output: &Path,
    ) -> Result<usize, HostError> {
        let old = self.roster.membership();
        let roster = read_sender_roster(next_configuration)?;
        let next = roster.membership().generation;
        if roster.membership().share != old.share || old.generation.checked_add(1) != Some(next) {
            return Err(HostError::Configuration);
        }
        let material = read_bounded(administrator_key, 32)?;
        let seed = material
            .as_slice()
            .try_into()
            .map_err(|_| HostError::Configuration)?;
        let administrator = PeerSigningKey::from_bytes(seed);
        let key_id = administrator.verifying_key().to_bytes();
        if old.members.contains(&key_id) || roster.membership().members.contains(&key_id) {
            return Err(HostError::Configuration);
        }
        let _lock = self.lock_state()?;
        let checkpoint = self
            .store
            .generation_checkpoint(std::time::SystemTime::now())
            .map_err(HostError::Store)?;
        let bytes = checkpoint
            .encode()
            .map_err(StoreError::from)
            .map_err(HostError::Store)?;
        if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
            return Err(HostError::CheckpointTooLarge);
        }
        let transition = GenerationTransition {
            share: old.share,
            previous: old.generation,
            next,
            checkpoint: crate::sha256(&bytes),
            members: roster.roles().clone(),
        };
        let signature = transition
            .sign(&administrator)
            .map_err(HostError::Transition)?;
        transition
            .verify_checkpoint(&bytes, &self.roster)
            .map_err(HostError::Transition)?;

        fs::create_dir(output).map_err(HostError::Io)?;
        let staged = BlobStore::open(output.join("files")).map_err(HostError::Io)?;
        let count = checkpoint
            .stage_contents(&self.completed, &staged)
            .map_err(HostError::CheckpointData)?;
        write_bundle_file(output, "checkpoint.bin", &bytes)?;
        write_bundle_file(output, "transition.bin", &signature)?;
        Ok(count)
    }

    pub fn serve(&mut self, socket: TcpStream) -> Result<(), HostError> {
        self.check_generation()?;
        {
            let lock = self.open_state_lock()?;
            // A concurrent outbound pull may be waiting for this peer to serve.
            match lock.try_lock() {
                Ok(()) => self.sweep_archives()?,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => return Err(HostError::Io(error)),
            }
        }
        direct::serve(
            &self.server,
            socket,
            &self.client_pins,
            Duration::from_secs(15),
            &mut self.store,
            &self.roster,
            &self.completed,
        )
        .map_err(HostError::Direct)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ConfigRole {
    Reader,
    Writer,
}

#[derive(Debug)]
pub enum HostError {
    Configuration,
    Io(std::io::Error),
    Store(StoreError),
    Tls(TlsError),
    Direct(DirectError),
    Publish(PublishError),
    Transition(TransitionError),
    CheckpointData(CheckpointDataError),
    CheckpointTooLarge,
    Baseline(BaselineError),
    InactiveGeneration,
}

fn generation_marker_bytes(generation: u64, digest: [u8; 32]) -> [u8; 40] {
    let mut marker = [0; 40];
    marker[..8].copy_from_slice(&generation.to_be_bytes());
    marker[8..].copy_from_slice(&digest);
    marker
}

fn check_generation(
    marker: &Path,
    generation: u64,
    digest: Option<[u8; 32]>,
) -> Result<(), HostError> {
    match read_bounded(marker, 40) {
        Ok(active)
            if digest
                .is_some_and(|digest| active == generation_marker_bytes(generation, digest)) =>
        {
            Ok(())
        }
        Ok(_) => Err(HostError::InactiveGeneration),
        Err(HostError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Publish the first verified transition atomically. Concurrent attempts
/// must agree with the already activated checkpoint and generation.
fn activate_generation(
    marker: &Path,
    generation: u64,
    digest: Option<[u8; 32]>,
) -> Result<(), HostError> {
    check_generation(marker, generation, digest)?;
    let Some(digest) = digest else { return Ok(()) };
    let parent = marker.parent().ok_or(HostError::Configuration)?;
    let name = marker
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(HostError::Configuration)?;
    match write_bundle_file(parent, name, &generation_marker_bytes(generation, digest)) {
        Ok(()) => {
            #[cfg(unix)]
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(HostError::Io)?;
            Ok(())
        }
        Err(HostError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            check_generation(marker, generation, Some(digest))
        }
        Err(error) => Err(error),
    }
}

fn open_host_store(
    state: &Path,
    completed: &BlobStore,
    roster: &Roster,
    configuration_directory: &Path,
    migration: Option<&MigrationConfig>,
) -> Result<(Store, Option<[u8; 32]>), HostError> {
    let database = state.join("sync.db");
    let Some(migration) = migration else {
        let store = Store::open(database, roster.membership().clone()).map_err(HostError::Store)?;
        return Ok((store, None));
    };
    let previous = read_sender_roster(&configuration_directory.join(&migration.previous_server))?;
    let material = read_bounded(
        &configuration_directory.join(&migration.administrator_public_key),
        32,
    )?;
    let admin = ed25519_dalek::VerifyingKey::from_bytes(
        material
            .as_slice()
            .try_into()
            .map_err(|_| HostError::Configuration)?,
    )
    .map_err(|_| HostError::Configuration)?;
    let bundle = configuration_directory.join(&migration.bundle);
    let source = BlobStore::open_existing(bundle.join("files")).map_err(HostError::Io)?;
    let wire = read_bounded(&bundle.join("transition.bin"), MAX_TRANSITION_BYTES as u64)?;
    let bytes = read_bounded(&bundle.join("checkpoint.bin"), MAX_CHECKPOINT_BYTES)?;
    let baseline = GenerationBaseline::load(&wire, &bytes, &admin, &previous, &source)
        .map_err(HostError::Baseline)?;
    if !baseline.matches_roster(roster) {
        return Err(HostError::Configuration);
    }
    baseline
        .checkpoint()
        .stage_contents(&source, completed)
        .map_err(HostError::CheckpointData)?;
    let digest = baseline.transition_digest();
    let store = Store::open_with_baseline(database, baseline).map_err(HostError::Store)?;
    Ok((store, Some(digest)))
}

/// Check an incoming generation bundle against trusted configuration and a
/// separately supplied administrative public key. No state is activated.
pub fn verify_generation_bundle(
    previous_configuration: &Path,
    next_configuration: &Path,
    administrator_public_key: &Path,
    bundle: &Path,
) -> Result<usize, HostError> {
    let previous = read_sender_roster(previous_configuration)?;
    let next = read_sender_roster(next_configuration)?;
    let material = read_bounded(administrator_public_key, 32)?;
    let public_key = material
        .as_slice()
        .try_into()
        .map_err(|_| HostError::Configuration)?;
    let admin = ed25519_dalek::VerifyingKey::from_bytes(public_key)
        .map_err(|_| HostError::Configuration)?;
    let scope = previous.membership();
    if scope.members.contains(public_key) || next.membership().members.contains(public_key) {
        return Err(HostError::Configuration);
    }
    let wire = read_bounded(&bundle.join("transition.bin"), MAX_TRANSITION_BYTES as u64)?;
    let transition = GenerationTransition::verify(&wire, &admin, scope.share, scope.generation)
        .map_err(HostError::Transition)?;
    if next.membership().share != transition.share
        || next.membership().generation != transition.next
        || next.roles() != &transition.members
    {
        return Err(HostError::Configuration);
    }
    let bytes = read_bounded(&bundle.join("checkpoint.bin"), MAX_CHECKPOINT_BYTES)?;
    let checkpoint = transition
        .verify_checkpoint(&bytes, &previous)
        .map_err(HostError::Transition)?;
    let blobs = BlobStore::open_existing(bundle.join("files")).map_err(HostError::Io)?;
    checkpoint
        .verify_contents(&blobs)
        .map_err(HostError::CheckpointData)
}

fn read_sender_roster(configuration: &Path) -> Result<Roster, HostError> {
    let config: SenderConfig =
        serde_json::from_slice(&read_bounded(configuration, MAX_CONFIG_BYTES)?)
            .map_err(|_| HostError::Configuration)?;
    build_roster(&config.share, config.generation, config.members)
}

fn write_bundle_file(output: &Path, name: &str, bytes: &[u8]) -> Result<(), HostError> {
    let mut temporary = tempfile::NamedTempFile::new_in(output).map_err(HostError::Io)?;
    temporary.write_all(bytes).map_err(HostError::Io)?;
    temporary.as_file().sync_all().map_err(HostError::Io)?;
    temporary
        .persist_noclobber(output.join(name))
        .map_err(|error| HostError::Io(error.error))?;
    Ok(())
}

#[repr(C)]
#[derive(Default, Debug, Eq, PartialEq)]
pub struct PullSummary {
    pub events_added: u64,
    pub files_published: u64,
    pub files_present: u64,
    pub files_blocked: u64,
}

#[repr(C)]
#[derive(Default, Debug, Eq, PartialEq)]
pub struct PullSummaryV2 {
    pub events_added: u64,
    pub files_published: u64,
    pub files_present: u64,
    pub files_blocked: u64,
    pub files_updated: u64,
    pub files_removed: u64,
}

#[repr(C)]
#[derive(Default, Debug, Eq, PartialEq)]
pub struct PullSummaryV3 {
    pub sync: PullSummaryV2,
    pub protected_files: u64,
    pub protected_bytes: u64,
    pub protected_missing: u64,
}

impl From<PullSummaryV3> for PullSummaryV2 {
    fn from(report: PullSummaryV3) -> Self {
        report.sync
    }
}

impl From<PullSummaryV3> for PullSummary {
    fn from(report: PullSummaryV3) -> Self {
        report.sync.into()
    }
}

impl From<PullSummaryV2> for PullSummary {
    fn from(report: PullSummaryV2) -> Self {
        Self {
            events_added: report.events_added,
            files_published: report.files_published,
            files_present: report.files_present,
            files_blocked: report.files_blocked,
        }
    }
}

/// Synchronous host entry point. Run off the UI thread. Config and credential
/// files must be chosen from a trusted local source.
pub fn pull_from_config(
    configuration: &Path,
    state_root: &Path,
    destination: &Path,
) -> Result<PullSummaryV2, HostError> {
    pull_detailed(configuration, state_root, destination).map(Into::into)
}

pub fn pull_detailed(
    configuration: &Path,
    state_root: &Path,
    destination: &Path,
) -> Result<PullSummaryV3, HostError> {
    pull_with_originals(configuration, state_root, destination, &|| false)
        .map(|(summary, _)| summary)
}

pub(crate) fn pull_with_originals(
    configuration: &Path,
    state_root: &Path,
    destination: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<(PullSummaryV3, crate::archive::SyncDetails), HostError> {
    if cancelled() {
        return Err(HostError::Direct(DirectError::Cancelled));
    }
    let config: Config = serde_json::from_slice(&read_bounded(configuration, MAX_CONFIG_BYTES)?)
        .map_err(|_| HostError::Configuration)?;
    let peers = config.endpoints()?;
    let roster = build_roster(&config.share, config.generation, config.members)?;
    let reader = config
        .event_key
        .map(|name| {
            let directory = configuration.parent().ok_or(HostError::Configuration)?;
            let material = read_bounded(&directory.join(name), 32)?;
            let seed = material
                .as_slice()
                .try_into()
                .map_err(|_| HostError::Configuration)?;
            let key = PeerSigningKey::from_bytes(seed);
            if !roster.is_reader(key.verifying_key().as_bytes()) {
                return Err(HostError::Configuration);
            }
            Ok(key)
        })
        .transpose()?;
    let (roots, credentials) = read_credentials(
        configuration,
        &config.ca_der,
        &config.certificate_der,
        &config.private_key_der,
    )?;
    if !destination.is_dir() {
        return Err(HostError::Configuration);
    }
    let destination = destination.canonicalize().map_err(HostError::Io)?;
    let destination_id = crate::sha256(destination.as_os_str().as_encoded_bytes());
    let destination_id: String = destination_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let share_root = state_root.join(config.share.to_ascii_lowercase());
    let state_root = share_root
        .join(config.generation.to_string())
        .join(destination_id);
    fs::create_dir_all(&state_root).map_err(HostError::Io)?;
    let chunks = BlobStore::open(state_root.join("chunks")).map_err(HostError::Io)?;
    let completed = BlobStore::open(state_root.join("files")).map_err(HostError::Io)?;
    let configuration_directory = configuration.parent().ok_or(HostError::Configuration)?;
    let (mut store, baseline_digest) = open_host_store(
        &state_root,
        &completed,
        &roster,
        configuration_directory,
        config.migration.as_ref(),
    )?;
    let client = Client::with_baseline(
        roots,
        credentials,
        roster.membership().share,
        roster.membership().generation,
        baseline_digest,
    )
    .map_err(HostError::Tls)?;
    activate_generation(
        &share_root.join("active-generation"),
        config.generation,
        baseline_digest,
    )?;

    store
        .prune_archives(std::time::SystemTime::now())
        .map_err(HostError::Store)?;
    let mut summary = PullSummaryV2::default();
    let mut files = BTreeMap::new();
    let mut first_error = None;
    let mut succeeded = false;
    for (index, peer) in peers.iter().enumerate() {
        let report = direct::pull_with_reader(
            &client,
            peer,
            Duration::from_secs(15),
            &mut store,
            &roster,
            Destination {
                root: &destination,
                chunks: &chunks,
                completed: &completed,
            },
            // Confirmations are currently exchanged with the primary only.
            reader.as_ref().filter(|_| index == 0),
            cancelled,
        );
        match report {
            Ok(report) => {
                succeeded = true;
                summary.events_added = summary
                    .events_added
                    .saturating_add(report.events_added as u64);
                for (object, result) in report.files {
                    if result == FileResult::Present
                        && matches!(
                            files.get(&object),
                            Some(FileResult::Published | FileResult::Updated)
                        )
                    {
                        continue;
                    }
                    files.insert(object, result);
                }
            }
            Err(DirectError::Cancelled) => return Err(HostError::Direct(DirectError::Cancelled)),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if !succeeded {
        return Err(HostError::Direct(
            first_error.expect("at least one configured peer"),
        ));
    }
    for result in files.into_values() {
        match result {
            FileResult::Published => summary.files_published += 1,
            FileResult::Present => summary.files_present += 1,
            FileResult::Removed => summary.files_removed += 1,
            FileResult::Updated => summary.files_updated += 1,
            _ => summary.files_blocked += 1,
        }
    }
    store.refresh().map_err(HostError::Store)?;
    let originals = store
        .protected_originals(&completed)
        .map_err(HostError::Io)?;
    let protected = crate::archive::ProtectedStorage::from_originals(&originals);
    let conflicts = store.file_conflicts(&completed).map_err(HostError::Io)?;
    Ok((
        PullSummaryV3 {
            sync: summary,
            protected_files: protected.files,
            protected_bytes: protected.bytes,
            protected_missing: protected.missing,
        },
        crate::archive::SyncDetails {
            originals,
            conflicts,
        },
    ))
}

fn build_roster(
    share: &str,
    generation: u64,
    members: BTreeMap<String, ConfigRole>,
) -> Result<Roster, HostError> {
    if members.is_empty() || members.len() > MAX_MEMBERS || generation == 0 {
        return Err(HostError::Configuration);
    }
    let mut roles = BTreeMap::<PeerId, Role>::new();
    for (peer, role) in members {
        let peer = hex(&peer)?;
        let role = match role {
            ConfigRole::Reader => Role::Reader,
            ConfigRole::Writer => Role::Writer,
        };
        if roles.insert(peer, role).is_some() {
            return Err(HostError::Configuration);
        }
    }
    Ok(Roster::new(hex(share)?, generation, roles))
}

fn read_credentials(
    configuration: &Path,
    ca_path: &Path,
    certificate_path: &Path,
    key_path: &Path,
) -> Result<(RootCertStore, Credentials), HostError> {
    let directory = configuration.parent().ok_or(HostError::Configuration)?;
    let read = |path: &Path| read_bounded(&directory.join(path), MAX_CREDENTIAL_BYTES);
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(read(ca_path)?))
        .map_err(|_| HostError::Configuration)?;
    let credentials = Credentials {
        chain: vec![CertificateDer::from(read(certificate_path)?)],
        key: PrivateKeyDer::try_from(read(key_path)?).map_err(|_| HostError::Configuration)?,
    };
    Ok((roots, credentials))
}

fn hex<const N: usize>(value: &str) -> Result<[u8; N], HostError> {
    let bytes = value.as_bytes();
    if bytes.len() != N * 2 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(HostError::Configuration);
    }
    let mut decoded = [0; N];
    for (index, out) in decoded.iter_mut().enumerate() {
        *out = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| HostError::Configuration)?;
    }
    Ok(decoded)
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, HostError> {
    let file = File::open(path).map_err(HostError::Io)?;
    if !file.metadata().map_err(HostError::Io)?.is_file() {
        return Err(HostError::Configuration);
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(HostError::Io)?;
    if bytes.len() as u64 > max_bytes {
        return Err(HostError::Configuration);
    }
    Ok(bytes)
}
