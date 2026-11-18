use std::{
    io,
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use rustls::pki_types::ServerName;

use crate::{
    auth::Roster,
    blob::BlobStore,
    store::Store,
    sync::{sync_new_files, Destination, SyncError, SyncReport},
    tls::{CertificatePin, Client, Server, TlsError},
    wire::{finish_session, SessionProgress, ShareSession, WireError},
};

#[derive(Debug)]
pub enum DirectError {
    Socket(io::Error),
    Tls(TlsError),
    Sync(SyncError),
    Wire(WireError),
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
    let socket = TcpStream::connect_timeout(&peer.address, timeout).map_err(DirectError::Socket)?;
    configure(&socket, timeout).map_err(DirectError::Socket)?;
    let mut connection = client
        .connect(socket, peer.server_name.clone(), peer.certificate)
        .map_err(DirectError::Tls)?;

    let report =
        sync_new_files(&mut connection, store, roster, destination).map_err(DirectError::Sync)?;
    finish_session(&mut connection).map_err(DirectError::Wire)?;
    Ok(report)
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
