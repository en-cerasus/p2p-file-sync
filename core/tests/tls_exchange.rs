use std::{
    net::{TcpListener, TcpStream},
    time::Duration,
};

use p2p_sync_core::{
    auth::{PeerSigningKey, Role, Roster},
    blob::{Blob, BlobStore},
    direct,
    host::{PullSummary, PullSummaryV3},
    p2p_sync_pull_v4,
    replica::{Change, Entry, EntryKind, Event, EventId, VersionVector},
    sha256,
    store::Store,
    tls::{certificate_pin, Client, Credentials, Server, TlsError},
};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::{
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName},
    RootCertStore,
};

fn identities() -> (
    RootCertStore,
    CertificateDer<'static>,
    Credentials,
    Credentials,
) {
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
    (roots, issuer.der().clone(), server, client)
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
    let (roots, ca_der, server_credentials, client_credentials) = identities();
    let client_pin = certificate_pin(&client_credentials.chain[0]);
    let server_pin = certificate_pin(&server_credentials.chain[0]);
    let server = Server::new(roots.clone(), server_credentials, [3; 16], 1).unwrap();
    let folder = tempfile::tempdir().unwrap();
    let config = folder.path().join("direct.json");
    let state = folder.path().join("state");
    let destination = folder.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(folder.path().join("ca.der"), ca_der.as_ref()).unwrap();
    std::fs::write(
        folder.path().join("client.der"),
        client_credentials.chain[0].as_ref(),
    )
    .unwrap();
    std::fs::write(
        folder.path().join("key.der"),
        client_credentials.key.secret_der(),
    )
    .unwrap();
    let client = Client::new(roots, client_credentials, [3; 16], 1).unwrap();
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
                &[client_pin],
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
            for _ in 0..2 {
                let (socket, _) = listener.accept().unwrap();
                configure(&socket);
                assert!(matches!(
                    server.accept(socket, client_pin),
                    Err(TlsError::Handshake(_) | TlsError::UnexpectedProtocol)
                ));
            }
        });

        let setup = serde_json::json!({
            "share": encode_hex(&[3; 16]),
            "generation": 1,
            "members": { (encode_hex(&peer)): "writer" },
            "address": addr.to_string(),
            "server_name": "localhost",
            "server_pin": encode_hex(&server_pin),
            "ca_der": "ca.der",
            "certificate_der": "client.der",
            "private_key_der": "key.der"
        });
        std::fs::write(&config, setup.to_string()).unwrap();
        let [config, state, destination_path] =
            [&config, &state, &destination].map(|path| path.to_str().unwrap().as_bytes());
        let mut report = PullSummary::default();
        extern "C" fn cancelled(context: *mut std::ffi::c_void) -> u8 {
            unsafe { *context.cast::<u8>() }
        }
        let mut stop = 1_u8;
        let mut extended = PullSummaryV3::default();
        extended.sync.events_added = 999;
        let status = unsafe {
            p2p_sync_core::p2p_sync_pull_v5(
                config.as_ptr(),
                config.len(),
                state.as_ptr(),
                state.len(),
                destination_path.as_ptr(),
                destination_path.len(),
                &mut extended,
                None,
                (&mut stop as *mut u8).cast(),
                Some(cancelled),
            )
        };
        assert_eq!(status, 7);
        assert_eq!(extended.sync.events_added, 999);
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        extern "C" fn copy_details(
            context: *mut std::ffi::c_void,
            bytes: *const u8,
            length: usize,
        ) {
            unsafe {
                (*context.cast::<Vec<u8>>())
                    .extend_from_slice(std::slice::from_raw_parts(bytes, length));
            }
        }
        let mut json = Vec::<u8>::new();
        let status = unsafe {
            p2p_sync_core::p2p_sync_pull_v6(
                config.as_ptr(),
                config.len(),
                state.as_ptr(),
                state.len(),
                destination_path.as_ptr(),
                destination_path.len(),
                &mut extended,
                Some(copy_details),
                (&mut json as *mut Vec<u8>).cast(),
                None,
            )
        };
        report.events_added = extended.sync.events_added;
        report.files_published = extended.sync.files_published;
        report.files_blocked = extended.sync.files_blocked;
        assert_eq!(status, 0);
        let details: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(details["originals"], serde_json::json!([]));
        assert_eq!(details["conflicts"], serde_json::json!([]));
        assert_eq!(details["history"], serde_json::json!([]));
        assert_eq!(details["history_bytes"], 0);
        assert_eq!(details["history_missing"], 0);
        assert_eq!(report.events_added, 2);
        assert_eq!(report.files_published, 1);
        assert_eq!(report.files_blocked, 0);
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
        for (share, generation) in [([3; 16], 2), ([6; 16], 1)] {
            let mut wrong = setup.clone();
            wrong["share"] = encode_hex(&share).into();
            wrong["generation"] = generation.into();
            let config = folder.path().join("wrong.json");
            std::fs::write(&config, wrong.to_string()).unwrap();
            assert!(matches!(
                p2p_sync_core::host::pull_from_config(
                    &config,
                    &folder.path().join("wrong-state"),
                    &destination,
                ),
                Err(p2p_sync_core::host::HostError::Direct(
                    direct::DirectError::Tls(_)
                ))
            ));
        }
        worker.join().unwrap();
    });
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn writer_ffi_history_restores_only_the_reviewed_current_version() {
    use p2p_sync_core::{host::Sender, p2p_sync_writer_history, p2p_sync_writer_restore};
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    assert!(std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8765", "127.0.0.1:8765"])
        .output()
        .unwrap()
        .status
        .success());
    let config = setup.join("sender/server.json");
    let state = root.path().join("writer-state");
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"first").unwrap();
    let mut writer = Sender::open(&config, &state).unwrap();
    writer.publish(&source).unwrap();
    std::fs::write(&source, b"second").unwrap();
    writer.revise("song.tja", Some(&source)).unwrap();
    drop(writer);
    extern "C" fn capture(context: *mut std::ffi::c_void, data: *const u8, length: usize) {
        unsafe {
            (*context.cast::<Vec<u8>>())
                .extend_from_slice(std::slice::from_raw_parts(data, length));
        }
    }
    let config_bytes = config.to_str().unwrap().as_bytes();
    let state_bytes = state.to_str().unwrap().as_bytes();
    let mut json = Vec::new();
    let status = unsafe {
        p2p_sync_writer_history(
            config_bytes.as_ptr(),
            config_bytes.len(),
            state_bytes.as_ptr(),
            state_bytes.len(),
            Some(capture),
            (&mut json as *mut Vec<u8>).cast(),
        )
    };
    assert_eq!(status, 0);
    let history: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let archive = history[0]["archive"].as_str().unwrap().as_bytes();
    let current = history[0]["current_heads"][0].as_str().unwrap().as_bytes();
    let restore = |reviewed: &[u8]| unsafe {
        p2p_sync_writer_restore(
            config_bytes.as_ptr(),
            config_bytes.len(),
            state_bytes.as_ptr(),
            state_bytes.len(),
            archive.as_ptr(),
            archive.len(),
            reviewed.as_ptr(),
            reviewed.len(),
        )
    };
    assert_eq!(restore(current), 0);
    assert_ne!(restore(current), 0);
    assert_eq!(restore(b"invalid"), 1);
}

