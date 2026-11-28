use std::net::{TcpListener, TcpStream};

use p2p_sync_core::{
    direct::DirectError,
    host::{pull_from_config, HostError, Sender},
    recovery::RecoveryError,
};

#[test]
fn receiver_auto_registers_first_anchor_after_successful_sync() {
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup").arg(&setup)
        .args(["127.0.0.1:8787", "127.0.0.1:8787", "1", "1"])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let sender_config = setup.join("sender/server.json");
    let receiver_config = setup.join("receiver/direct.json");
    let mut server_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sender_config).unwrap()).unwrap();
    server_json["bind"] = "127.0.0.1:0".into();
    std::fs::write(&sender_config, server_json.to_string()).unwrap();
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"auto anchor verified content").unwrap();
    let mut sender = Sender::open(&sender_config, &root.path().join("sender-state")).unwrap();
    sender.publish(&source).unwrap();
    let listener = sender.bind().unwrap();
    let mut direct_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receiver_config).unwrap()).unwrap();
    direct_json["address"] = listener.local_addr().unwrap().to_string().into();
    direct_json["auto_recovery_anchor"] = "../auto-receiver-anchor.bin".into();
    std::fs::write(&receiver_config, direct_json.to_string()).unwrap();
    let state = root.path().join("receiver-state");
    let destination = root.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            for _ in 0..2 {
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
            }
        });
        let summary = pull_from_config(&receiver_config, &state, &destination).unwrap();
        assert_eq!(summary.events_added, 1);
        server.join().unwrap();
    });
    assert_eq!(std::fs::read(destination.join("song.tja")).unwrap(),
        b"auto anchor verified content");
    let anchor = setup.join("auto-receiver-anchor.bin");
    assert!(anchor.is_file());
    let updated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receiver_config).unwrap()).unwrap();
    assert_eq!(updated["recovery_anchor"]["file"].as_str().unwrap(),
        anchor.canonicalize().unwrap().to_str().unwrap());
    assert!(updated["recovery_anchor"]["peer"].as_str().is_some());
}

