use std::{
    net::{TcpListener, TcpStream},
    time::Duration,
};

use p2p_sync_core::{
    auth::{PeerSigningKey, Role, Roster},
    replica::{Change, Entry, EntryKind, Event, EventId, VersionVector},
    store::Store,
    tls::{certificate_pin, Client, Credentials, Server, TlsError},
    wire::{pull_page, serve_pull},
};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::{
    pki_types::{PrivatePkcs8KeyDer, ServerName},
    RootCertStore,
};

fn identities() -> (RootCertStore, Credentials, Credentials) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
    let issue = |usage| {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        params.extended_key_usages = vec![usage];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let cert = params.signed_by(&key, &issuer).unwrap();
        Credentials {
            chain: vec![cert.der().clone()],
            key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        }
    };
    let server = issue(ExtendedKeyUsagePurpose::ServerAuth);
    let client = issue(ExtendedKeyUsagePurpose::ClientAuth);
    let mut roots = RootCertStore::empty();
    roots.add(issuer.der().clone()).unwrap();
    (roots, server, client)
}

fn configure(stream: &TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn exchanges_signed_events_only_after_mutual_tls_and_pin_validation() {
    let (roots, server_credentials, client_credentials) = identities();
    let client_pin = certificate_pin(&client_credentials.chain[0]);
    let server_pin = certificate_pin(&server_credentials.chain[0]);
    let server = Server::new(roots.clone(), server_credentials).unwrap();
    let client = Client::new(roots, client_credentials).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let key = PeerSigningKey::from_bytes(&[4; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = || Roster::new([3; 16], 1, [(peer, Role::Writer)].into());
    let item = Event {
        id: EventId { peer, sequence: 1 },
        context: VersionVector::new(),
        object: [5; 16],
        change: Change::Create(Entry {
            parent: None,
            name: "song.tja".into(),
            kind: EntryKind::File([6; 32]),
        }),
    };
    let wire = roster().sign_event(&key, &item).unwrap();

    std::thread::scope(|threads| {
        let worker = threads.spawn(|| {
            let roster = roster();
            let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
            roster.receive(&mut store, &wire).unwrap();
            let (socket, _) = listener.accept().unwrap();
            configure(&socket);
            let mut secure = server.accept(socket, client_pin).unwrap();
            serve_pull(&mut secure, &mut store).unwrap();

            let (socket, _) = listener.accept().unwrap();
            configure(&socket);
            assert!(matches!(
                server.accept(socket, [0; 32]),
                Err(TlsError::UnexpectedCertificate)
            ));
        });

        let socket = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
        configure(&socket);
        let mut secure = client
            .connect(
                socket,
                ServerName::try_from("localhost").unwrap(),
                server_pin,
            )
            .unwrap();
        let roster = roster();
        let mut receiver = Store::open(":memory:", roster.membership().clone()).unwrap();
        assert_eq!(
            pull_page(&mut secure, &mut receiver, &roster)
                .unwrap()
                .0
                .added,
            1
        );
        assert_eq!(receiver.replica().get(item.id), Some(&item));

        let socket = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
        configure(&socket);
        assert!(matches!(
            client.connect(socket, ServerName::try_from("localhost").unwrap(), [0; 32]),
            Err(TlsError::UnexpectedCertificate)
        ));
        worker.join().unwrap();
    });
}