#[test]
fn writer_ffi_deletes_selected_history_but_keeps_unconfirmed_original() {
    use p2p_sync_core::{host::Sender, p2p_sync_writer_delete_history, p2p_sync_writer_history};
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    assert!(std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8766", "127.0.0.1:8766"])
        .output()
        .unwrap()
        .status
        .success());
    let config = setup.join("sender/server.json");
    let state = root.path().join("writer-state");
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"first").unwrap();
    let mut writer = Sender::open(&config, &state).unwrap();
    writer.publish(&source).unwrap();
    for content in [b"second".as_slice(), b"third".as_slice()] {
        std::fs::write(&source, content).unwrap();
        writer.revise("song.tja", Some(&source)).unwrap();
    }
    drop(writer);
    extern "C" fn capture(context: *mut std::ffi::c_void, data: *const u8, length: usize) {
        unsafe {
            (*context.cast::<Vec<u8>>())
                .extend_from_slice(std::slice::from_raw_parts(data, length));
        }
    }
    let config_bytes = config.to_str().unwrap().as_bytes();
    let state_bytes = state.to_str().unwrap().as_bytes();
    let history = || {
        let mut json = Vec::new();
        assert_eq!(
            unsafe {
                p2p_sync_writer_history(
                    config_bytes.as_ptr(),
                    config_bytes.len(),
                    state_bytes.as_ptr(),
                    state_bytes.len(),
                    Some(capture),
                    (&mut json as *mut Vec<u8>).cast(),
                )
            },
            0
        );
        serde_json::from_slice::<Vec<serde_json::Value>>(&json).unwrap()
    };
    let delete = |selection: &[u8]| unsafe {
        p2p_sync_writer_delete_history(
            config_bytes.as_ptr(),
            config_bytes.len(),
            state_bytes.as_ptr(),
            state_bytes.len(),
            selection.as_ptr(),
            selection.len(),
        )
    };
    let before = history();
    assert_eq!(before.len(), 2);
    assert_eq!(delete(b"[]"), 1);
    let token = |entry: &serde_json::Value| {
        serde_json::json!({
            "id": entry["id"], "archive": entry["archive"],
            "replaced_by": entry["replaced_by"], "archived_at": entry["archived_at"],
        })
    };
    let mut stale = token(&before[0]);
    stale["archived_at"] = 0.into();
    assert_eq!(delete(&serde_json::to_vec(&[stale]).unwrap()), 5);
    assert_eq!(history().len(), 2);
    let first = token(&before[0]);
    assert_eq!(delete(&serde_json::to_vec(&[first.clone()]).unwrap()), 0);
    assert_eq!(history().len(), 1);
    assert_eq!(delete(&serde_json::to_vec(&[first]).unwrap()), 5);
    assert_eq!(
        delete(&serde_json::to_vec(&[token(&before[1])]).unwrap()),
        0
    );
    assert!(history().is_empty());
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    let share = settings["share"].as_str().unwrap().to_ascii_lowercase();
    let original = state
        .join(share)
        .join("1/files")
        .join(encode_hex(&sha256(b"first")));
    assert!(
        original.is_file(),
        "unconfirmed common original must survive history deletion"
    );
}