#[test]
fn writer_auto_registers_first_anchor_after_catch_up() {
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup").arg(&setup)
        .args(["127.0.0.1:8787", "127.0.0.1:8787", "1", "2"])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let first_config = setup.join("sender/server.json");
    let second_config = setup.join("writer-2/server.json");
    let peer_config = setup.join("writer-2/direct.json");
    let mut first_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&first_config).unwrap()).unwrap();
    first_json["bind"] = "127.0.0.1:0".into();
    std::fs::write(&first_config, first_json.to_string()).unwrap();
    let mut first = Sender::open(&first_config, &root.path().join("first-state")).unwrap();
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"auto writer anchor content").unwrap();
    first.publish(&source).unwrap();
    let listener = first.bind().unwrap();
    let mut direct_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&peer_config).unwrap()).unwrap();
    direct_json["address"] = listener.local_addr().unwrap().to_string().into();
    std::fs::write(&peer_config, direct_json.to_string()).unwrap();
    let mut second_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&second_config).unwrap()).unwrap();
    second_json["auto_recovery_anchor"] = "../auto-writer-anchor.bin".into();
    std::fs::write(&second_config, second_json.to_string()).unwrap();
    let second_state = root.path().join("second-state");
    let mut second = Sender::open(&second_config, &second_state).unwrap();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            for _ in 0..2 {
                let (socket, _) = listener.accept().unwrap();
                first.serve(socket).unwrap();
            }
        });
        assert_eq!(second.catch_up_with_anchor(&second_config, &peer_config).unwrap(), 1);
        server.join().unwrap();
    });
    let anchor = setup.join("auto-writer-anchor.bin");
    assert!(anchor.is_file());
    let updated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&second_config).unwrap()).unwrap();
    assert_eq!(updated["recovery_anchor"]["file"].as_str().unwrap(),
        anchor.canonicalize().unwrap().to_str().unwrap());
    assert!(Sender::open(&second_config, &second_state).is_ok());
}

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
            vec![vec!["next.tja".to_owned()], vec!["song.tja".to_owned()]]
        );
        let unsent = recovered_source.join("unsent/nested");
        std::fs::create_dir_all(&unsent).unwrap();
        std::fs::write(unsent.join("draft.tja"), b"local draft not sent").unwrap();
        let archive = root.path().join("source-archive");
        assert!(matches!(
            recovered.archive_recovered_source(&recovered_source,
                &recovered_source.join("bad-archive")),
            Err(HostError::Configuration)
        ));
        let report = recovered.archive_recovered_source(&recovered_source, &archive).unwrap();
        assert_eq!(report.missing, vec![vec!["next.tja".to_owned()]]);
        assert_eq!(report.files.len(), 2);
        assert_eq!(std::fs::read(archive.join("source/song.tja")).unwrap(), b"anchor exchange");
        assert_eq!(std::fs::read(archive.join("source/unsent/nested/draft.tja")).unwrap(),
            b"local draft not sent");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["missing"], serde_json::json!([["next.tja"]]));
        assert!(recovered.archive_recovered_source(&recovered_source, &archive).is_err());
        let cli_archive = root.path().join("cli-source-archive");
        let cli = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("archive-recovered-source")
            .arg(&second_config).arg(&second_state)
            .arg(&recovered_source).arg(&cli_archive)
            .output().unwrap();
        assert!(cli.status.success(), "{}", String::from_utf8_lossy(&cli.stderr));
        assert_eq!(std::fs::read(cli_archive.join("source/unsent/nested/draft.tja")).unwrap(),
            b"local draft not sent");
        let verify = || std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("verify-recovered-source-archive").arg(&cli_archive)
            .output().unwrap();
        assert!(verify().status.success());
        std::fs::write(cli_archive.join("source/unlisted.tja"), b"unlisted").unwrap();
        assert!(!verify().status.success());
        std::fs::remove_file(cli_archive.join("source/unlisted.tja")).unwrap();
        assert!(verify().status.success());
        std::fs::write(cli_archive.join("source/unsent/nested/draft.tja"), b"changed").unwrap();
        assert!(!verify().status.success());
        let manifest_path = archive.join("manifest.json");
        let original_manifest = std::fs::read(&manifest_path).unwrap();
        let mut omitted: serde_json::Value = serde_json::from_slice(&original_manifest).unwrap();
        omitted["files"].as_array_mut().unwrap().remove(0);
        std::fs::write(&manifest_path, omitted.to_string()).unwrap();
        assert!(p2p_sync_core::source_review::verify_archive(&archive).is_err());
        std::fs::write(&manifest_path, &original_manifest).unwrap();
        assert!(p2p_sync_core::source_review::verify_archive(&archive).is_ok());
        let mut forged: serde_json::Value =
            serde_json::from_slice(&original_manifest).unwrap();
        forged["files"][0]["path"] = serde_json::json!(["..", "outside"]);
        std::fs::write(&manifest_path, forged.to_string()).unwrap();
        assert!(p2p_sync_core::source_review::verify_archive(&archive).is_err());
        assert_eq!(std::fs::read(recovered_source.join("song.tja")).unwrap(), b"anchor exchange");
        std::fs::remove_dir_all(recovered_source.join("unsent")).unwrap();
        extern "C" fn capture_paths(context: *mut std::ffi::c_void, data: *const u8, len: usize) {
            let output = unsafe { &mut *(context as *mut Vec<u8>) };
            output.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
        }
        let [config, state, source] = [&second_config, &second_state, &recovered_source]
            .map(|path| path.to_str().unwrap().as_bytes());
        let ffi_archive = root.path().join("ffi-source-archive");
        let archive_path = ffi_archive.to_str().unwrap().as_bytes();
        let mut archive_json = Vec::new();
        assert_eq!(unsafe { p2p_sync_core::p2p_sync_writer_archive_recovered_source_v1(
            config.as_ptr(), config.len(), state.as_ptr(), state.len(),
            source.as_ptr(), source.len(), archive_path.as_ptr(), archive_path.len(),
            Some(capture_paths), &mut archive_json as *mut _ as *mut std::ffi::c_void,
        ) }, 0);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&archive_json).unwrap()["missing"],
            serde_json::json!([["next.tja"]]));
        let mut verified_json: Vec<u8> = Vec::new();
        assert_eq!(unsafe { p2p_sync_core::p2p_sync_verify_recovered_source_archive_v1(
            archive_path.as_ptr(), archive_path.len(), Some(capture_paths),
            &mut verified_json as *mut _ as *mut std::ffi::c_void,
        ) }, 0);
        assert_eq!(archive_json, verified_json);
        let mut paths = Vec::new();
        assert_eq!(unsafe { p2p_sync_core::p2p_sync_writer_recovery_candidates_v1(
            config.as_ptr(), config.len(), state.as_ptr(), state.len(),
            source.as_ptr(), source.len(), Some(capture_paths),
            &mut paths as *mut _ as *mut std::ffi::c_void,
        ) }, 0);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&paths).unwrap(),
            serde_json::json!([["next.tja"], ["song.tja"]]));
        assert_eq!(unsafe { p2p_sync_core::p2p_sync_writer_resume_after_recovery_v1(
            config.as_ptr(), config.len(), state.as_ptr(), state.len(),
            source.as_ptr(), source.len(),
        ) }, 9);
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
        assert_eq!(
            recovered.recovery_review_candidates(&recovered_source).unwrap(),
            vec![vec!["next.tja".to_owned()]]
        );
        assert!(matches!(
            recovered.resume_tree_refresh(&recovered_source),
            Err(HostError::RecoveryReviewRequired)
        ));
        std::fs::copy(&next_source, recovered_source.join("next.tja")).unwrap();
        assert_eq!(unsafe { p2p_sync_core::p2p_sync_writer_resume_after_recovery_v1(
            config.as_ptr(), config.len(), state.as_ptr(), state.len(),
            source.as_ptr(), source.len(),
        ) }, 0);
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
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receiver_config).unwrap()).unwrap();
        let connections = if config.get("recovery_anchor").is_some() { 3 } else { 1 };
        std::thread::scope(|threads| {
            let server = threads.spawn(|| {
                for _ in 0..connections {
                    let (socket, _) = listener.accept().unwrap();
                    sender.serve(socket).unwrap();
                }
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
    let [config_bytes, state_bytes, destination_bytes] =
        [&receiver_config, &state, &destination].map(|path| path.to_str().unwrap().as_bytes());
    let mut added = u64::MAX;
    assert_eq!(unsafe { p2p_sync_core::p2p_sync_recover_receiver_v1(
        config_bytes.as_ptr(), config_bytes.len(),
        state_bytes.as_ptr(), state_bytes.len(),
        destination_bytes.as_ptr(), destination_bytes.len(),
        &mut added, None, std::ptr::null_mut(),
    ) }, 0);
    assert_eq!(added, 0);
    extern "C" fn stop_now(_: *mut std::ffi::c_void) -> u8 { 1 }
    assert_eq!(unsafe { p2p_sync_core::p2p_sync_recover_receiver_v1(
        config_bytes.as_ptr(), config_bytes.len(),
        state_bytes.as_ptr(), state_bytes.len(),
        destination_bytes.as_ptr(), destination_bytes.len(),
        &mut added, Some(stop_now), std::ptr::null_mut(),
    ) }, 7);
    assert_eq!(added, 0);
    pull_once(&mut sender);
    assert!(!destination.join("song.tja").exists());
}

#[test]
fn sender_registers_first_anchor_only_after_catching_up() {
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup").arg(&setup)
        .args(["127.0.0.1:8786", "127.0.0.1:8786", "1", "2"])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let first_config = setup.join("sender/server.json");
    let second_config = setup.join("writer-2/server.json");
    let direct_config = setup.join("writer-2/direct.json");
    for config in [&first_config, &second_config] {
        let mut json: serde_json::Value = serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
        json["bind"] = "127.0.0.1:0".into();
        std::fs::write(config, json.to_string()).unwrap();
    }
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"signed history").unwrap();
    let mut first = Sender::open(&first_config, &root.path().join("first-state")).unwrap();
    let trusted = first.publish(&source).unwrap().peer;
    let listener = first.bind().unwrap();
    let mut direct: serde_json::Value = serde_json::from_slice(&std::fs::read(&direct_config).unwrap()).unwrap();
    direct["address"] = listener.local_addr().unwrap().to_string().into();
    direct.as_object_mut().unwrap().remove("fallback_peers");
    std::fs::write(&direct_config, direct.to_string()).unwrap();
    let second_state = root.path().join("second-state");
    let mut second = Sender::open(&second_config, &second_state).unwrap();
    let anchor = setup.join("registered-anchor.bin");
    assert!(matches!(second.register_recovery_anchor(
        &second_config, &direct_config, trusted, &second_state.join("bad-anchor.bin")),
        Err(HostError::Configuration)));
    let original = std::fs::read(&second_config).unwrap();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            for _ in 0..3 {
                let (socket, _) = listener.accept().unwrap();
                first.serve(socket).unwrap();
            }
        });
        assert!(matches!(second.register_recovery_anchor(
            &second_config, &direct_config, trusted, &anchor),
            Err(HostError::Recovery(RecoveryError::Behind(_)))));
        assert_eq!(std::fs::read(&second_config).unwrap(), original);
        assert!(!anchor.exists());
        assert_eq!(second.catch_up(&direct_config).unwrap(), 1);
        second.register_recovery_anchor(&second_config, &direct_config, trusted, &anchor).unwrap();
        server.join().unwrap();
    });
    assert!(anchor.is_file());
    let updated: serde_json::Value = serde_json::from_slice(&std::fs::read(&second_config).unwrap()).unwrap();
    assert_eq!(updated["recovery_anchor"]["file"].as_str().unwrap(), anchor.canonicalize().unwrap().to_str().unwrap());
    assert!(Sender::open(&second_config, &second_state).is_ok());
    assert!(matches!(second.register_recovery_anchor(
        &second_config, &direct_config, trusted, &setup.join("another-anchor.bin")),
        Err(HostError::Configuration)));
    let later = root.path().join("writer-later.tja");
    std::fs::write(&later, b"new signed event").unwrap();
    first.publish(&later).unwrap();
    let old_anchor = std::fs::read(&anchor).unwrap();
    extern "C" fn keep_running(_: *mut std::ffi::c_void) -> u8 { 0 }
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            for _ in 0..2 {
                let (socket, _) = listener.accept().unwrap();
                first.serve(socket).unwrap();
            }
        });
        let [config, state, peers] = [&second_config, &second_state, &direct_config]
            .map(|path| path.to_str().unwrap().as_bytes());
        let mut added = 0;
        assert_eq!(unsafe { p2p_sync_core::p2p_sync_writer_catch_up_v2(
            config.as_ptr(), config.len(), state.as_ptr(), state.len(),
            peers.as_ptr(), peers.len(), &mut added,
            Some(keep_running), std::ptr::null_mut(),
        ) }, 0);
        assert_eq!(added, 1);
        server.join().unwrap();
    });
    assert_ne!(std::fs::read(&anchor).unwrap(), old_anchor);
    assert!(Sender::open(&second_config, &second_state).is_ok());
    let mut serving = Sender::open(&second_config, &second_state).unwrap();
    let future = root.path().join("writer-future.tja");
    std::fs::write(&future, b"not yet imported").unwrap();
    first.publish(&future).unwrap();
    let future_anchor = setup.join("future-anchor.bin");
    first.export_recovery_anchor(&future_anchor).unwrap();
    std::fs::copy(&future_anchor, &anchor).unwrap();
    let listener = serving.bind().unwrap();
    let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (socket, _) = listener.accept().unwrap();
    assert!(matches!(serving.serve(socket),
        Err(HostError::Recovery(RecoveryError::Behind(_)))));
}

