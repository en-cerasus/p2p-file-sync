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
    materialize::FileTarget,
    projection::Projection,
    replica::{Change, EntryKind, EventId, ObjectId, PeerId, Replica},
    retention::{Confirmation, PeerState},
    store::Store,
    sync::{sync_cancellable, sync_serving_cache, Destination, FileResult, SyncError, SyncReport},
    tls::{CertificatePin, Client, Server, TlsError},
    wire::{
        exchange_confirmations, finish_session, pull_recovery_anchor, send_confirmation,
        SessionProgress, ShareSession,
        WireError,
    },
};

#[derive(Debug)]
pub enum DirectError {
    Socket(io::Error),
    Tls(TlsError),
    Sync(SyncError),
    Wire(WireError),
    Confirmation(AuthError),
    Cancelled,
}

/// A peer established by the host's trusted configuration. The endpoint
/// is independent of the DNS name validated by TLS.
pub struct PeerEndpoint {
    pub address: SocketAddr,
    pub server_name: ServerName<'static>,
    pub certificate: CertificatePin,
    pub connect_timeout: Option<Duration>,
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
    pull_with_reader(
        client,
        peer,
        timeout,
        store,
        roster,
        destination,
        None,
        &|| false,
    )
}

pub(crate) fn pull_with_reader(
    client: &Client,
    peer: &PeerEndpoint,
    timeout: Duration,
    store: &mut Store,
    roster: &Roster,
    destination: Destination<'_>,
    reader: Option<&PeerSigningKey>,
    cancelled: &dyn Fn() -> bool,
) -> Result<SyncReport, DirectError> {
    if cancelled() {
        return Err(DirectError::Cancelled);
    }
    let stream = connect(client, peer, timeout)?;
    pull_authenticated_stream(stream, store, roster, destination, reader, cancelled)
}

fn pull_authenticated_stream(
    stream: impl io::Read + io::Write,
    store: &mut Store,
    roster: &Roster,
    destination: Destination<'_>,
    reader: Option<&PeerSigningKey>,
    cancelled: &dyn Fn() -> bool,
) -> Result<SyncReport, DirectError> {
    if cancelled() {
        return Err(DirectError::Cancelled);
    }
    let result = (|| {
        let mut connection = CancellableStream {
            inner: stream,
            cancelled,
        };
        let root = destination.root;
        let report = sync_cancellable(&mut connection, store, roster, destination, cancelled)
            .map_err(DirectError::Sync)?;
        exchange_confirmations(&mut connection, store, roster).map_err(DirectError::Wire)?;
        if let Some(reader) = reader {
            confirm_applied(&mut connection, store, roster, reader, root, &report)?;
        }
        finish_session(&mut connection).map_err(DirectError::Wire)?;
        Ok(report)
    })();
    if cancelled() {
        Err(DirectError::Cancelled)
    } else {
        result
    }
}

/// Pull through a supplied byte stream, such as a future WebRTC DataChannel
/// stream. The existing mutual TLS and certificate pin remain mandatory.
pub fn pull_over_stream(
    client: &Client,
    stream: impl io::Read + io::Write,
    peer: &PeerEndpoint,
    store: &mut Store,
    roster: &Roster,
    destination: Destination<'_>,
) -> Result<SyncReport, DirectError> {
    let authenticated = client.connect(stream, peer.server_name.clone(), peer.certificate)
        .map_err(DirectError::Tls)?;
    pull_authenticated_stream(authenticated, store, roster, destination, None, &|| false)
}

struct CancellableStream<'a, S> {
    inner: S,
    cancelled: &'a dyn Fn() -> bool,
}

impl<S> CancellableStream<'_, S> {
    fn check(&self) -> io::Result<()> {
        if (self.cancelled)() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "sync cancelled",
            ));
        }
        Ok(())
    }
}

impl<S: io::Read> io::Read for CancellableStream<'_, S> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.read(bytes)
    }
}

impl<S: io::Write> io::Write for CancellableStream<'_, S> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check()?;
        self.inner.flush()
    }
}

pub(crate) fn pull_serving_cache_cancellable(
    client: &Client,
    peer: &PeerEndpoint,
    timeout: Duration,
    store: &mut Store,
    roster: &Roster,
    chunks: &BlobStore,
    completed: &BlobStore,
    cancelled: &dyn Fn() -> bool,
) -> Result<usize, DirectError> {
    if cancelled() {
        return Err(DirectError::Cancelled);
    }
    let result = (|| {
        let mut connection = CancellableStream {
            inner: connect(client, peer, timeout)?,
            cancelled,
        };
        let added = sync_serving_cache(&mut connection, store, roster, chunks, completed)
            .map_err(DirectError::Sync)?;
        exchange_confirmations(&mut connection, store, roster).map_err(DirectError::Wire)?;
        finish_session(&mut connection).map_err(DirectError::Wire)?;
        Ok(added)
    })();
    if cancelled() {
        Err(DirectError::Cancelled)
    } else {
        result
    }
}

fn connect(
    client: &Client,
    peer: &PeerEndpoint,
    timeout: Duration,
) -> Result<impl io::Read + io::Write + use<>, DirectError> {
    let socket = TcpStream::connect_timeout(&peer.address, peer.connect_timeout.unwrap_or(timeout))
        .map_err(DirectError::Socket)?;
    configure(&socket, timeout).map_err(DirectError::Socket)?;
    client
        .connect(socket, peer.server_name.clone(), peer.certificate)
        .map_err(DirectError::Tls)
}

