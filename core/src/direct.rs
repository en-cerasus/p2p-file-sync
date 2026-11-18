use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use rustls::pki_types::ServerName;

use crate::{
    auth::{AuthError, PeerSigningKey, Roster},
    blob::BlobStore,
    local_target::{open_root, NewFileTarget},
    materialize::{FileTarget, MaterializeError},
    replica::{Change, EventId, Replica},
    retention::{Confirmation, PeerState},
    store::Store,
    sync::{sync_new_files, Destination, FileResult, SyncError, SyncReport},
    tls::{CertificatePin, Client, Server, TlsError},
    wire::{finish_session, send_confirmation, SessionProgress, ShareSession, WireError},
};

#[derive(Debug)]
pub enum DirectError {
    Socket(io::Error),
    Tls(TlsError),
    Sync(SyncError),
    Wire(WireError),
    Confirmation(AuthError),
}

/// A peer established by the host's trusted configuration. The endpoint
/// is independent of the DNS name validated by TLS.
pub struct PeerEndpoint {
    pub address: SocketAddr,
    pub server_name: ServerName<'static>,
    pub certificate: CertificatePin,
}

/// Run one bounded-I/O pull connection against a manually configured peer.
pub fn pull(
    client: &Client,
    peer: &PeerEndpoint,
    timeout: Duration,
    store: &mut Store,
    roster: &Roster,
    destination: Destination<'_>,
) -> Result<SyncReport, DirectError> {
    pull_with_reader(client, peer, timeout, store, roster, destination, None)
}

pub(crate) fn pull_with_reader(
    client: &Client,
    peer: &PeerEndpoint,
    timeout: Duration,
    store: &mut Store,
    roster: &Roster,
    destination: Destination<'_>,
    reader: Option<&PeerSigningKey>,
) -> Result<SyncReport, DirectError> {
    let socket = TcpStream::connect_timeout(&peer.address, timeout).map_err(DirectError::Socket)?;
    configure(&socket, timeout).map_err(DirectError::Socket)?;
    let mut connection = client
        .connect(socket, peer.server_name.clone(), peer.certificate)
        .map_err(DirectError::Tls)?;

    let root = destination.root;
    let report =
        sync_new_files(&mut connection, store, roster, destination).map_err(DirectError::Sync)?;
    if let Some(reader) = reader {
        confirm_applied(&mut connection, store, roster, reader, root, &report)?;
    }
    finish_session(&mut connection).map_err(DirectError::Wire)?;
    Ok(report)
}

fn confirm_applied(
    connection: &mut (impl io::Read + io::Write),
    store: &mut Store,
    roster: &Roster,
    reader: &PeerSigningKey,
    root: &std::path::Path,
    report: &SyncReport,
) -> Result<(), DirectError> {
    let peer = reader.verifying_key().to_bytes();
    if !roster.is_reader(&peer) {
        return Err(DirectError::Confirmation(AuthError::InvalidConfirmation));
    }
    let clock = store.replica().clock();
    let mut awaiting = BTreeMap::<_, BTreeSet<_>>::new();
    for protection in store.protections() {
        if protection
            .peer_states(clock)
            .any(|(member, state)| member == peer && state == PeerState::AwaitingConfirmation)
        {
            awaiting
                .entry(protection.object())
                .or_default()
                .insert(protection.successor());
        }
    }
    for &(object, ref result) in &report.files {
        let Some(awaiting) = awaiting.get(&object) else {
            continue;
        };
        if !matches!(
            result,
            FileResult::Published | FileResult::Updated | FileResult::Present | FileResult::Removed
        ) || store.replica().has_conflict(object)
        {
            continue;
        }
        let Some(applied) = store
            .applied_file(object)
            .map_err(|error| DirectError::Sync(SyncError::Materialize(error)))?
        else {
            continue;
        };
        let replica = store.replica();
        if !replica.heads(object).any(|head| head.id == applied.event) {
            continue;
        }
        let successors = applied_lineage(replica, applied.event, awaiting);
        if successors.is_empty() {
            continue;
        }
        let Some(path) = applied.path else { continue };
        let root = open_root(root).map_err(|error| DirectError::Sync(SyncError::Content(error)))?;
        let path: Vec<_> = path.iter().map(String::as_str).collect();
        let mut target = NewFileTarget::existing(&root, &path)
            .map_err(|error| DirectError::Sync(SyncError::Content(error)))?;
        if target.inspect(object).map_err(|error| {
            DirectError::Sync(SyncError::Materialize(MaterializeError::Io(error)))
        })? != applied.contents
        {
            continue;
        }
        for successor in successors {
            let confirmation = Confirmation {
                generation: roster.membership().generation,
                peer,
                object,
                successor,
                sent_through: 0,
            };
            let signed = roster
                .sign_confirmation(reader, &confirmation)
                .map_err(DirectError::Confirmation)?;
            send_confirmation(connection, &signed).map_err(DirectError::Wire)?;
            roster
                .receive_confirmation(store, &signed)
                .map_err(DirectError::Confirmation)?;
        }
    }
    Ok(())
}

/// Traverse the actual file-version bases. The causal clock also includes
/// observations of unrelated files and cannot prove they were materialized.
fn applied_lineage(replica: &Replica, head: EventId, awaiting: &BTreeSet<EventId>) -> Vec<EventId> {
    let mut pending = vec![head];
    let mut visited = BTreeSet::new();
    let mut confirmed = Vec::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Some(event) = replica.get(id) else {
            continue;
        };
        if awaiting.contains(&id) {
            confirmed.push(id);
        }
        match &event.change {
            Change::Create(_) => {}
            Change::Revise { base, .. } => pending.push(*base),
            Change::Resolve { bases, .. } => pending.extend(bases.iter().copied()),
        }
    }
    confirmed
}

/// Serve one accepted socket until the authenticated peer finishes.
/// Listener ownership and peer selection remain with the host.
pub fn serve(
    server: &Server,
    socket: TcpStream,
    peers: &[CertificatePin],
    timeout: Duration,
    store: &mut Store,
    roster: &Roster,
    completed: &BlobStore,
) -> Result<(), DirectError> {
    configure(&socket, timeout).map_err(DirectError::Socket)?;
    let mut connection = server
        .accept_registered(socket, peers)
        .map_err(DirectError::Tls)?;
    let mut session = ShareSession::default();
    while session
        .serve_next(&mut connection, store, completed, roster)
        .map_err(DirectError::Wire)?
        == SessionProgress::Continue
    {}
    Ok(())
}

fn configure(socket: &TcpStream, timeout: Duration) -> io::Result<()> {
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))
}
