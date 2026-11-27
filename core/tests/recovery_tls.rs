use std::net::TcpListener;

use p2p_sync_core::{
    direct::DirectError,
    host::{pull_from_config, HostError, Sender},
    recovery::RecoveryError,
};

#[test]
fn pinned_tls_peer_supplies_anchor_then_catchup_satisfies_startup_gate() {
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8780", "127.0.0.1:8780", "1", "2"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let first_config = setup.join("sender/server.json");
    let second_config = setup.join("writer-2/server.json");
    let direct_config = setup.join("writer-2/direct.json");
    for config in [&first_config, &second_config] {
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
        json["bind"] = "127.0.0.1:0".into();
        std::fs::write(config, json.to_string()).unwrap();
    }
    let first_state = root.path().join("first-state");
    let second_state = root.path().join("second-state");
    let source = root.path().join("song.tja");
    let next_source = root.path().join("next.tja");
    let recovered_source = root.path().join("recovered-source");
    std::fs::create_dir(&recovered_source).unwrap();
    std::fs::write(recovered_source.join("song.tja"), b"anchor exchange").unwrap();
    std::fs::write(&source, b"anchor exchange").unwrap();
    let mut first = Sender::open(&first_config, &first_state).unwrap();
    let trusted_peer = first.publish(&source).unwrap().peer;
    let listener: TcpListener = first.bind().unwrap();
    let mut direct: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&direct_config).unwrap()).unwrap();
    direct["address"] = listener.local_addr().unwrap().to_string().into();
    direct["event_peer"] = trusted_peer
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .into();
    direct.as_object_mut().unwrap().remove("fallback_peers");
    std::fs::write(&direct_config, direct.to_string()).unwrap();

    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            for _ in 0..2 {
                let (socket, _) = listener.accept().unwrap();
                first.serve(socket).unwrap();
            }
            std::fs::write(&next_source, b"later event").unwrap();
            first.publish(&next_source).unwrap();
            first.revise("song.tja", None).unwrap();
            for _ in 0..2 {
                let (socket, _) = listener.accept().unwrap();
                first.serve(socket).unwrap();
            }
        });
        let second = Sender::open(&second_config, &second_state).unwrap();
        let anchor = setup.join("peer-anchor.bin");
        second
            .fetch_recovery_anchor(&direct_config, trusted_peer, &anchor)
            .unwrap();
        drop(second);
        assert!(anchor.is_file());
        let mut settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&second_config).unwrap()).unwrap();
        let peer: String = trusted_peer
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        settings["recovery_anchor"] = serde_json::json!({"file":"../peer-anchor.bin", "peer":peer});
        std::fs::write(&second_config, settings.to_string()).unwrap();
        assert!(matches!(
            Sender::open(&second_config, &second_state),
            Err(HostError::Recovery(RecoveryError::Behind(_)))
        ));
        assert_eq!(
            Sender::recover_from_anchor(&second_config, &second_state, &direct_config).unwrap(),
            1
        );
        let old_anchor = std::fs::read(&anchor).unwrap();
        let refresh = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("refresh-recovery-anchor")
            .arg(&second_config)
            .arg(&second_state)
            .arg(&direct_config)
            .output()
            .unwrap();
        assert!(
            refresh.status.success(),
            "{}",
            String::from_utf8_lossy(&refresh.stderr)
        );
        assert!(String::from_utf8_lossy(&refresh.stdout).contains("Imported 2 event(s)"));
        assert_ne!(std::fs::read(&anchor).unwrap(), old_anchor);
        server.join().unwrap();
        let mut recovered = Sender::open(&second_config, &second_state).unwrap();
        assert_eq!(
            recovered
                .recovery_review_candidates(&recovered_source)
                .unwrap(),
            vec![vec!["song.tja".to_owned()]]
        );
        assert!(matches!(
            recovered.refresh_tree(&recovered_source),
            Err(HostError::RecoveryReviewRequired)
        ));
        assert!(matches!(
            recovered.publish(&recovered_source.join("song.tja")),
            Err(HostError::RecoveryReviewRequired)
        ));
        assert!(matches!(
            recovered.revise("song.tja", Some(&recovered_source.join("song.tja"))),
            Err(HostError::RecoveryReviewRequired)
        ));
        assert!(matches!(
            recovered.relocate("song.tja", "restored.tja"),
            Err(HostError::RecoveryReviewRequired)
        ));
        assert!(matches!(
            recovered.resume_tree_refresh(&recovered_source),
            Err(HostError::RecoveryReviewRequired)
        ));
        std::fs::rename(
            recovered_source.join("song.tja"),
            root.path().join("saved-song.tja"),
        )
        .unwrap();
        recovered.resume_tree_refresh(&recovered_source).unwrap();
        recovered.refresh_tree(&recovered_source).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("saved-song.tja")).unwrap(),
            b"anchor exchange"
        );
    });
}

