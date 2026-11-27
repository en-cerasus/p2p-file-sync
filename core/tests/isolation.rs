use p2p_sync_core::{
    auth::PeerSigningKey,
    host::{verify_isolated_archive_from_config, Sender},
    isolation::{archive_isolated_generation, verify_isolated_archive},
    p2p_sync_verify_isolated_archive_v1, sha256,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn offline_writer_edits_are_archived_without_republishing_or_changing_old_state() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let setup = root.join("setup");
    let created = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8764", "127.0.0.1:8764", "1", "2"])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let original = setup.join("sender/server.json");
    let offline = setup.join("writer-2/server.json");
    let main_state = root.join("main-state");
    let offline_state = root.join("offline-state");
    let baseline_source = root.join("baseline.tja");
    std::fs::write(&baseline_source, b"before migration").unwrap();
    let mut main = Sender::open(&original, &main_state).unwrap();
    main.publish(&baseline_source).unwrap();

    let revoked_seed = std::fs::read(setup.join("writer-2/event-key.bin")).unwrap();
    let revoked = PeerSigningKey::from_bytes(revoked_seed.as_slice().try_into().unwrap());
    let revoked_peer = hex(revoked.verifying_key().as_bytes());
    let revoked_pin = hex(&sha256(
        &std::fs::read(setup.join("writer-2/certificate.der")).unwrap(),
    ));
    let next = setup.join("sender/next.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&original).unwrap()).unwrap();
    config["generation"] = 2.into();
    config["members"]
        .as_object_mut()
        .unwrap()
        .remove(&revoked_peer)
        .unwrap();
    config["client_identities"]
        .as_object_mut()
        .unwrap()
        .remove(&revoked_peer)
        .unwrap();
    config["client_pins"]
        .as_array_mut()
        .unwrap()
        .retain(|pin| pin != &revoked_pin);
    std::fs::write(&next, config.to_string()).unwrap();
    let admin_seed = root.join("admin-seed.bin");
    let admin_public = root.join("admin-public.bin");
    let admin = PeerSigningKey::from_bytes(&[67; 32]);
    std::fs::write(&admin_seed, admin.to_bytes()).unwrap();
    std::fs::write(&admin_public, admin.verifying_key().as_bytes()).unwrap();
    let bundle = root.join("migration");
    main.prepare_generation(&next, &admin_seed, &bundle)
        .unwrap();
    drop(main);

    let late_source = root.join("late.tja");
    let late_content = b"offline writer edit after checkpoint";
    std::fs::write(&late_source, late_content).unwrap();
    let mut old_writer = Sender::open(&offline, &offline_state).unwrap();
    let late_event = old_writer.publish(&late_source).unwrap();
    drop(old_writer);
    let share = config["share"].as_str().unwrap().to_owned();
    let database = offline_state.join(&share).join("1/sync.db");
    let before = std::fs::read(&database).unwrap();
    let archive = root.join("isolated");
    let report = archive_isolated_generation(
        &offline,
        &offline_state,
        &next,
        &admin_public,
        &bundle,
        &archive,
    )
    .unwrap();
    assert_eq!(report.events.len(), 1);
    assert_eq!(report.events[0].sequence, late_event.sequence);
    assert_eq!(report.copied_blobs, 1);
    assert!(report.missing_blobs.is_empty());
    assert_eq!(std::fs::read(&database).unwrap(), before);
    assert_eq!(std::fs::read(&late_source).unwrap(), late_content);
    assert_eq!(
        std::fs::read(archive.join("files").join(hex(&sha256(late_content)))).unwrap(),
        late_content
    );
    let events: Vec<Vec<u8>> =
        postcard::from_bytes(&std::fs::read(archive.join("events.bin")).unwrap()).unwrap();
    assert_eq!(events.len(), 1);
    assert!(archive.join("report.json").is_file());
    assert!(archive.join("old-sync.db").is_file());
    assert_eq!(
        verify_isolated_archive(&offline, &next, &admin_public, &bundle, &archive).unwrap(),
        report
    );
    config["migration"] = serde_json::json!({
        "previous_server": original, "administrator_public_key": admin_public, "bundle": bundle,
    });
    std::fs::write(&next, config.to_string()).unwrap();
    assert_eq!(
        verify_isolated_archive_from_config(&next, &archive).unwrap(),
        report
    );
    extern "C" fn collect(context: *mut std::ffi::c_void, data: *const u8, length: usize) {
        unsafe {
            (*context.cast::<Vec<u8>>())
                .extend_from_slice(std::slice::from_raw_parts(data, length));
        }
    }
    let config_bytes = next.to_str().unwrap().as_bytes();
    let archive_bytes = archive.to_str().unwrap().as_bytes();
    let mut json = Vec::new();
    let status = unsafe {
        p2p_sync_verify_isolated_archive_v1(
            config_bytes.as_ptr(),
            config_bytes.len(),
            archive_bytes.as_ptr(),
            archive_bytes.len(),
            Some(collect),
            (&mut json as *mut Vec<u8>).cast(),
        )
    };
    assert_eq!(status, 0);
    let ffi_report: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(ffi_report["events"][0]["path_hint"], "late.tja");
    let verify_cli = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("verify-isolated-archive")
        .args([&offline, &next, &admin_public, &bundle, &archive])
        .output()
        .unwrap();
    assert!(
        verify_cli.status.success(),
        "{}",
        String::from_utf8_lossy(&verify_cli.stderr)
    );
    assert!(String::from_utf8_lossy(&verify_cli.stdout).contains("1 isolated event(s)"));
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(archive.join("report.json")).unwrap()).unwrap();
    assert_eq!(summary["events"][0]["operation"], "file");
    assert_eq!(summary["events"][0]["change"], "create");
    assert_eq!(summary["events"][0]["path_hint"], "late.tja");
    let report_path = archive.join("report.json");
    let original_report = std::fs::read(&report_path).unwrap();
    let mut changed_report: serde_json::Value = serde_json::from_slice(&original_report).unwrap();
    changed_report["events"][0]["sequence"] = 99.into();
    std::fs::write(&report_path, changed_report.to_string()).unwrap();
    assert!(verify_isolated_archive(&offline, &next, &admin_public, &bundle, &archive).is_err());
    std::fs::write(&report_path, original_report).unwrap();
    let event_path = archive.join("events.bin");
    let original_events = std::fs::read(&event_path).unwrap();
    let mut changed_events = original_events.clone();
    *changed_events.last_mut().unwrap() ^= 1;
    std::fs::write(&event_path, changed_events).unwrap();
    assert!(verify_isolated_archive(&offline, &next, &admin_public, &bundle, &archive).is_err());
    std::fs::write(&event_path, original_events).unwrap();
    assert!(archive_isolated_generation(
        &offline,
        &offline_state,
        &next,
        &admin_public,
        &bundle,
        &archive,
    )
    .is_err());

    let cli_archive = root.join("isolated-cli");
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("archive-isolated-generation")
        .args([
            &offline,
            &offline_state,
            &next,
            &admin_public,
            &bundle,
            &cli_archive,
        ])
        .output()
        .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert!(String::from_utf8_lossy(&cli.stdout).contains("1 isolated event(s)"));

    let blob = offline_state
        .join(&share)
        .join("1/files")
        .join(hex(&sha256(late_content)));
    let held_blob = root.join("held-blob");
    std::fs::rename(&blob, &held_blob).unwrap();
    let missing_archive = root.join("isolated-missing");
    let missing = archive_isolated_generation(
        &offline,
        &offline_state,
        &next,
        &admin_public,
        &bundle,
        &missing_archive,
    )
    .unwrap();
    assert_eq!(missing.events.len(), 1);
    assert_eq!(missing.copied_blobs, 0);
    assert_eq!(missing.missing_blobs, vec![hex(&sha256(late_content))]);
    assert_eq!(
        verify_isolated_archive(&offline, &next, &admin_public, &bundle, &missing_archive).unwrap(),
        missing
    );
    std::fs::rename(&held_blob, &blob).unwrap();
    assert_eq!(std::fs::read(&database).unwrap(), before);

    let wrong_admin = root.join("wrong-admin.bin");
    std::fs::write(
        &wrong_admin,
        PeerSigningKey::from_bytes(&[68; 32])
            .verifying_key()
            .as_bytes(),
    )
    .unwrap();
    let rejected = root.join("rejected-admin");
    assert!(archive_isolated_generation(
        &offline,
        &offline_state,
        &next,
        &wrong_admin,
        &bundle,
        &rejected,
    )
    .is_err());
    assert!(!rejected.exists());

    std::fs::write(&blob, b"corrupted").unwrap();
    let corrupt_archive = root.join("isolated-corrupt");
    assert!(archive_isolated_generation(
        &offline,
        &offline_state,
        &next,
        &admin_public,
        &bundle,
        &corrupt_archive,
    )
    .is_err());
    assert!(!corrupt_archive.join("report.json").exists());
    std::fs::write(&blob, late_content).unwrap();
}
