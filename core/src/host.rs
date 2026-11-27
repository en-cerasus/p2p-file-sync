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
#[cfg(target_os = "ios")]
use serde::Serialize;

use crate::{
    auth::{AuthError, PeerSigningKey, Role, Roster},
    blob::{Blob, BlobStore},
    direct::{self, DirectError, PeerEndpoint},
    generation::{
        BaselineError, CheckpointDataError, GenerationBaseline, GenerationTransition,
        TransitionError, MAX_TRANSITION_BYTES,
    },
    local_target::{open_root, NewFileTarget},
    materialize::FileTarget,
    names::collision_key,
    projection::Projection,
    publish::{
        publish_directory_path, publish_file, publish_tree, refresh_tree, relocate_file,
        relocate_file_with_content, resolve_file_conflict, restore_archived_file, revise_file,
        store_file, PublishError, TreeSummary,
    },
    recovery::{self, RecoveryError},
    replica::{EntryKind, Event, EventId, PeerId},
    store::{Store, StoreError},
    sync::{Destination, FileResult},
    tls::{CertificatePin, Client, Credentials, Server, TlsError},
};

const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;
const MAX_MEMBERS: usize = 1024;
const MAX_CHECKPOINT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_RECOVERY_ANCHOR_BYTES: u64 = 1024 * 1024 + 256;

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
    connect_timeout_ms: Option<u64>,
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
            None,
        );
        std::iter::once(primary)
            .chain(self.fallback_peers.iter().map(|peer| {
                (
                    peer.address,
                    peer.server_name.as_str(),
                    peer.server_pin.as_str(),
                    peer.connect_timeout_ms,
                )
            }))
            .map(|(address, server_name, certificate, connect_timeout_ms)| {
                let connect_timeout = connect_timeout_ms
                    .map(|value| {
                        (100..=15_000)
                            .contains(&value)
                            .then(|| Duration::from_millis(value))
                            .ok_or(HostError::Configuration)
                    })
                    .transpose()?;
                Ok(PeerEndpoint {
                    address,
                    server_name: ServerName::try_from(server_name.to_owned())
                        .map_err(|_| HostError::Configuration)?,
                    certificate: hex(certificate)?,
                    connect_timeout,
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
    recovery_anchor: Option<RecoveryAnchorConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryAnchorConfig {
    file: PathBuf,
    peer: String,
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
        Self::open_internal(configuration, state_root, false)
    }

    fn open_internal(
        configuration: &Path,
        state_root: &Path,
        recovery_only: bool,
    ) -> Result<Self, HostError> {
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
        if !roster
            .roles()
            .contains_key(&signer.verifying_key().to_bytes())
        {
            return Err(HostError::Configuration);
        }
        let share_root = state_root.join(config.share.to_ascii_lowercase());
        let generation_marker = share_root.join("active-generation");
        let root = share_root.join(config.generation.to_string());
        fs::create_dir_all(&root).map_err(HostError::Io)?;
        let completed = BlobStore::open(root.join("files")).map_err(HostError::Io)?;
        let config_directory = configuration.parent().ok_or(HostError::Configuration)?;
        let (mut store, baseline_digest) = open_host_store(
            &root,
            &completed,
            &roster,
            config_directory,
            config.migration.as_ref(),
        )?;
        store
            .enable_event_witness(
                share_root.join("witness").join(config.generation.to_string()).join("sender"),
            )
            .map_err(HostError::Store)?;
        if recovery_only && config.recovery_anchor.is_none() {
            return Err(HostError::Configuration);
        }
        if let Some(anchor) = &config.recovery_anchor {
            let trusted_peer = hex::<32>(&anchor.peer)?;
            if trusted_peer == signer.verifying_key().to_bytes() {
                return Err(HostError::Configuration);
            }
            let path = fs::canonicalize(config_directory.join(&anchor.file)).map_err(HostError::Io)?;
            if path.starts_with(fs::canonicalize(state_root).map_err(HostError::Io)?) {
                return Err(HostError::Configuration);
            }
            let wire = read_bounded(&path, MAX_RECOVERY_ANCHOR_BYTES)?;
            match recovery::verify_anchor(&roster, &mut store, &wire, trusted_peer) {
                Ok(()) | Err(RecoveryError::Behind(_)) if recovery_only => {}
                Ok(()) => {}
                Err(error) => return Err(HostError::Recovery(error)),
            }
        }
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
        if recovery_only {
            return Ok(sender);
        }
        activate_generation(
            &sender.generation_marker,
            config.generation,
            baseline_digest,
        )?;
        let _lock = sender.lock_state()?;
        sender.sweep_archives()?;
        Ok(sender)
    }

    /// Import missing signed history before returning to normal startup.
    /// No source scan, publication, serving, or archive sweep runs while the
    /// local database remains behind the trusted anchor.
    pub fn recover_from_anchor(
        configuration: &Path,
        state_root: &Path,
        peers: &Path,
    ) -> Result<usize, HostError> {
        let mut sender = Self::open_internal(configuration, state_root, true)?;
        let added = sender.catch_up(peers)?;
        drop(sender);
        Self::open(configuration, state_root)?;
        Ok(added)
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
            false,
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

    /// Export a signed history frontier for a different trusted peer to hold.
    /// Existing output is never replaced. The caller must transfer it outside
    /// this device's restorable state before treating it as rollback evidence.
    pub fn export_recovery_anchor(&mut self, output: &Path) -> Result<(), HostError> {
        let _lock = self.lock_state()?;
        let wire = recovery::sign_anchor(&self.roster, &mut self.store, &self.signer)
            .map_err(HostError::Recovery)?;
        let parent = output.parent().ok_or(HostError::Configuration)?;
        let name = output.file_name().and_then(|name| name.to_str()).ok_or(HostError::Configuration)?;
        write_bundle_file(parent, name, &wire)
    }

    /// Fetch a signed anchor from the manually pinned primary peer and save
    /// it outside this device's state root without replacing an older file.
    pub fn fetch_recovery_anchor(
        &self,
        configuration: &Path,
        trusted_peer: PeerId,
        output: &Path,
    ) -> Result<(), HostError> {
        self.check_generation()?;
        if trusted_peer == self.signer.verifying_key().to_bytes()
            || !self.roster.membership().members.contains(&trusted_peer)
        {
            return Err(HostError::Configuration);
        }
        let config: Config = serde_json::from_slice(&read_bounded(configuration, MAX_CONFIG_BYTES)?)
            .map_err(|_| HostError::Configuration)?;
        let peer = config.endpoints()?.into_iter().next().ok_or(HostError::Configuration)?;
        let roster = build_roster(&config.share, config.generation, config.members)?;
        if roster.membership() != self.roster.membership() {
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
            roster.membership().share,
            roster.membership().generation,
            self.baseline_digest,
        ).map_err(HostError::Tls)?;
        let parent = output.parent().ok_or(HostError::Configuration)?;
        let parent = fs::canonicalize(parent).map_err(HostError::Io)?;
        let state_root = self.generation_marker.parent()
            .and_then(Path::parent).ok_or(HostError::Configuration)?;
        let state_root = fs::canonicalize(state_root).map_err(HostError::Io)?;
        if parent.starts_with(state_root) {
            return Err(HostError::Configuration);
        }
        let name = output.file_name().and_then(|name| name.to_str()).ok_or(HostError::Configuration)?;
        let wire = direct::fetch_recovery_anchor(
            &client, &peer, Duration::from_secs(15), &self.roster, trusted_peer,
        ).map_err(HostError::Direct)?;
        write_bundle_file(&parent, name, &wire)
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

    /// List locally issued, single-headed shared files whose exact source
    /// name is absent. A missing parent or inaccessible directory is not
    /// evidence of a deletion. The caller must still review each candidate.
    pub fn missing_source_files(
        &mut self,
        root: &Path,
    ) -> Result<Vec<(Vec<String>, EventId)>, HostError> {
        let _lock = self.lock_state()?;
        let root = self.validate_source_root(root)?;
        self.store.refresh().map_err(HostError::Store)?;
        let directory = open_root(&root).map_err(HostError::Io)?;
        let replica = self.store.replica();
        let projection = Projection::new(replica);
        let local_peer = self.signer.verifying_key().to_bytes();
        let mut missing = Vec::new();
        for object in replica.objects() {
            let mut heads = replica.heads(object);
            let Some(head) = heads.next() else { continue };
            if heads.next().is_some()
                || head.id.peer != local_peer
                || !head.change.entry().is_some_and(|entry| matches!(entry.kind, EntryKind::File(_)))
                || self.store.signed_event(head.id).is_none()
            {
                continue;
            }
            let Ok(path) = projection.path(object) else { continue };
            let target = match NewFileTarget::existing(&directory, &path) {
                Ok(target) => target,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(HostError::Io(error)),
            };
            if target.name_is_vacant().map_err(HostError::Io)? {
                missing.push((path.into_iter().map(str::to_owned).collect::<Vec<_>>(), head.id));
            }
        }
        missing.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(missing)
    }

    /// Publish a tombstone only for the version and missing source name the
    /// user reviewed. This leaves the physical source folder untouched.
    pub fn delete_missing_source_file(
        &mut self,
        root: &Path,
        shared_path: &str,
        reviewed: EventId,
    ) -> Result<EventId, HostError> {
        let _lock = self.lock_state()?;
        let root = self.validate_source_root(root)?;
        let current = self.shared_head(shared_path, false)?;
        if current != reviewed || current.peer != self.signer.verifying_key().to_bytes() {
            return Err(HostError::Publish(PublishError::StaleBase));
        }
        let object = self.store.replica().get(current).ok_or(HostError::Publish(PublishError::StaleBase))?.object;
        let path = Projection::new(self.store.replica()).path(object)
            .map_err(|_| HostError::Publish(PublishError::StaleBase))?;
        let directory = open_root(&root).map_err(HostError::Io)?;
        let target = NewFileTarget::existing(&directory, &path).map_err(HostError::Io)?;
        if !target.name_is_vacant().map_err(HostError::Io)? {
            return Err(HostError::Publish(PublishError::StaleBase));
        }
        revise_file(None, &mut self.store, &self.roster, &self.signer, &self.completed, current)
            .map_err(HostError::Publish)
    }

    /// Keep the reviewed file ID when the user selects its new physical file.
    /// Both the old vacancy and the destination bytes are checked before a
    /// signed move (possibly with changed contents) is published. No source
    /// file is modified by this call.
    pub fn relocate_missing_source_file(
        &mut self,
        root: &Path,
        shared_path: &str,
        reviewed: EventId,
        destination: &Path,
    ) -> Result<EventId, HostError> {
        let _lock = self.lock_state()?;
        let root = self.validate_source_root(root)?;
        let current = self.shared_head(shared_path, false)?;
        if current != reviewed || current.peer != self.signer.verifying_key().to_bytes() {
            return Err(HostError::Publish(PublishError::StaleBase));
        }
        let event = self.store.replica().get(current).ok_or(HostError::Publish(PublishError::StaleBase))?;
        let object = event.object;
        let Some(EntryKind::File(hash)) = event.change.entry().map(|entry| &entry.kind) else {
            return Err(HostError::Publish(PublishError::StaleBase));
        };
        let hash = *hash;
        let old_path = Projection::new(self.store.replica()).path(object)
            .map_err(|_| HostError::Publish(PublishError::StaleBase))?;
        let directory = open_root(&root).map_err(HostError::Io)?;
        let old = NewFileTarget::existing(&directory, &old_path).map_err(HostError::Io)?;
        if !old.name_is_vacant().map_err(HostError::Io)? {
            return Err(HostError::Publish(PublishError::StaleBase));
        }
        let destination = fs::canonicalize(destination).map_err(HostError::Io)?;
        let relative = destination.strip_prefix(&root).map_err(|_| HostError::Configuration)?;
        let components = relative.iter().map(|component| component.to_str().ok_or(HostError::Configuration))
            .collect::<Result<Vec<_>, _>>()?;
        if components.is_empty() {
            return Err(HostError::Configuration);
        }
        for component in &components {
            collision_key(component).map_err(|error| HostError::Publish(PublishError::InvalidName(error)))?;
        }
        let mut target = NewFileTarget::existing(&directory, &components).map_err(HostError::Io)?;
        let stored = self.completed.open_candidate(hash).map_err(HostError::Io)?;
        let expected = Blob { hash, length: stored.metadata().map_err(HostError::Io)?.len() };
        self.completed.open_verified(expected).map_err(HostError::Io)?;
        let observed = target.inspect(object).map_err(HostError::Io)?
            .ok_or(HostError::Publish(PublishError::StaleBase))?;
        let replacement = if observed == expected {
            observed
        } else {
            let copied = store_file(&destination, &self.completed).map_err(HostError::Publish)?;
            if copied != observed {
                return Err(HostError::Publish(PublishError::StaleBase));
            }
            copied
        };
        let (name, parents) = components.split_last().ok_or(HostError::Configuration)?;
        let parent = publish_directory_path(&root, parents, &mut self.store, &self.roster, &self.signer)
            .map_err(HostError::Publish)?;
        if !old.name_is_vacant().map_err(HostError::Io)?
            || target.inspect(object).map_err(HostError::Io)? != Some(replacement)
        {
            return Err(HostError::Publish(PublishError::StaleBase));
        }
        if replacement == expected {
            relocate_file(&mut self.store, &self.roster, &self.signer, current, parent, name)
                .map_err(HostError::Publish)
        } else {
            relocate_file_with_content(
                &mut self.store, &self.roster, &self.signer, &self.completed,
                current, parent, name, replacement,
            ).map_err(HostError::Publish)
        }
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

    pub fn history(&mut self) -> Result<serde_json::Value, HostError> {
        self.store.refresh().map_err(HostError::Store)?;
        let history = self
            .store
            .archive_choices(std::time::SystemTime::now())
            .map_err(HostError::Store)?;
        let format_id = |id: EventId| {
            let peer: String = id.peer.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("{peer}:{}", id.sequence)
        };
        Ok(serde_json::Value::Array(
            history
                .into_iter()
                .map(|(selection, archive)| {
                    let current: Vec<_> = self
                        .store
                        .replica()
                        .heads(archive.object)
                        .map(|head| format_id(head.id))
                        .collect();
                    serde_json::json!({
                        "archive": format_id(archive.event),
                        "id": selection.id,
                        "replaced_by": format_id(archive.replaced_by),
                        "current_heads": current,
                        "path": archive.path,
                        "bytes": archive.content.length,
                        "archived_at": archive.archived_at,
                    })
                })
                .collect(),
        ))
    }

    pub fn protected_originals_report(&mut self) -> Result<serde_json::Value, HostError> {
        self.store.refresh().map_err(HostError::Store)?;
        let originals = self
            .store
            .protected_originals(&self.completed)
            .map_err(HostError::Io)?;
        let storage = crate::archive::ProtectedStorage::from_originals(&originals);
        Ok(serde_json::json!({
            "files": storage.files,
            "bytes": storage.bytes,
            "missing": storage.missing,
            "originals": originals,
        }))
    }

    pub fn delete_history(
        &mut self,
        selected: &[crate::archive::ArchiveSelection],
    ) -> Result<usize, HostError> {
        let _lock = self.lock_state()?;
        self.store
            .delete_archives(&self.completed, selected, std::time::SystemTime::now())
            .map_err(HostError::Store)
    }

    pub fn restore_archive(
        &mut self,
        archived: EventId,
        reviewed: EventId,
    ) -> Result<EventId, HostError> {
        let _lock = self.lock_state()?;
        restore_archived_file(
            &mut self.store,
            &self.roster,
            &self.signer,
            &self.completed,
            archived,
            reviewed,
        )
        .map_err(HostError::Publish)
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
        self.catch_up_cancellable(configuration, &|| false)
    }

    pub fn catch_up_cancellable(
        &mut self,
        configuration: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<usize, HostError> {
        if cancelled() {
            return Err(HostError::Direct(DirectError::Cancelled));
        }
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
            if cancelled() {
                return Err(HostError::Direct(DirectError::Cancelled));
            }
            match direct::pull_serving_cache_cancellable(
                &client,
                peer,
                Duration::from_secs(15),
                &mut self.store,
                &self.roster,
                &chunks,
                &self.completed,
                cancelled,
            ) {
                Ok(_) => succeeded = true,
                Err(DirectError::Cancelled) => {
                    return Err(HostError::Direct(DirectError::Cancelled));
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if !succeeded {
            return Err(first_error.map_or(HostError::Configuration, HostError::Direct));
        }
        if cancelled() {
            return Err(HostError::Direct(DirectError::Cancelled));
        }
        Ok(self.store.event_order().len().saturating_sub(before))
    }

    /// Merge signed confirmations from this device's receiving state into
    /// its serving state. Both states must have the same trusted roster and
    /// generation; missing successor events are retried on a later import.
    pub fn import_local_confirmations(
        &mut self,
        configuration: &Path,
        receiver_state_root: &Path,
        destination: &Path,
    ) -> Result<usize, HostError> {
        let config: Config =
            serde_json::from_slice(&read_bounded(configuration, MAX_CONFIG_BYTES)?)
                .map_err(|_| HostError::Configuration)?;
        let roster = build_roster(&config.share, config.generation, config.members)?;
        if roster.membership() != self.roster.membership() || !destination.is_dir() {
            return Err(HostError::Configuration);
        }
        let destination = destination.canonicalize().map_err(HostError::Io)?;
        let destination_id = crate::sha256(destination.as_os_str().as_encoded_bytes());
        let destination_id: String = destination_id
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let source_root = receiver_state_root
            .join(config.share.to_ascii_lowercase())
            .join(config.generation.to_string())
            .join(destination_id);
        if !source_root.join("sync.db").is_file() {
            return Err(HostError::Configuration);
        }
        let source_completed = BlobStore::open(source_root.join("files")).map_err(HostError::Io)?;
        let config_directory = configuration.parent().ok_or(HostError::Configuration)?;
        let (source, baseline_digest) = open_host_store(
            &source_root,
            &source_completed,
            &roster,
            config_directory,
            config.migration.as_ref(),
        )?;
        if baseline_digest != self.baseline_digest {
            return Err(HostError::Configuration);
        }
        let _lock = self.lock_state()?;
        self.store.refresh().map_err(HostError::Store)?;
        let mut cursor = 0_i64;
        let mut imported = 0;
        loop {
            let page = source
                .signed_confirmations_page(cursor, 128)
                .map_err(HostError::Store)?;
            if page.is_empty() {
                break;
            }
            for (id, wire) in page {
                let confirmation = self
                    .roster
                    .verify_confirmation(&wire)
                    .map_err(HostError::Authentication)?;
                if self.store.replica().get(confirmation.successor).is_some() {
                    self.roster
                        .receive_confirmation(&mut self.store, &wire)
                        .map_err(HostError::Authentication)?;
                    imported += 1;
                }
                cursor = id;
            }
        }
        Ok(imported)
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
        // Another handle may have published while this listener was idle.
        // Read those commits before calculating the peer's event frontier.
        self.store.refresh().map_err(HostError::Store)?;
        {
            let lock = self.open_state_lock()?;
            // A concurrent outbound pull may be waiting for this peer to serve.
            match lock.try_lock() {
                Ok(()) => self.sweep_archives()?,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => return Err(HostError::Io(error)),
            }
        }
        direct::serve_signed(
            &self.server,
            socket,
            &self.client_pins,
            Duration::from_secs(15),
            &mut self.store,
            &self.roster,
            &self.completed,
            &self.signer,
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
    Authentication(AuthError),
    Io(std::io::Error),
    Store(StoreError),
    Recovery(RecoveryError),
    Tls(TlsError),
    Direct(DirectError),
    Publish(PublishError),
    Transition(TransitionError),
    CheckpointData(CheckpointDataError),
    CheckpointTooLarge,
    Baseline(BaselineError),
    InactiveGeneration,
    #[cfg(target_os = "ios")]
    Materialize(crate::materialize::MaterializeError),
}

#[cfg(target_os = "ios")]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PendingLocalKind {
    Update,
    Delete,
    Move,
}

#[cfg(target_os = "ios")]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingLocalFile {
    pub object: String,
    pub kind: PendingLocalKind,
    pub source: Vec<String>,
    pub destination: Option<Vec<String>>,
}

#[cfg(target_os = "ios")]
struct LocalReceiverLayout {
    roster: Roster,
    migration: Option<MigrationConfig>,
    generation: u64,
    destination: PathBuf,
    share_root: PathBuf,
    state: PathBuf,
    witness: PathBuf,
}

#[cfg(target_os = "ios")]
fn local_receiver_layout(
    configuration: &Path,
    state_root: &Path,
    destination: &Path,
) -> Result<LocalReceiverLayout, HostError> {
    let config: Config = serde_json::from_slice(&read_bounded(configuration, MAX_CONFIG_BYTES)?)
        .map_err(|_| HostError::Configuration)?;
    let roster = build_roster(&config.share, config.generation, config.members)?;
    let destination = destination.canonicalize().map_err(HostError::Io)?;
    if !destination.is_dir() {
        return Err(HostError::Configuration);
    }
    let destination_id: String = crate::sha256(destination.as_os_str().as_encoded_bytes())
        .iter().map(|byte| format!("{byte:02x}")).collect();
    let share_root = state_root.join(config.share.to_ascii_lowercase());
    let state = share_root.join(config.generation.to_string()).join(&destination_id);
    let witness = share_root.join("witness").join(config.generation.to_string()).join(&destination_id);
    Ok(LocalReceiverLayout {
        roster, migration: config.migration, generation: config.generation,
        destination, share_root, state, witness,
    })
}

#[cfg(target_os = "ios")]
fn open_local_receiver(
    configuration: &Path,
    layout: LocalReceiverLayout,
) -> Result<(Store, BlobStore, PathBuf), HostError> {
    let completed = BlobStore::open(layout.state.join("files")).map_err(HostError::Io)?;
    let (mut store, digest) = open_host_store(
        &layout.state, &completed, &layout.roster,
        configuration.parent().ok_or(HostError::Configuration)?,
        layout.migration.as_ref(),
    )?;
    check_generation(&layout.share_root.join("active-generation"), layout.generation, digest, false)?;
    store.enable_event_witness(layout.witness).map_err(HostError::Store)?;
    store.refresh().map_err(HostError::Store)?;
    Ok((store, completed, layout.destination))
}

#[cfg(target_os = "ios")]
fn pending_local_file(store: &Store, intent: &crate::materialize::FileIntent,
                      staged_path: Option<Vec<String>>)
    -> Result<Option<PendingLocalFile>, HostError> {
    let applied = store.applied_file(intent.object).map_err(HostError::Materialize)?;
    let Some(applied) = applied.filter(|entry| entry.contents == intent.previous) else {
        return Ok(None);
    };
    let Some(source) = applied.path else { return Ok(None) };
    if source.is_empty() { return Ok(None) }
    let (kind, destination) = if intent.desired.is_none() {
        if staged_path.as_ref() != Some(&source) { return Ok(None) }
        (PendingLocalKind::Delete, None)
    } else {
        let Some(path) = staged_path else { return Ok(None) };
        if path.is_empty() { return Ok(None) }
        if path == source {
            (PendingLocalKind::Update, Some(path))
        } else {
            (PendingLocalKind::Move, Some(path))
        }
    };
    if intent.previous.is_none() { return Ok(None) }
    let object = intent.object.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(Some(PendingLocalFile { object, kind, source, destination }))
}

#[cfg(target_os = "ios")]
pub fn pending_local_files(configuration: &Path, state: &Path, destination: &Path)
    -> Result<Vec<PendingLocalFile>, HostError> {
    let layout = local_receiver_layout(configuration, state, destination)?;
    if !layout.state.exists() && !layout.witness.exists() {
        return Ok(Vec::new());
    }
    let (store, _, _) = open_local_receiver(configuration, layout)?;
    let pending = store.pending_files_with_paths().map_err(HostError::Materialize)?;
    let files: Result<Vec<_>, _> = pending.iter()
        .map(|(intent, path)| pending_local_file(&store, intent, path.clone())).collect();
    Ok(files?.into_iter().flatten().collect())
}

/// The caller must hold NSFileCoordinator write access for every URL in the
/// reviewed item for the duration of this call.
#[cfg(target_os = "ios")]
pub fn apply_local_file(configuration: &Path, state: &Path, destination: &Path,
                        reviewed: &PendingLocalFile)
    -> Result<crate::materialize::Recovery, HostError> {
    use crate::local_target::{AppleExistingTarget, AppleMoveTarget};
    let layout = local_receiver_layout(configuration, state, destination)?;
    let (mut store, blobs, root) = open_local_receiver(configuration, layout)?;
    let object = hex::<16>(&reviewed.object)?;
    let (intent, path) = store.pending_files_with_paths().map_err(HostError::Materialize)?
        .into_iter().find(|(intent, _)| intent.object == object)
        .ok_or(HostError::Configuration)?;
    if pending_local_file(&store, &intent, path)?.as_ref() != Some(reviewed) {
        return Err(HostError::Configuration);
    }
    let result = match reviewed.kind {
        PendingLocalKind::Update => store.recover_updated_file_with(&blobs, object, |path, intent| {
            let components: Vec<_> = path.iter().map(String::as_str).collect();
            AppleExistingTarget::open(&open_root(&root)?, &components, intent)
        }),
        PendingLocalKind::Delete => store.recover_deleted_file_with(&blobs, object, |path, intent| {
            let components: Vec<_> = path.iter().map(String::as_str).collect();
            AppleExistingTarget::open(&open_root(&root)?, &components, intent)
        }),
        PendingLocalKind::Move => store.recover_moved_file_with(&blobs, object, |old, new, intent, _| {
            let before: Vec<_> = old.iter().map(String::as_str).collect();
            let after: Vec<_> = new.iter().map(String::as_str).collect();
            AppleMoveTarget::open(&open_root(&root)?, &before, &after, intent)
        }),
    };
    let outcome = result.map_err(HostError::Materialize)?;
    if outcome == crate::materialize::Recovery::Superseded {
        store.retire_superseded_apple(&blobs, &root, object).map_err(HostError::Materialize)?;
    }
    Ok(outcome)
}

#[cfg(target_os = "ios")]
pub fn local_receiver_details(configuration: &Path, state: &Path, destination: &Path)
    -> Result<(PullSummaryV3, crate::archive::SyncDetails), HostError> {
    let layout = local_receiver_layout(configuration, state, destination)?;
    let (mut store, completed, _) = open_local_receiver(configuration, layout)?;
    let (protected, details) = receiver_details(&mut store, &completed)?;
    Ok((PullSummaryV3 {
        sync: PullSummaryV2::default(),
        protected_files: protected.files,
        protected_bytes: protected.bytes,
        protected_missing: protected.missing,
    }, details))
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
    allow_new_marker: bool,
) -> Result<(), HostError> {
    match read_bounded(marker, 40) {
        Ok(active)
            if digest
                .is_some_and(|digest| active == generation_marker_bytes(generation, digest)) =>
        {
            Ok(())
        }
        Ok(_) => Err(HostError::InactiveGeneration),
        Err(HostError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = marker.parent().ok_or(HostError::Configuration)?;
            // A removed marker must not reactivate an older generation while
            // a newer generation's state is still present on this device.
            for child in fs::read_dir(parent).map_err(HostError::Io)? {
                let child = child.map_err(HostError::Io)?;
                if child.file_type().map_err(HostError::Io)?.is_dir()
                    && child
                        .file_name()
                        .to_str()
                        .and_then(|name| name.parse::<u64>().ok())
                        .is_some_and(|value| value > generation)
                {
                    return Err(HostError::InactiveGeneration);
                }
            }
            if digest.is_some() && !allow_new_marker {
                Err(HostError::InactiveGeneration)
            } else {
                Ok(())
            }
        }
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
    check_generation(marker, generation, digest, true)?;
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
            check_generation(marker, generation, Some(digest), false)
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
    let witness = share_root.join("witness").join(config.generation.to_string()).join(&destination_id);
    let state_root = share_root
        .join(config.generation.to_string())
        .join(&destination_id);
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
    store.enable_event_witness(witness).map_err(HostError::Store)?;
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
    for peer in &peers {
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
            reader.as_ref(),
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
    let (protected, details) = receiver_details(&mut store, &completed)?;
    Ok((
        PullSummaryV3 {
            sync: summary,
            protected_files: protected.files,
            protected_bytes: protected.bytes,
            protected_missing: protected.missing,
        },
        details,
    ))
}

fn receiver_details(store: &mut Store, completed: &BlobStore)
    -> Result<(crate::archive::ProtectedStorage, crate::archive::SyncDetails), HostError> {
    store.refresh().map_err(HostError::Store)?;
    let originals = store.protected_originals(completed).map_err(HostError::Io)?;
    let protected = crate::archive::ProtectedStorage::from_originals(&originals);
    let conflicts = store.file_conflicts(completed).map_err(HostError::Io)?;
    let history = store.archives(std::time::SystemTime::now()).map_err(HostError::Store)?;
    let (history_bytes, history_missing) =
        crate::archive::history_storage(&history, completed).map_err(HostError::Io)?;
    Ok((protected, crate::archive::SyncDetails {
        originals, conflicts, history, history_bytes, history_missing,
    }))
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