#[test]
fn receiver_anchor_rejects_restored_old_database_before_touching_destination() {
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8785", "127.0.0.1:8785", "1", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sender_config = setup.join("sender/server.json");
    let receiver_config = setup.join("receiver/direct.json");
    let state = root.path().join("receiver-state");
    let destination = root.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"deleted after first pull").unwrap();
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sender_config).unwrap()).unwrap();
    settings["bind"] = "127.0.0.1:0".into();
    std::fs::write(&sender_config, settings.to_string()).unwrap();
    let mut sender = Sender::open(&sender_config, &root.path().join("sender-state")).unwrap();
    let trusted_peer = sender.publish(&source).unwrap().peer;
    let listener = sender.bind().unwrap();
    let mut receiver: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receiver_config).unwrap()).unwrap();
    receiver["address"] = listener.local_addr().unwrap().to_string().into();
    std::fs::write(&receiver_config, receiver.to_string()).unwrap();

    let pull_once = |sender: &mut Sender| {
        std::thread::scope(|threads| {
            let server = threads.spawn(|| {
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
            });
            let result = pull_from_config(&receiver_config, &state, &destination);
            server.join().unwrap();
            result.unwrap();
        });
    };
    pull_once(&mut sender);
    assert_eq!(
        std::fs::read(destination.join("song.tja")).unwrap(),
        b"deleted after first pull"
    );
    let share = receiver["share"].as_str().unwrap();
    let destination_id: String = p2p_sync_core::sha256(
        destination
            .canonicalize()
            .unwrap()
            .as_os_str()
            .as_encoded_bytes(),
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
    let database = state
        .join(share)
        .join("1")
        .join(&destination_id)
        .join("sync.db");
    let witness = state.join(share).join("witness/1").join(&destination_id);
    let old_database = root.path().join("old-receiver.db");
    let old_witness = root.path().join("old-receiver-witness");
    std::fs::copy(&database, &old_database).unwrap();
    std::fs::create_dir(&old_witness).unwrap();
    for entry in std::fs::read_dir(&witness).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), old_witness.join(entry.file_name())).unwrap();
    }

    sender.revise("song.tja", None).unwrap();
    pull_once(&mut sender);
    assert!(!destination.join("song.tja").exists());
    let anchor = setup.join("receiver-anchor.bin");
    sender.export_recovery_anchor(&anchor).unwrap();
    let peer: String = trusted_peer
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    receiver["recovery_anchor"] = serde_json::json!({"file":"../receiver-anchor.bin", "peer":peer});
    std::fs::write(&receiver_config, receiver.to_string()).unwrap();
    pull_once(&mut sender);

    let reader_key = p2p_sync_core::auth::PeerSigningKey::from_bytes(
        &std::fs::read(setup.join("receiver/event-key.bin"))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let reader_peer: String = reader_key
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    receiver["recovery_anchor"]["peer"] = reader_peer.into();
    std::fs::write(&receiver_config, receiver.to_string()).unwrap();
    assert!(matches!(
        pull_from_config(&receiver_config, &state, &destination),
        Err(HostError::Configuration)
    ));
    receiver["recovery_anchor"]["peer"] = peer.into();
    let internal_anchor = state.join("internal-anchor.bin");
    std::fs::copy(&anchor, &internal_anchor).unwrap();
    receiver["recovery_anchor"]["file"] = internal_anchor.to_string_lossy().into_owned().into();
    std::fs::write(&receiver_config, receiver.to_string()).unwrap();
    assert!(matches!(
        pull_from_config(&receiver_config, &state, &destination),
        Err(HostError::Configuration)
    ));
    receiver["recovery_anchor"]["file"] = "../receiver-anchor.bin".into();
    std::fs::write(&receiver_config, receiver.to_string()).unwrap();

    std::fs::copy(&old_database, &database).unwrap();
    std::fs::remove_dir_all(&witness).unwrap();
    std::fs::rename(&old_witness, &witness).unwrap();
    assert!(matches!(
        pull_from_config(&receiver_config, &state, &destination),
        Err(HostError::Recovery(RecoveryError::Behind(_)))
    ));
    let [config_bytes, state_bytes, destination_bytes] =
        [&receiver_config, &state, &destination].map(|path| path.to_str().unwrap().as_bytes());
    let mut report = p2p_sync_core::host::PullSummaryV3::default();
    assert_eq!(
        unsafe {
            p2p_sync_core::p2p_sync_pull_v6(
                config_bytes.as_ptr(),
                config_bytes.len(),
                state_bytes.as_ptr(),
                state_bytes.len(),
                destination_bytes.as_ptr(),
                destination_bytes.len(),
                &mut report,
                None,
                std::ptr::null_mut(),
                None,
            )
        },
        8
    );
    assert!(!destination.join("song.tja").exists());

    std::fs::write(destination.join("song.tja"), b"deleted after first pull").unwrap();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            let (socket, _) = listener.accept().unwrap();
            sender.serve(socket).unwrap();
        });
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("recover-receiver-from-anchor")
            .arg(&receiver_config)
            .arg(&state)
            .arg(&destination)
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("Recovered 1 signed event(s)"));
    });
    assert_eq!(std::fs::read(destination.join("song.tja")).unwrap(), b"deleted after first pull");
    pull_once(&mut sender);
    assert!(!destination.join("song.tja").exists());
}