/// Fetch an authenticated recovery anchor without changing the local store.
pub fn fetch_recovery_anchor(
    client: &Client,
    peer: &PeerEndpoint,
    timeout: Duration,
    roster: &Roster,
    trusted_peer: PeerId,
) -> Result<Vec<u8>, DirectError> {
    let mut connection = connect(client, peer, timeout)?;
    let anchor = pull_recovery_anchor(&mut connection, roster, trusted_peer)
        .map_err(DirectError::Wire)?;
    finish_session(&mut connection).map_err(DirectError::Wire)?;
    Ok(anchor)
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
    let results: BTreeMap<_, _> = report
        .files
        .iter()
        .map(|(object, result)| (*object, result))
        .collect();
    for (object, awaiting) in awaiting {
        let result = results.get(&object).copied();
        if result.is_some_and(|result| {
            !matches!(
                result,
                FileResult::Published
                    | FileResult::Updated
                    | FileResult::Present
                    | FileResult::Removed
            )
        }) || store.replica().has_conflict(object)
        {
            continue;
        }
        let applied = store
            .applied_file(object)
            .map_err(|error| DirectError::Sync(SyncError::Materialize(error)))?;
        let replica = store.replica();
        let root = open_root(root).map_err(|error| DirectError::Sync(SyncError::Content(error)))?;
        let head = if let Some(applied) = applied {
            if !replica.heads(object).any(|head| head.id == applied.event)
                || (result.is_none() && applied.contents.is_some())
            {
                continue;
            }
            let Some(path) = applied.path else { continue };
            let path: Vec<_> = path.iter().map(String::as_str).collect();
            let Ok(mut target) = NewFileTarget::existing(&root, &path) else {
                continue;
            };
            if !target
                .inspect(object)
                .is_ok_and(|actual| actual == applied.contents)
            {
                continue;
            }
            applied.event
        } else {
            let Some((head, path)) = unapplied_deletion_path(replica, object) else {
                continue;
            };
            let Ok(target) = NewFileTarget::existing(&root, &path) else {
                continue;
            };
            if !target.name_is_vacant().unwrap_or(false) {
                continue;
            }
            head
        };
        for successor in applied_lineage(replica, head, &awaiting) {
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

fn unapplied_deletion_path(replica: &Replica, object: ObjectId) -> Option<(EventId, Vec<&str>)> {
    let mut heads = replica.heads(object);
    let head = heads.next()?;
    if heads.next().is_some() {
        return None;
    }
    let Change::Revise { base, entry: None } = &head.change else {
        return None;
    };
    let entry = replica.get(*base)?.change.entry()?;
    let projection = Projection::new(replica);
    if !matches!(entry.kind, EntryKind::File(_)) || !projection.vacant(entry.parent, &entry.name) {
        return None;
    }
    let mut path = match entry.parent {
        Some(parent) => projection.path(parent).ok()?,
        None => Vec::new(),
    };
    path.push(&entry.name);
    Some((head.id, path))
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
    serve_over_stream(server, socket, peers, store, roster, completed)
}

/// Host-owned serving path that can also answer signed recovery requests.
pub fn serve_signed(
    server: &Server,
    socket: TcpStream,
    peers: &[CertificatePin],
    timeout: Duration,
    store: &mut Store,
    roster: &Roster,
    completed: &BlobStore,
    signer: &PeerSigningKey,
) -> Result<(), DirectError> {
    configure(&socket, timeout).map_err(DirectError::Socket)?;
    serve_over_stream_signed(server, socket, peers, store, roster, completed, Some(signer))
}

/// Serve a supplied byte stream with the same TLS peer checks as TCP.
pub fn serve_over_stream(
    server: &Server,
    stream: impl io::Read + io::Write,
    peers: &[CertificatePin],
    store: &mut Store,
    roster: &Roster,
    completed: &BlobStore,
) -> Result<(), DirectError> {
    serve_over_stream_signed(server, stream, peers, store, roster, completed, None)
}

fn serve_over_stream_signed(
    server: &Server,
    stream: impl io::Read + io::Write,
    peers: &[CertificatePin],
    store: &mut Store,
    roster: &Roster,
    completed: &BlobStore,
    signer: Option<&PeerSigningKey>,
) -> Result<(), DirectError> {
    let mut connection = server
        .accept_registered(stream, peers)
        .map_err(DirectError::Tls)?;
    let mut session = ShareSession::default();
    while session
        .serve_next_signed(&mut connection, store, completed, roster, signer)
        .map_err(DirectError::Wire)?
        == SessionProgress::Continue
    {}
    Ok(())
}

fn configure(socket: &TcpStream, timeout: Duration) -> io::Result<()> {
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        io::{Cursor, Read, Write},
    };

    #[test]
    fn cancellation_stops_io_without_consuming_more_bytes() {
        let cancelled = Cell::new(false);
        let mut stream = CancellableStream {
            inner: Cursor::new(vec![1, 2, 3]),
            cancelled: &|| cancelled.get(),
        };
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [1]);
        cancelled.set(true);
        assert_eq!(
            stream.read_exact(&mut byte).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            stream.write_all(&[9]).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(stream.inner.position(), 1);
        assert_eq!(stream.inner.into_inner(), [1, 2, 3]);
    }
}
