use p2p_sync_core::{
    auth::PeerSigningKey,
    host::{HostError, Sender},
    recovery::RecoveryError,
};

#[test]
fn configured_peer_anchor_stops_publication_after_database_and_witness_rollback() {
    let root = tempfile::tempdir().unwrap();
    let setup = root.path().join("setup");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_p2p-send"))
        .arg("setup")
        .arg(&setup)
        .args(["127.0.0.1:8780", "127.0.0.1:8780", "1", "2"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first_config = setup.join("sender/server.json");
    let second_config = setup.join("writer-2/server.json");
    let state = root.path().join("state");
    let source = root.path().join("song.tja");
    std::fs::write(&source, b"original contents").unwrap();
    Sender::open(&first_config, &state)
        .unwrap()
        .publish(&source)
        .unwrap();

    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&first_config).unwrap()).unwrap();
    let share = settings["share"].as_str().unwrap();
    let database = state.join(share).join("1/sync.db");
    let witness = state.join(share).join("witness/1/sender");
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

    Sender::open(&first_config, &state)
        .unwrap()
        .revise("song.tja", None)
        .unwrap();
    let peer_key = PeerSigningKey::from_bytes(
        &std::fs::read(setup.join("writer-2/event-key.bin"))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let peer: String = peer_key
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let anchor = setup.join("peer-anchor.bin");
    Sender::open(&second_config, &state)
        .unwrap()
        .export_recovery_anchor(&anchor)
        .unwrap();
    settings["recovery_anchor"] = serde_json::json!({"file": "../peer-anchor.bin", "peer": peer});
    std::fs::write(&first_config, settings.to_string()).unwrap();
    assert!(Sender::open(&first_config, &state).is_ok());

    std::fs::copy(&old_database, &database).unwrap();
    std::fs::remove_dir_all(&witness).unwrap();
    std::fs::rename(&old_witness, &witness).unwrap();
    assert!(matches!(
        Sender::open(&first_config, &state),
        Err(HostError::Recovery(RecoveryError::Behind(_)))
    ));
    assert_eq!(std::fs::read(&source).unwrap(), b"original contents");
}
