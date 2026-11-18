use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
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
    names::collision_key,
    projection::Projection,
    publish::{publish_file, publish_tree, refresh_tree, revise_file, PublishError, TreeSummary},
    replica::{EntryKind, EventId, PeerId},
    store::{Store, StoreError},
    sync::{Destination, FileResult},
    tls::{CertificatePin, Client, Credentials, Server, TlsError},
};

const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;
const MAX_MEMBERS: usize = 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    share: String,
    generation: u64,
    members: BTreeMap<String, ConfigRole>,
    address: SocketAddr,
    server_name: String,
    server_pin: String,
    ca_der: PathBuf,
    certificate_der: PathBuf,
    private_key_der: PathBuf,
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
}

pub struct Sender {
    address: SocketAddr,
    client_pins: Vec<CertificatePin>,
    server: Server,
    roster: Roster,
    signer: PeerSigningKey,
    store: Store,
    completed: BlobStore,
    private_paths: Vec<PathBuf>,
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
        let server = Server::new(roots, credentials).map_err(HostError::Tls)?;
        let root = state_root
            .join(config.share.to_ascii_lowercase())
            .join(config.generation.to_string());
        fs::create_dir_all(&root).map_err(HostError::Io)?;
        let completed = BlobStore::open(root.join("files")).map_err(HostError::Io)?;
        let store = Store::open(root.join("sync.db"), roster.membership().clone())
            .map_err(HostError::Store)?;
        let config_directory = configuration.parent().ok_or(HostError::Configuration)?;
        let private_paths = [
            configuration.to_owned(),
            root,
            config_directory.join(config.private_key_der),
            config_directory.join(&config.event_key),
        ]
        .into_iter()
        .map(|path| fs::canonicalize(path).map_err(HostError::Io))
        .collect::<Result<_, _>>()?;
        Ok(Self {
            address: config.bind,
            client_pins,
            server,
            roster,
            signer,
            store,
            completed,
            private_paths,
        })
    }

    pub fn publish(&mut self, source: &Path) -> Result<EventId, HostError> {
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
        let target: Vec<_> = shared_path
            .split('/')
            .map(collision_key)
            .collect::<Result<_, _>>()
            .map_err(|error| HostError::Publish(PublishError::InvalidName(error)))?;
        self.store.refresh().map_err(HostError::Store)?;

        let base = {
            let replica = self.store.replica();
            let projection = Projection::new(replica);
            replica
                .objects()
                .find_map(|object| {
                    let mut heads = replica.heads(object);
                    let first = heads.next()?;
                    if heads.next().is_some()
                        || !first
                            .change
                            .entry()
                            .is_some_and(|entry| matches!(entry.kind, EntryKind::File(_)))
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
                .ok_or(HostError::Publish(PublishError::StaleBase))?
        };

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

    pub fn bind(&self) -> Result<TcpListener, HostError> {
        TcpListener::bind(self.address).map_err(HostError::Io)
    }

    pub fn serve(&mut self, socket: TcpStream) -> Result<(), HostError> {
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
    pull_with_originals(configuration, state_root, destination).map(|(summary, _)| summary)
}

pub(crate) fn pull_with_originals(
    configuration: &Path,
    state_root: &Path,
    destination: &Path,
) -> Result<(PullSummaryV3, Vec<crate::archive::ProtectedOriginal>), HostError> {
    let config: Config = serde_json::from_slice(&read_bounded(configuration, MAX_CONFIG_BYTES)?)
        .map_err(|_| HostError::Configuration)?;
    let roster = build_roster(&config.share, config.generation, config.members)?;
    let server_pin = hex(&config.server_pin)?;
    let server_name =
        ServerName::try_from(config.server_name).map_err(|_| HostError::Configuration)?;
    let (roots, credentials) = read_credentials(
        configuration,
        &config.ca_der,
        &config.certificate_der,
        &config.private_key_der,
    )?;
    let client = Client::new(roots, credentials).map_err(HostError::Tls)?;
    let peer = PeerEndpoint {
        address: config.address,
        server_name,
        certificate: server_pin,
    };

    if !destination.is_dir() {
        return Err(HostError::Configuration);
    }
    let destination = destination.canonicalize().map_err(HostError::Io)?;
    let destination_id = crate::sha256(destination.as_os_str().as_encoded_bytes());
    let destination_id: String = destination_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let state_root = state_root
        .join(config.share.to_ascii_lowercase())
        .join(config.generation.to_string())
        .join(destination_id);
    fs::create_dir_all(&state_root).map_err(HostError::Io)?;
    let chunks = BlobStore::open(state_root.join("chunks")).map_err(HostError::Io)?;
    let completed = BlobStore::open(state_root.join("files")).map_err(HostError::Io)?;
    let mut store = Store::open(state_root.join("sync.db"), roster.membership().clone())
        .map_err(HostError::Store)?;

    store
        .prune_archives(std::time::SystemTime::now())
        .map_err(HostError::Store)?;
    let report = direct::pull(
        &client,
        &peer,
        Duration::from_secs(15),
        &mut store,
        &roster,
        Destination {
            root: &destination,
            chunks: &chunks,
            completed: &completed,
        },
    )
    .map_err(HostError::Direct)?;
    let mut summary = PullSummaryV2 {
        events_added: report.events_added as u64,
        ..Default::default()
    };
    for (_, result) in report.files {
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
    Ok((
        PullSummaryV3 {
            sync: summary,
            protected_files: protected.files,
            protected_bytes: protected.bytes,
            protected_missing: protected.missing,
        },
        originals,
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
