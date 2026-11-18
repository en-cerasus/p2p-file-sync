use std::{
    io::{self, Read, Write},
    sync::Arc,
};

use rustls::{
    client::Resumption,
    crypto::ring::default_provider,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName},
    server::WebPkiClientVerifier,
    version::TLS13,
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};

const ALPN: &[u8] = b"p2p-file-sync/1";

pub type CertificatePin = [u8; 32];

pub struct Credentials {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

#[derive(Debug)]
pub enum TlsError {
    Handshake(io::Error),
    Configuration(rustls::Error),
    ClientVerifier(rustls::server::VerifierBuilderError),
    MissingPeerCertificate,
    UnexpectedCertificate,
    UnexpectedProtocol,
}

impl From<rustls::Error> for TlsError {
    fn from(error: rustls::Error) -> Self {
        Self::Configuration(error)
    }
}

pub fn certificate_pin(der: &CertificateDer<'_>) -> CertificatePin {
    crate::sha256(der.as_ref())
}

pub struct Client {
    config: Arc<ClientConfig>,
}

impl Client {
    pub fn new(roots: RootCertStore, credentials: Credentials) -> Result<Self, TlsError> {
        let mut config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_protocol_versions(&[&TLS13])?
            .with_root_certificates(roots)
            .with_client_auth_cert(credentials.chain, credentials.key)?;
        config.alpn_protocols = vec![ALPN.to_vec()];
        config.resumption = Resumption::disabled();
        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// The supplied pin must come from the current share's trusted peer
    /// configuration. A verified CA chain alone is insufficient.
    pub fn connect(
        &self,
        mut stream: impl Read + Write,
        name: ServerName<'static>,
        peer: CertificatePin,
    ) -> Result<impl Read + Write, TlsError> {
        let mut connection = ClientConnection::new(self.config.clone(), name)?;
        while connection.is_handshaking() {
            connection
                .complete_io(&mut stream)
                .map_err(TlsError::Handshake)?;
        }
        verify_peer(
            connection.alpn_protocol(),
            connection.peer_certificates(),
            &[peer],
        )?;
        Ok(StreamOwned::new(connection, stream))
    }
}

pub struct Server {
    config: Arc<ServerConfig>,
}

impl Server {
    pub fn new(roots: RootCertStore, credentials: Credentials) -> Result<Self, TlsError> {
        let provider = Arc::new(default_provider());
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()
                .map_err(TlsError::ClientVerifier)?;
        let mut config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&TLS13])?
            .with_client_cert_verifier(verifier)
            .with_single_cert(credentials.chain, credentials.key)?;
        config.alpn_protocols = vec![ALPN.to_vec()];
        config.send_tls13_tickets = 0;
        Ok(Self {
            config: Arc::new(config),
        })
    }

    pub fn accept(
        &self,
        stream: impl Read + Write,
        peer: CertificatePin,
    ) -> Result<impl Read + Write, TlsError> {
        self.accept_registered(stream, &[peer])
    }

    pub fn accept_registered(
        &self,
        mut stream: impl Read + Write,
        peers: &[CertificatePin],
    ) -> Result<impl Read + Write, TlsError> {
        let mut connection = ServerConnection::new(self.config.clone())?;
        while connection.is_handshaking() {
            connection
                .complete_io(&mut stream)
                .map_err(TlsError::Handshake)?;
        }
        verify_peer(
            connection.alpn_protocol(),
            connection.peer_certificates(),
            peers,
        )?;
        Ok(StreamOwned::new(connection, stream))
    }
}

fn verify_peer(
    alpn: Option<&[u8]>,
    chain: Option<&[CertificateDer<'_>]>,
    pins: &[CertificatePin],
) -> Result<(), TlsError> {
    if alpn != Some(ALPN) {
        return Err(TlsError::UnexpectedProtocol);
    }
    let leaf = chain
        .and_then(|chain| chain.first())
        .ok_or(TlsError::MissingPeerCertificate)?;
    if !pins.contains(&certificate_pin(leaf)) {
        return Err(TlsError::UnexpectedCertificate);
    }
    Ok(())
}