#[test]
fn writer_ffi_listener_serves_and_stops_on_request() {
    use p2p_sync_core::{
        host::{pull_from_config, Sender},
        p2p_sync_writer_serve,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    struct Context {
        ready: mpsc::Sender<String>,
        stop: AtomicBool,
    }
    extern "C" fn ready(context: *mut std::ffi::c_void, data: *const u8, length: usize) {
        let context = unsafe { &*context.cast::<Context>() };
        let bytes = unsafe { std::slice::from_raw_parts(data, length) };
        let _ = context
            .ready
            .send(std::str::from_utf8(bytes).unwrap().to_owned());
    }
    extern "C" fn cancelled(context: *mut std::ffi::c_void) -> u8 {
        let context = unsafe { &*context.cast::<Context>() };
        u8::from(context.stop.load(Ordering::Relaxed))
    }

    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    assert!(std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8767", "127.0.0.1:8767"])
        .output()
        .unwrap()
        .status
        .success());
    let config = setup.join("sender/server.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    settings["bind"] = "127.0.0.1:0".into();
    std::fs::write(&config, settings.to_string()).unwrap();
    let state = root.path().join("server-state");
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"served from GUI entry point").unwrap();
    Sender::open(&config, &state)
        .unwrap()
        .publish(&source)
        .unwrap();

    let (tx, rx) = mpsc::channel();
    let context = Context {
        ready: tx,
        stop: AtomicBool::new(false),
    };
    let config_bytes = config.to_str().unwrap().as_bytes();
    let state_bytes = state.to_str().unwrap().as_bytes();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| unsafe {
            p2p_sync_writer_serve(
                config_bytes.as_ptr(),
                config_bytes.len(),
                state_bytes.as_ptr(),
                state_bytes.len(),
                Some(ready),
                Some(cancelled),
                (&context as *const Context).cast_mut().cast(),
            )
        });
        let address = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let receiver = setup.join("receiver/direct.json");
        let mut settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receiver).unwrap()).unwrap();
        settings["address"] = address.into();
        std::fs::write(&receiver, settings.to_string()).unwrap();
        let destination = root.path().join("destination");
        std::fs::create_dir(&destination).unwrap();
        let result = pull_from_config(&receiver, &root.path().join("receiver-state"), &destination);
        context.stop.store(true, Ordering::Relaxed);
        assert_eq!(server.join().unwrap(), 7);
        let result = result.unwrap();
        assert_eq!(result.files_published, 1);
        assert_eq!(
            std::fs::read(destination.join("song.tja")).unwrap(),
            b"served from GUI entry point"
        );
    });
}

#[test]
fn local_receiver_confirmations_can_join_a_serving_cache() {
    use p2p_sync_core::{
        auth::{PeerSigningKey, Role, Roster},
        host::Sender,
        replica::{Change, Entry, EntryKind, Event, EventId, VersionVector},
        retention::Confirmation,
        store::Store,
    };
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    assert!(std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8765", "127.0.0.1:8765"])
        .status()
        .unwrap()
        .success());
    let writer = PeerSigningKey::from_bytes(
        &std::fs::read(setup.join("sender/event-key.bin"))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let reader = PeerSigningKey::from_bytes(
        &std::fs::read(setup.join("receiver/event-key.bin"))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let writer_id = writer.verifying_key().to_bytes();
    let reader_id = reader.verifying_key().to_bytes();
    let server_config = setup.join("sender/server.json");
    let direct_config = setup.join("receiver/direct.json");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&server_config).unwrap()).unwrap();
    let share = config["share"].as_str().unwrap();
    let share_bytes: [u8; 16] = (0..16)
        .map(|index| u8::from_str_radix(&share[index * 2..index * 2 + 2], 16).unwrap())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let roster = Roster::new(
        share_bytes,
        1,
        [(writer_id, Role::Writer), (reader_id, Role::Reader)].into(),
    );
    let object = [9; 16];
    let first = Event {
        id: EventId {
            peer: writer_id,
            sequence: 1,
        },
        context: VersionVector::new(),
        object,
        change: Change::Create(Entry {
            parent: None,
            name: "song.tja".into(),
            kind: EntryKind::File([3; 32]),
        }),
    };
    let next = Event {
        id: EventId {
            peer: writer_id,
            sequence: 2,
        },
        context: [(writer_id, 1)].into(),
        object,
        change: Change::Revise {
            base: first.id,
            entry: Some(Entry {
                parent: None,
                name: "song.tja".into(),
                kind: EntryKind::File([4; 32]),
            }),
        },
    };
    let server_state = root.path().join("server-state");
    let mut server = Sender::open(&server_config, &server_state).unwrap();
    let server_db = server_state.join(share).join("1/sync.db");
    let destination = root.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    let receive_state = root.path().join("receive-state");
    let destination_id = encode_hex(&sha256(
        destination
            .canonicalize()
            .unwrap()
            .as_os_str()
            .as_encoded_bytes(),
    ));
    let receive_db = receive_state
        .join(share)
        .join("1")
        .join(destination_id)
        .join("sync.db");
    std::fs::create_dir_all(receive_db.parent().unwrap()).unwrap();
    let mut source = Store::open(&receive_db, roster.membership().clone()).unwrap();
    let mut target = Store::open(&server_db, roster.membership().clone()).unwrap();
    for event in [&first, &next] {
        let wire = roster.sign_event(&writer, event).unwrap();
        roster.receive(&mut source, &wire).unwrap();
        roster.receive(&mut target, &wire).unwrap();
    }
    let confirmation = Confirmation {
        generation: 1,
        peer: reader_id,
        object,
        successor: next.id,
        sent_through: 0,
    };
    let wire = roster.sign_confirmation(&reader, &confirmation).unwrap();
    roster.receive_confirmation(&mut source, &wire).unwrap();
    drop(source);
    drop(target);
    assert_eq!(
        server
            .import_local_confirmations(&direct_config, &receive_state, &destination)
            .unwrap(),
        1
    );
    assert_eq!(
        server
            .import_local_confirmations(&direct_config, &receive_state, &destination)
            .unwrap(),
        1
    );
    let reopened = Store::open(&server_db, roster.membership().clone()).unwrap();
    assert_eq!(reopened.signed_confirmations_page(0, 128).unwrap().len(), 1);
    assert!(server
        .import_local_confirmations(&direct_config, &receive_state, root.path())
        .is_err());
}

