use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, ServerName},
    RootCertStore,
};
use serde::Deserialize;

use crate::{
    auth::{Role, Roster},
    blob::BlobStore,
    direct::{self, DirectError, PeerEndpoint},
    replica::PeerId,
    store::{Store, StoreError},
    sync::{Destination, FileResult},
    tls::{Client, Credentials, TlsError},
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
}

#[repr(C)]
#[derive(Default, Debug, Eq, PartialEq)]
pub struct PullSummary {
    pub events_added: u64,
    pub files_published: u64,
    pub files_present: u64,
    pub files_blocked: u64,
}

/// Synchronous host entry point. Run off the UI thread. Config and credential
/// files must be chosen from a trusted local source.
pub fn pull_from_config(
    configuration: &Path,
    state_root: &Path,
    destination: &Path,
) -> Result<PullSummary, HostError> {
    let config: Config = serde_json::from_slice(&read_bounded(configuration, MAX_CONFIG_BYTES)?)
        .map_err(|_| HostError::Configuration)?;
    let share = hex::<16>(&config.share)?;
    if config.members.is_empty() || config.members.len() > MAX_MEMBERS || config.generation == 0 {
        return Err(HostError::Configuration);
    }
    let server_pin = hex::<32>(&config.server_pin)?;
    let server_name =
        ServerName::try_from(config.server_name).map_err(|_| HostError::Configuration)?;
    let mut roles = BTreeMap::<PeerId, Role>::new();
    for (peer, role) in config.members {
        let peer = hex(&peer)?;
        let role = match role {
            ConfigRole::Reader => Role::Reader,
            ConfigRole::Writer => Role::Writer,
        };
        if roles.insert(peer, role).is_some() {
            return Err(HostError::Configuration);
        }
    }
    let roster = Roster::new(share, config.generation, roles);
    let directory = configuration.parent().ok_or(HostError::Configuration)?;
    let read_credential = |path: &Path| read_bounded(&directory.join(path), MAX_CREDENTIAL_BYTES);
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(read_credential(&config.ca_der)?))
        .map_err(|_| HostError::Configuration)?;
    let credentials = Credentials {
        chain: vec![CertificateDer::from(read_credential(
            &config.certificate_der,
        )?)],
        key: PrivateKeyDer::try_from(read_credential(&config.private_key_der)?)
            .map_err(|_| HostError::Configuration)?,
    };
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
    let mut summary = PullSummary {
        events_added: report.events_added as u64,
        ..Default::default()
    };
    for (_, result) in report.files {
        match result {
            FileResult::Published => summary.files_published += 1,
            FileResult::Present => summary.files_present += 1,
            _ => summary.files_blocked += 1,
        }
    }
    Ok(summary)
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