#[test]
fn receiver_registers_pinned_anchor_only_after_catching_up() {
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup").arg(&setup)
        .args(["127.0.0.1:8787", "127.0.0.1:8787", "1", "1"])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let sender_config = setup.join("sender/server.json");
    let receiver_config = setup.join("receiver/direct.json");
    let mut sender_json: serde_json::Value = serde_json::from_slice(&std::fs::read(&sender_config).unwrap()).unwrap();
    sender_json["bind"] = "127.0.0.1:0".into();
    std::fs::write(&sender_config, sender_json.to_string()).unwrap();
    let state = root.path().join("receiver-state");
    let destination = root.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"verified receiver content").unwrap();
    let mut sender = Sender::open(&sender_config, &root.path().join("sender-state")).unwrap();
    sender.publish(&source).unwrap();
    let listener = sender.bind().unwrap();
    let mut direct: serde_json::Value = serde_json::from_slice(&std::fs::read(&receiver_config).unwrap()).unwrap();
    direct["address"] = listener.local_addr().unwrap().to_string().into();
    std::fs::write(&receiver_config, direct.to_string()).unwrap();
    let anchor = setup.join("receiver-registered-anchor.bin");
    assert!(matches!(p2p_sync_core::host::register_receiver_recovery_anchor(
        &receiver_config, &state, &destination, &destination.join("bad-anchor.bin")),
        Err(HostError::Configuration)));
    let original = std::fs::read(&receiver_config).unwrap();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            for _ in 0..6 {
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
            }
        });
        let register = || std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("register-receiver-recovery-anchor")
            .arg(&receiver_config).arg(&state).arg(&destination).arg(&anchor)
            .output().unwrap();
        let rejected = register();
        assert!(!rejected.status.success());
        assert_eq!(std::fs::read(&receiver_config).unwrap(), original);
        assert!(!anchor.exists());
        pull_from_config(&receiver_config, &state, &destination).unwrap();
        assert_eq!(std::fs::read(destination.join("song.tja")).unwrap(), b"verified receiver content");
        let registered = register();
        assert!(registered.status.success(), "{}", String::from_utf8_lossy(&registered.stderr));
        assert_eq!(std::fs::read(destination.join("song.tja")).unwrap(), b"verified receiver content");
        pull_from_config(&receiver_config, &state, &destination).unwrap();
        server.join().unwrap();
    });
    assert!(anchor.is_file());
    let updated: serde_json::Value = serde_json::from_slice(&std::fs::read(&receiver_config).unwrap()).unwrap();
    assert_eq!(updated["recovery_anchor"]["file"].as_str().unwrap(), anchor.canonicalize().unwrap().to_str().unwrap());
    assert!(matches!(p2p_sync_core::host::register_receiver_recovery_anchor(
        &receiver_config, &state, &destination, &setup.join("another-anchor.bin")),
        Err(HostError::Configuration)));
    let later = root.path().join("later.tja");
    std::fs::write(&later, b"later receiver content").unwrap();
    sender.publish(&later).unwrap();
    let old_anchor = std::fs::read(&anchor).unwrap();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            for _ in 0..5 {
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
            }
        });
        let refresh = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
            .arg("refresh-receiver-recovery-anchor")
            .arg(&receiver_config).arg(&state).arg(&destination)
            .output().unwrap();
        assert!(refresh.status.success(), "{}", String::from_utf8_lossy(&refresh.stderr));
        assert!(String::from_utf8_lossy(&refresh.stdout).contains("Imported 1 event(s)"));
        assert!(!destination.join("later.tja").exists());
        assert_ne!(std::fs::read(&anchor).unwrap(), old_anchor);
        pull_from_config(&receiver_config, &state, &destination).unwrap();
        server.join().unwrap();
    });
    assert_eq!(std::fs::read(destination.join("later.tja")).unwrap(), b"later receiver content");
    let automatic = root.path().join("automatic.tja");
    std::fs::write(&automatic, b"automatic anchor update").unwrap();
    sender.publish(&automatic).unwrap();
    let old_anchor = std::fs::read(&anchor).unwrap();
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            for _ in 0..3 {
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
            }
        });
        pull_from_config(&receiver_config, &state, &destination).unwrap();
        server.join().unwrap();
    });
    assert_ne!(std::fs::read(&anchor).unwrap(), old_anchor);
    assert_eq!(std::fs::read(destination.join("automatic.tja")).unwrap(), b"automatic anchor update");
    let pending = root.path().join("before-pending.tja");
    std::fs::write(&pending, b"preflight keeps destination unchanged").unwrap();
    sender.publish(&pending).unwrap();
    let old_anchor = std::fs::read(&anchor).unwrap();
    extern "C" fn keep_running(_: *mut std::ffi::c_void) -> u8 { 0 }
    let [config, storage, target] = [&receiver_config, &state, &destination]
        .map(|path| path.to_str().unwrap().as_bytes());
    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            for _ in 0..2 {
                let (socket, _) = listener.accept().unwrap();
                sender.serve(socket).unwrap();
            }
        });
        assert_eq!(unsafe { p2p_sync_core::p2p_sync_receiver_anchor_preflight_v1(
            config.as_ptr(), config.len(), storage.as_ptr(), storage.len(),
            target.as_ptr(), target.len(), Some(keep_running), std::ptr::null_mut(),
        ) }, 0);
        server.join().unwrap();
    });
    assert_ne!(std::fs::read(&anchor).unwrap(), old_anchor);
    assert!(!destination.join("before-pending.tja").exists());
    let future = root.path().join("after-preflight.tja");
    std::fs::write(&future, b"newer trusted history").unwrap();
    sender.publish(&future).unwrap();
    let future_anchor = setup.join("receiver-future-anchor.bin");
    sender.export_recovery_anchor(&future_anchor).unwrap();
    std::fs::copy(&future_anchor, &anchor).unwrap();
    assert_eq!(unsafe { p2p_sync_core::p2p_sync_receiver_anchor_preflight_v1(
        config.as_ptr(), config.len(), storage.as_ptr(), storage.len(),
        target.as_ptr(), target.len(), Some(keep_running), std::ptr::null_mut(),
    ) }, 8);
    assert!(!destination.join("before-pending.tja").exists());
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