#[test]
fn command_line_publish_can_be_served_to_two_registered_receive_hosts() {
    use p2p_sync_core::host::{pull_from_config, Sender};

    let root = tempfile::tempdir().unwrap();
    let path = root.path();
    let setup = path.join("setup");
    let setup_command = || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"));
        command
            .arg("setup")
            .arg(&setup)
            .args(["127.0.0.1:8765", "127.0.0.1:8765", "2"]);
        command
    };
    assert!(setup_command().output().unwrap().status.success());
    assert!(!setup_command().output().unwrap().status.success());
    let fingerprints = ["sender", "receiver-1", "receiver-2"]
        .map(|name| sha256(&std::fs::read(setup.join(name).join("event-key.bin")).unwrap()));
    assert_ne!(fingerprints[0], fingerprints[1]);
    assert_ne!(fingerprints[0], fingerprints[2]);
    assert_ne!(fingerprints[1], fingerprints[2]);
    assert_ne!(
        std::fs::read(setup.join("receiver-1/certificate.der")).unwrap(),
        std::fs::read(setup.join("receiver-2/certificate.der")).unwrap()
    );
    let second_direct: serde_json::Value =
        serde_json::from_slice(&std::fs::read(setup.join("receiver-2/direct.json")).unwrap())
            .unwrap();
    assert_eq!(
        second_direct["fallback_peers"][0]["server_pin"],
        encode_hex(&sha256(
            &std::fs::read(setup.join("receiver-1/certificate.der")).unwrap(),
        ))
    );
    assert_eq!(
        second_direct["fallback_peers"][0]["connect_timeout_ms"],
        1500
    );

    let server_config = setup.join("sender/server.json");
    let server_state = path.join("server-state");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&server_config).unwrap()).unwrap();
    settings["bind"] = "127.0.0.1:0".into();
    std::fs::write(&server_config, settings.to_string()).unwrap();
    let tree = path.join("share");
    std::fs::create_dir_all(tree.join("譜面")).unwrap();
    std::fs::create_dir(tree.join("空フォルダー")).unwrap();
    let source = tree.join("譜面/試験譜面.tja");
    let contents = b"TITLE:Command line published";
    std::fs::write(&source, contents).unwrap();
    let register = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("publish-tree")
        .arg(&server_config)
        .arg(&server_state)
        .arg(&tree)
        .output()
        .unwrap();
    assert!(
        register.status.success(),
        "{}",
        String::from_utf8_lossy(&register.stderr)
    );

    let mut sender = Sender::open(&server_config, &server_state).unwrap();
    let listener = sender.bind().unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::scope(|threads| {
        let serving = threads.spawn(move || {
            for _ in 0..3 {
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
            }
        });
        for index in 1..=2 {
            let receive_config = setup.join(format!("receiver-{index}/direct.json"));
            let mut settings: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&receive_config).unwrap()).unwrap();
            settings["address"] = address.to_string().into();
            if index == 2 {
                settings["fallback_peers"] = serde_json::json!([{
                    "address": settings["address"],
                    "server_name": settings["server_name"],
                    "server_pin": settings["server_pin"],
                }]);
                settings["address"] = "127.0.0.1:0".into();
            }
            std::fs::write(&receive_config, settings.to_string()).unwrap();
            let destination = path.join(format!("destination-{index}"));
            std::fs::create_dir(&destination).unwrap();
            let report =
                pull_from_config(&receive_config, &path.join("client-state"), &destination)
                    .unwrap();
            assert_eq!(report.files_published, 1);
            assert_eq!(
                std::fs::read(destination.join("譜面/試験譜面.tja")).unwrap(),
                contents
            );
            assert!(destination.join("空フォルダー").is_dir());
        }
        let direct = setup.join("receiver-1/direct.json");
        let mirror_config = setup.join("receiver-1/server.json");
        let mirror_state = path.join("mirror-state");
        let imported = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("catch-up")
            .arg(&mirror_config)
            .arg(&mirror_state)
            .arg(&direct)
            .output()
            .unwrap();
        assert!(
            imported.status.success(),
            "{}",
            String::from_utf8_lossy(&imported.stderr)
        );
        serving.join().unwrap();
        let rejected = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("publish-tree")
            .arg(&mirror_config)
            .arg(&mirror_state)
            .arg(&tree)
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        let mut mirror = Sender::open(&mirror_config, &mirror_state).unwrap();
        let listener = mirror.bind().unwrap();
        let relay_direct = setup.join("receiver-2/direct.json");
        let mut settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&relay_direct).unwrap()).unwrap();
        settings["address"] = listener.local_addr().unwrap().to_string().into();
        settings["server_pin"] = second_direct["fallback_peers"][0]["server_pin"].clone();
        settings.as_object_mut().unwrap().remove("fallback_peers");
        std::fs::write(&relay_direct, settings.to_string()).unwrap();
        let serving = threads.spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            mirror.serve(socket).unwrap();
        });
        let destination = path.join("mirror-destination");
        std::fs::create_dir(&destination).unwrap();
        let report = pull_from_config(
            &relay_direct,
            &path.join("mirror-client-state"),
            &destination,
        )
        .unwrap();
        assert_eq!(report.files_published, 1);
        assert_eq!(
            std::fs::read(destination.join("譜面/試験譜面.tja")).unwrap(),
            contents
        );
        serving.join().unwrap();
    });

    let mut legacy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&server_config).unwrap()).unwrap();
    let pin = legacy["client_pins"][0].clone();
    legacy.as_object_mut().unwrap().remove("client_pins");
    legacy["client_pin"] = pin;
    std::fs::write(&server_config, legacy.to_string()).unwrap();
    assert!(Sender::open(&server_config, &server_state).is_ok());

    let revised = path.join("revision.tja");
    std::fs::write(&revised, b"TITLE:Revision").unwrap();
    let change = |mode: &str, shared_path: &str, source: Option<&std::path::Path>| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"));
        command
            .arg(mode)
            .arg(&server_config)
            .arg(&server_state)
            .arg(shared_path);
        if let Some(source) = source {
            command.arg(source);
        }
        command.output().unwrap()
    };

    let update = change("update", "譜面/試験譜面.TJA", Some(&revised));
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    let invalid = change("delete", "../試験譜面.tja", None);
    assert!(!invalid.status.success());

    let delete = change("delete", "譜面/試験譜面.tja", None);
    assert!(
        delete.status.success(),
        "{}",
        String::from_utf8_lossy(&delete.stderr)
    );
    assert_eq!(std::fs::read(&source).unwrap(), contents);
    assert_eq!(std::fs::read(&revised).unwrap(), b"TITLE:Revision");

    let next_config = setup.join("sender/next.json");
    legacy["generation"] = 2.into();
    let revoked = setup.join("receiver-2/direct.json");
    let revoked_seed = std::fs::read(revoked.with_file_name("event-key.bin")).unwrap();
    let revoked_key = PeerSigningKey::from_bytes(revoked_seed.as_slice().try_into().unwrap());
    let revoked_peer = encode_hex(revoked_key.verifying_key().as_bytes());
    assert!(legacy["members"]
        .as_object_mut()
        .unwrap()
        .remove(&revoked_peer)
        .is_some());
    std::fs::write(&next_config, legacy.to_string()).unwrap();
    let admin = path.join("admin-test.bin");
    std::fs::write(&admin, [47; 32]).unwrap();
    let bundle = path.join("migration");
    let prepare = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("prepare-generation")
            .arg(&server_config)
            .arg(&server_state)
            .arg(&next_config)
            .arg(&admin)
            .arg(&bundle)
            .output()
            .unwrap()
    };
    let output = prepare();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!prepare().status.success());
    assert!(bundle.join("checkpoint.bin").is_file());
    assert!(bundle.join("transition.bin").is_file());
    let admin_public = path.join("admin-public.bin");
    let admin_key = PeerSigningKey::from_bytes(&[47; 32]);
    std::fs::write(&admin_public, admin_key.verifying_key().as_bytes()).unwrap();
    let verify = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("verify-generation")
            .arg(&server_config)
            .arg(&next_config)
            .arg(&admin_public)
            .arg(&bundle)
            .output()
            .unwrap()
    };
    let output = verify();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let obsolete = bundle.join("files").join(encode_hex(&sha256(contents)));
    std::fs::write(&obsolete, vec![0; contents.len()]).unwrap();
    assert!(!verify().status.success());
    std::fs::write(&obsolete, contents).unwrap();
    let wrong = PeerSigningKey::from_bytes(&[48; 32]);
    std::fs::write(&admin_public, wrong.verifying_key().as_bytes()).unwrap();
    assert!(!verify().status.success());
    std::fs::write(&admin_public, admin_key.verifying_key().as_bytes()).unwrap();
    assert_eq!(
        std::fs::read(bundle.join("files").join(encode_hex(&sha256(contents)))).unwrap(),
        contents
    );
    assert_eq!(
        std::fs::read(
            bundle
                .join("files")
                .join(encode_hex(&sha256(b"TITLE:Revision")))
        )
        .unwrap(),
        b"TITLE:Revision"
    );

    let mut sender = Sender::open(&server_config, &server_state).unwrap();
    let listener = sender.bind().unwrap();
    let receiver = setup.join("receiver-1/direct.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receiver).unwrap()).unwrap();
    config["fallback_peers"] = serde_json::json!([{
        "address": listener.local_addr().unwrap().to_string(),
        "server_name": config["server_name"],
        "server_pin": config["server_pin"],
    }]);
    config["address"] = "127.0.0.1:0".into();
    std::fs::write(&receiver, config.to_string()).unwrap();
    let destination = path.join("fresh-destination");
    let local = destination.join("譜面/試験譜面.TJA");
    std::fs::create_dir_all(local.parent().unwrap()).unwrap();
    std::fs::write(&local, b"local file").unwrap();
    std::thread::scope(|threads| {
        let session = threads.spawn(move || {
            for _ in 0..2 + usize::from(cfg!(windows)) {
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
            }
        });
        let state = path.join("client-state");
        let pull = |destination: &std::path::Path| {
            let [config_path, state_path, output_path] =
                [receiver.as_path(), state.as_path(), destination]
                    .map(|path| path.to_str().unwrap().as_bytes());
            extern "C" fn copy_originals(
                context: *mut std::ffi::c_void,
                data: *const u8,
                length: usize,
            ) {
                unsafe {
                    (*context.cast::<Vec<u8>>())
                        .extend_from_slice(std::slice::from_raw_parts(data, length));
                }
            }
            let mut json = Vec::<u8>::new();
            let mut report = PullSummaryV3::default();
            let status = unsafe {
                p2p_sync_pull_v4(
                    config_path.as_ptr(),
                    config_path.len(),
                    state_path.as_ptr(),
                    state_path.len(),
                    output_path.as_ptr(),
                    output_path.len(),
                    &mut report,
                    Some(copy_originals),
                    (&mut json as *mut Vec<u8>).cast(),
                )
            };
            assert_eq!(status, 0);
            let originals: Vec<serde_json::Value> = serde_json::from_slice(&json).unwrap();
            (report, originals)
        };
        let seed = std::fs::read(receiver.parent().unwrap().join("event-key.bin")).unwrap();
        let reader = PeerSigningKey::from_bytes(seed.as_slice().try_into().unwrap());
        let reader_hex: String = reader
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let reader_waiting = |original: &serde_json::Value| {
            original["waiting"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["peer"] == reader_hex)
        };
        let (report, originals) = pull(&destination);
        assert_eq!(report.sync.events_added, 5);
        assert_eq!(report.sync.files_published, 0);
        assert_eq!(report.protected_files, 2);
        assert_eq!(report.protected_missing, 0);
        assert_eq!(
            report.protected_bytes,
            (contents.len() + b"TITLE:Revision".len()) as u64
        );
        assert_eq!(originals.len(), 2);
        assert!(originals.iter().all(reader_waiting));
        assert_eq!(std::fs::read(&local).unwrap(), b"local file");
        std::fs::remove_file(&local).unwrap();

        let (_, originals) = pull(&destination);
        assert_eq!(originals.len(), 2);
        assert!(originals.iter().all(|original| !reader_waiting(original)));
        assert!(!destination.join("譜面/試験譜面.tja").exists());

        #[cfg(windows)]
        {
            let existing = path.join("destination-1");
            let (report, originals) = pull(&existing);
            assert_eq!(report.sync.events_added, 2);
            assert_eq!(report.sync.files_blocked, 0);
            assert_eq!(report.sync.files_removed, 1);
            assert_eq!(originals.len(), 2);
            assert!(originals.iter().all(|original| !reader_waiting(original)));
            assert!(!existing.join("譜面/試験譜面.tja").exists());
        }
        session.join().unwrap();
    });
    let migration = serde_json::json!({
        "previous_server": server_config,
        "administrator_public_key": admin_public,
        "bundle": bundle,
    });
    legacy["migration"] = migration.clone();
    std::fs::write(&next_config, legacy.to_string()).unwrap();
    let old_sender = Sender::open(&server_config, &server_state).unwrap();
    let old_receiver = receiver.with_file_name("old-generation.json");
    std::fs::write(&old_receiver, config.to_string()).unwrap();
    let mut upgraded = Sender::open(&next_config, &server_state).unwrap();
    assert!(matches!(
        old_sender.bind(),
        Err(p2p_sync_core::host::HostError::InactiveGeneration)
    ));
    assert!(matches!(
        Sender::open(&server_config, &server_state),
        Err(p2p_sync_core::host::HostError::InactiveGeneration)
    ));
    let new_file = path.join("after-migration.tja");
    std::fs::write(&new_file, b"new generation").unwrap();
    upgraded.publish(&new_file).unwrap();
    let listener = upgraded.bind().unwrap();
    config["address"] = listener.local_addr().unwrap().to_string().into();
    config["generation"] = 2.into();
    config["members"] = legacy["members"].clone();
    let unseeded = receiver.with_file_name("unseeded.json");
    std::fs::write(&unseeded, config.to_string()).unwrap();
    config["migration"] = migration;
    std::fs::write(&receiver, config.to_string()).unwrap();
    // The removed peer may know the new roster and checkpoint. Keep its
    // original certificate and attempt the new TLS scope anyway.
    let mut revoked_config = config.clone();
    revoked_config.as_object_mut().unwrap().remove("event_key");
    std::fs::write(&revoked, revoked_config.to_string()).unwrap();
    let destination = path.join("new-generation-destination");
    std::fs::create_dir(&destination).unwrap();
    std::thread::scope(|threads| {
        let session = threads.spawn(move || {
            assert!(matches!(
                upgraded.serve(listener.accept().unwrap().0),
                Err(p2p_sync_core::host::HostError::Direct(
                    direct::DirectError::Tls(_)
                ))
            ));
            assert!(matches!(
                upgraded.serve(listener.accept().unwrap().0),
                Err(p2p_sync_core::host::HostError::Direct(
                    direct::DirectError::Tls(TlsError::UnexpectedCertificate)
                ))
            ));
            upgraded.serve(listener.accept().unwrap().0).unwrap();
        });
        assert!(matches!(
            pull_from_config(&unseeded, &path.join("unseeded-state"), &destination),
            Err(p2p_sync_core::host::HostError::Direct(
                direct::DirectError::Tls(_)
            ))
        ));
        assert!(pull_from_config(&revoked, &path.join("revoked-state"), &destination).is_err());
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        let report = pull_from_config(&receiver, &path.join("client-state"), &destination).unwrap();
        assert_eq!(report.events_added, 1);
        assert_eq!(report.files_published, 1);
        session.join().unwrap();
    });
    assert!(!destination.join("譜面/試験譜面.tja").exists());
    assert!(matches!(
        pull_from_config(&old_receiver, &path.join("client-state"), &destination),
        Err(p2p_sync_core::host::HostError::InactiveGeneration)
    ));
    assert_eq!(
        std::fs::read(destination.join("after-migration.tja")).unwrap(),
        b"new generation"
    );
}

