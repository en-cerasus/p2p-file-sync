use std::{
    net::{TcpListener, TcpStream},
    time::Duration,
};

use p2p_sync_core::{
    auth::{PeerSigningKey, Role, Roster},
    blob::{Blob, BlobStore},
    direct::{self, PeerEndpoint},
    replica::{Change, Entry, EntryKind, Event, EventId, VersionVector},
    sha256,
    store::Store,
    sync::{Destination, FileResult},
    tls::{certificate_pin, Client, Credentials, Server, TlsError},
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
    let data = b"TITLE:Loopback\nBPM:120\n";
    let blob = Blob {
        hash: sha256(data),
        length: data.len() as u64,
    };
    let directory = Event {
        id: EventId { peer, sequence: 1 },
        context: VersionVector::new(),
        object: [7; 16],
        change: Change::Create(Entry {
            parent: None,
            name: "譜面".into(),
            kind: EntryKind::Directory,
        }),
    };
    let directory_wire = roster().sign_event(&key, &directory).unwrap();
    let item = Event {
        id: EventId { peer, sequence: 2 },
        context: [(peer, 1)].into(),
        object: [5; 16],
        change: Change::Create(Entry {
            parent: Some(directory.object),
            name: "song.tja".into(),
            kind: EntryKind::File(blob.hash),
        }),
    };
    let wire = roster().sign_event(&key, &item).unwrap();

    std::thread::scope(|threads| {
        let worker = threads.spawn(|| {
            let roster = roster();
            let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
            roster.receive(&mut store, &directory_wire).unwrap();
            roster.receive(&mut store, &wire).unwrap();
            let (socket, _) = listener.accept().unwrap();
            configure(&socket);
            let root = tempfile::tempdir().unwrap();
            let blobs = BlobStore::open(root.path()).unwrap();
            blobs.put(blob, &data[..]).unwrap();
            direct::serve(
                &server,
                socket,
                client_pin,
                Duration::from_secs(5),
                &mut store,
                &roster,
                &blobs,
            )
            .unwrap();

            let (socket, _) = listener.accept().unwrap();
            configure(&socket);
            assert!(matches!(
                server.accept(socket, [0; 32]),
                Err(TlsError::UnexpectedCertificate)
            ));
        });

        let endpoint = PeerEndpoint {
            address: addr,
            server_name: ServerName::try_from("localhost").unwrap(),
            certificate: server_pin,
        };
        let roster = roster();
        let mut receiver = Store::open(":memory:", roster.membership().clone()).unwrap();
        let folder = tempfile::tempdir().unwrap();
        let cache = BlobStore::open(folder.path().join("chunks")).unwrap();
        let completed = BlobStore::open(folder.path().join("blobs")).unwrap();
        let destination = folder.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        let report = direct::pull(
            &client,
            &endpoint,
            Duration::from_secs(5),
            &mut receiver,
            &roster,
            Destination {
                root: &destination,
                chunks: &cache,
                completed: &completed,
            },
        )
        .unwrap();
        assert_eq!(report.events_added, 2);
        assert_eq!(report.files, [(item.object, FileResult::Published)]);
        assert_eq!(receiver.replica().get(item.id), Some(&item));
        assert_eq!(
            std::fs::read(destination.join("譜面/song.tja")).unwrap(),
            data
        );

        let socket = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).unwrap();
        configure(&socket);
        assert!(matches!(
            client.connect(socket, ServerName::try_from("localhost").unwrap(), [0; 32]),
            Err(TlsError::UnexpectedCertificate)
        ));
        worker.join().unwrap();
    });
}