#[test]
fn receiver_rejects_a_peer_whose_own_signed_history_was_rolled_back() {
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8782", "127.0.0.1:8782", "1", "2"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let first_config = setup.join("sender/server.json");
    let second_config = setup.join("writer-2/server.json");
    let direct_config = setup.join("writer-2/direct.json");
    for config in [&first_config, &second_config] {
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
        json["bind"] = "127.0.0.1:0".into();
        std::fs::write(config, json.to_string()).unwrap();
    }
    let first_state = root.path().join("first-state");
    let second_state = root.path().join("second-state");
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"original is still here").unwrap();
    let peer = Sender::open(&first_config, &first_state)
        .unwrap()
        .publish(&source)
        .unwrap()
        .peer;
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&first_config).unwrap()).unwrap();
    let share = settings["share"].as_str().unwrap();
    let database = first_state.join(share).join("1/sync.db");
    let witness = first_state.join(share).join("witness/1/sender");
    let old_database = root.path().join("old.db");
    let old_witness = root.path().join("old-witness");
    std::fs::copy(&database, &old_database).unwrap();
    std::fs::create_dir(&old_witness).unwrap();
    for entry in std::fs::read_dir(&witness).unwrap() {
        let entry = entry.unwrap();
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "witness")
        {
            std::fs::copy(entry.path(), old_witness.join(entry.file_name())).unwrap();
        }
    }
    Sender::open(&first_config, &first_state)
        .unwrap()
        .revise("song.tja", None)
        .unwrap();
    let mut direct: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&direct_config).unwrap()).unwrap();
    direct["event_peer"] = peer
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .into();
    direct.as_object_mut().unwrap().remove("fallback_peers");

    let mut first = Sender::open(&first_config, &first_state).unwrap();
    let listener = first.bind().unwrap();
    direct["address"] = listener.local_addr().unwrap().to_string().into();
    std::fs::write(&direct_config, direct.to_string()).unwrap();
    let mut second = Sender::open(&second_config, &second_state).unwrap();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            let (socket, _) = listener.accept().unwrap();
            first.serve(socket).unwrap();
        });
        assert_eq!(second.catch_up(&direct_config).unwrap(), 2);
        server.join().unwrap();
    });
    drop(first);

    std::fs::copy(&old_database, &database).unwrap();
    std::fs::remove_dir_all(&witness).unwrap();
    std::fs::rename(&old_witness, &witness).unwrap();
    let mut rolled_back = Sender::open(&first_config, &first_state).unwrap();
    let listener = rolled_back.bind().unwrap();
    direct["address"] = listener.local_addr().unwrap().to_string().into();
    std::fs::write(&direct_config, direct.to_string()).unwrap();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            let (socket, _) = listener.accept().unwrap();
            let _ = rolled_back.serve(socket);
        });
        assert!(matches!(
            second.catch_up(&direct_config),
            Err(HostError::Direct(DirectError::Recovery(
                RecoveryError::RemoteBehind(_)
            )))
        ));
        server.join().unwrap();
    });
    assert_eq!(std::fs::read(&source).unwrap(), b"original is still here");
}