#[test]
fn generated_writers_use_distinct_credentials_and_share_their_changes() {
    use p2p_sync_core::{
        host::{pull_from_config, Sender},
        p2p_sync_writer_conflicts, p2p_sync_writer_resolve,
    };

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let setup = root.join("setup");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8750", "127.0.0.1:8750", "1", "2"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = |name: &str| {
        let path = setup.join(name).join("server.json");
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["bind"] = "127.0.0.1:0".into();
        std::fs::write(&path, json.to_string()).unwrap();
        path
    };
    let mut first = Sender::open(&config("sender"), &root.join("first")).unwrap();
    let second_config = config("writer-2");
    let second_state = root.join("second");
    let mut second = Sender::open(&second_config, &second_state).unwrap();
    let first_file = root.join("first.tja");
    let second_file = root.join("second.tja");
    std::fs::write(&first_file, b"writer one").unwrap();
    std::fs::write(&second_file, b"writer two").unwrap();
    assert_ne!(
        first.publish(&first_file).unwrap().peer,
        second.publish(&second_file).unwrap().peer
    );
    let first_listener = first.bind().unwrap();
    let update_address = |path: &std::path::Path, listener: &TcpListener| {
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        json["address"] = listener.local_addr().unwrap().to_string().into();
        std::fs::write(path, json.to_string()).unwrap();
    };
    let peer_config = setup.join("writer-2/direct.json");
    update_address(&peer_config, &first_listener);
    let mut peer_settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&peer_config).unwrap()).unwrap();
    peer_settings["fallback_peers"] = serde_json::json!([{
        "address": peer_settings["address"],
        "server_name": peer_settings["server_name"],
        "server_pin": peer_settings["server_pin"],
    }]);
    peer_settings["address"] = "127.0.0.1:0".into();
    std::fs::write(&peer_config, peer_settings.to_string()).unwrap();
    let left_file = root.join("left.tja");
    let right_file = root.join("right.tja");
    std::fs::write(&left_file, b"left edit").unwrap();
    std::fs::write(&right_file, b"right edit").unwrap();
    let right = std::thread::scope(|threads| {
        let serving = threads.spawn(move || {
            first.serve(first_listener.accept().unwrap().0).unwrap();
            first.revise("first.tja", Some(&left_file)).unwrap();
            first.serve(first_listener.accept().unwrap().0).unwrap();
        });
        assert_eq!(second.catch_up(&peer_config).unwrap(), 1);
        let right = second.revise("first.tja", Some(&right_file)).unwrap();
        assert_eq!(second.catch_up(&peer_config).unwrap(), 1);
        serving.join().unwrap();
        right
    });
    let conflicts = second.conflicts().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].len(), 2);
    let config_bytes = second_config.to_str().unwrap().as_bytes();
    let state_bytes = second_state.to_str().unwrap().as_bytes();
    extern "C" fn capture(context: *mut std::ffi::c_void, data: *const u8, length: usize) {
        unsafe {
            (*context.cast::<Vec<u8>>())
                .extend_from_slice(std::slice::from_raw_parts(data, length));
        }
    }
    let mut json = Vec::new();
    assert_eq!(
        unsafe {
            p2p_sync_writer_conflicts(
                config_bytes.as_ptr(),
                config_bytes.len(),
                state_bytes.as_ptr(),
                state_bytes.len(),
                Some(capture),
                (&mut json as *mut Vec<u8>).cast(),
            )
        },
        0
    );
    let listed: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let versions = listed[0]["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    let reviewed: Vec<&str> = versions
        .iter()
        .map(|version| version["id"].as_str().unwrap())
        .collect();
    let chosen = format!("{}:{}", encode_hex(&right.peer), right.sequence);
    assert!(reviewed.contains(&chosen.as_str()));
    let reviewed_json = serde_json::to_vec(&reviewed).unwrap();
    let resolve = |reviewed: &[u8]| unsafe {
        p2p_sync_writer_resolve(
            config_bytes.as_ptr(),
            config_bytes.len(),
            state_bytes.as_ptr(),
            state_bytes.len(),
            chosen.as_ptr(),
            chosen.len(),
            reviewed.as_ptr(),
            reviewed.len(),
        )
    };
    assert_eq!(resolve(b"[]"), 1);
    assert_eq!(
        resolve(
            serde_json::to_string(&[reviewed[0], reviewed[0]])
                .unwrap()
                .as_bytes()
        ),
        1
    );
    let second_listener = second.bind().unwrap();
    let receiver = setup.join("receiver/direct.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receiver).unwrap()).unwrap();
    let endpoint = json["fallback_peers"][0].clone();
    json["server_pin"] = endpoint["server_pin"].clone();
    json.as_object_mut().unwrap().remove("fallback_peers");
    std::fs::write(&receiver, json.to_string()).unwrap();
    update_address(&receiver, &second_listener);
    let destination = root.join("received");
    std::fs::create_dir(&destination).unwrap();
    std::thread::scope(|threads| {
        let serving = threads.spawn(move || {
            second.serve(second_listener.accept().unwrap().0).unwrap();
            assert_eq!(resolve(&reviewed_json), 0);
            assert_ne!(resolve(&reviewed_json), 0);
            second.serve(second_listener.accept().unwrap().0).unwrap();
        });
        let report = pull_from_config(&receiver, &root.join("receiver"), &destination).unwrap();
        assert_eq!(report.files_blocked, 1);
        assert_eq!(report.files_published, 1);
        assert!(!destination.join("first.tja").exists());
        let report = pull_from_config(&receiver, &root.join("receiver"), &destination).unwrap();
        assert_eq!(report.events_added, 1);
        assert_eq!(report.files_published, 1);
        assert_eq!(report.files_present, 1);
        assert_eq!(report.files_blocked, 0);
        serving.join().unwrap();
    });
    assert_eq!(
        std::fs::read(destination.join("first.tja")).unwrap(),
        b"right edit"
    );
    assert_eq!(
        std::fs::read(destination.join("second.tja")).unwrap(),
        b"writer two"
    );
}

#[test]
fn running_sender_refreshes_its_source_and_serves_new_files() {
    use p2p_sync_core::host::{pull_from_config, Sender};
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command, Stdio},
        time::Instant,
    };

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let configuration = root.join("setup");
    let state = root.join("sender-state");
    let binary = env!("CARGO_BIN_EXE_p2p-send");
    let output = Command::new(binary)
        .arg("setup")
        .arg(&configuration)
        .args(["127.0.0.1:8765", "127.0.0.1:8765"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let sender_config = configuration.join("sender/server.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sender_config).unwrap()).unwrap();
    settings["bind"] = "127.0.0.1:0".into();
    std::fs::write(&sender_config, settings.to_string()).unwrap();

    let source = root.join("shared");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(source.join("folder")).unwrap();
    std::fs::write(source.join("existing.tja"), b"initial").unwrap();
    let sender = Sender::open(&sender_config, &state).unwrap();
    assert!(sender.validate_source_root(root).is_err());
    drop(sender);

    let mut child = ChildGuard(
        Command::new(binary)
            .arg("serve-tree")
            .arg(&sender_config)
            .arg(&state)
            .arg(&source)
            .arg("1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut debug_output = BufReader::new(child.0.stdout.take().unwrap());
    let mut ready = String::new();
    debug_output.read_line(&mut ready).unwrap();
    let address = ready.trim().strip_prefix("Serving on ").unwrap();

    let receiver = configuration.join("receiver/direct.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receiver).unwrap()).unwrap();
    settings["address"] = address.into();
    std::fs::write(&receiver, settings.to_string()).unwrap();
    let destination = root.join("destination");
    let client_state = root.join("receiver-state");
    std::fs::create_dir(&destination).unwrap();
    let report = pull_from_config(&receiver, &client_state, &destination).unwrap();
    assert_eq!(report.files_published, 1);

    std::fs::write(source.join("追加.tja"), b"added while serving").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let report = pull_from_config(&receiver, &client_state, &destination).unwrap();
        if report.files_published == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "running sender did not refresh");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        std::fs::read(destination.join("追加.tja")).unwrap(),
        b"added while serving"
    );
    let mirror_state = root.join("mirror-state");
    let mut mirror = ChildGuard(
        Command::new(binary)
            .arg("serve-peer")
            .arg(&sender_config)
            .arg(&mirror_state)
            .arg(&receiver)
            .arg("1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(mirror.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let mirror_address = ready.trim().strip_prefix("Serving on ").unwrap();
    let mirror_receiver = receiver.with_file_name("mirror.json");
    let mut mirror_settings = settings.clone();
    mirror_settings["address"] = mirror_address.into();
    std::fs::write(&mirror_receiver, mirror_settings.to_string()).unwrap();
    let mirror_destination = root.join("mirror-destination");
    std::fs::create_dir(&mirror_destination).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !mirror_destination.join("追加.tja").exists() {
        pull_from_config(&mirror_receiver, &client_state, &mirror_destination).unwrap();
        assert!(Instant::now() < deadline, "running mirror did not catch up");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        std::fs::read(mirror_destination.join("追加.tja")).unwrap(),
        b"added while serving"
    );
    drop(mirror);
    #[cfg(windows)]
    {
        drop(child);
        let output = Command::new(binary)
            .arg("move")
            .arg(&sender_config)
            .arg(&state)
            .args(["existing.tja", "folder/moved.tja"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut sender = Sender::open(&sender_config, &state).unwrap();
        let listener = sender.bind().unwrap();
        settings["address"] = listener.local_addr().unwrap().to_string().into();
        std::fs::write(&receiver, settings.to_string()).unwrap();
        let old = destination.join("existing.tja");
        let new = destination.join("folder/moved.tja");
        std::fs::write(&new, b"local file").unwrap();
        std::thread::scope(|threads| {
            let server = threads.spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
                sender
                    .relocate("folder/moved.tja", "folder/final.tja")
                    .unwrap();
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
                sender
                    .relocate("folder/final.tja", "folder/Final.TJA")
                    .unwrap();
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
                sender.relocate("folder/Final.TJA", "edited.tja").unwrap();
                let replacement = source.join("replacement.tja");
                std::fs::write(&replacement, b"new contents").unwrap();
                sender.revise("edited.tja", Some(&replacement)).unwrap();
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
            });
            let report = pull_from_config(&receiver, &client_state, &destination).unwrap();
            assert_eq!(report.files_blocked, 1);
            assert_eq!(std::fs::read(&old).unwrap(), b"initial");
            assert_eq!(std::fs::read(&new).unwrap(), b"local file");
            let report = pull_from_config(&receiver, &client_state, &destination).unwrap();
            assert_eq!(report.files_updated, 1);
            assert!(!old.exists());
            assert_eq!(std::fs::read(&new).unwrap(), b"local file");
            assert_eq!(
                std::fs::read(destination.join("folder/final.tja")).unwrap(),
                b"initial"
            );
            let report = pull_from_config(&receiver, &client_state, &destination).unwrap();
            assert_eq!(report.files_updated, 1);
            let exact_names: Vec<_> = std::fs::read_dir(destination.join("folder"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert!(exact_names.iter().any(|name| name == "Final.TJA"));
            assert!(!exact_names.iter().any(|name| name == "final.tja"));
            let report = pull_from_config(&receiver, &client_state, &destination).unwrap();
            assert_eq!(report.events_added, 2);
            assert_eq!(report.files_updated, 1);
            assert!(!destination.join("folder/Final.TJA").exists());
            assert_eq!(
                std::fs::read(destination.join("edited.tja")).unwrap(),
                b"new contents"
            );
            server.join().unwrap();
        });
    }
}
