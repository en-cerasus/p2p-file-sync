use std::net::TcpListener;

use p2p_sync_core::{
    host::{HostError, Sender},
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
    std::fs::write(&source, b"anchor exchange").unwrap();
    let mut first = Sender::open(&first_config, &first_state).unwrap();
    let trusted_peer = first.publish(&source).unwrap().peer;
    let listener: TcpListener = first.bind().unwrap();
    let mut direct: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&direct_config).unwrap()).unwrap();
    direct["address"] = listener.local_addr().unwrap().to_string().into();
    direct.as_object_mut().unwrap().remove("fallback_peers");
    std::fs::write(&direct_config, direct.to_string()).unwrap();

    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
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
        server.join().unwrap();
        Sender::open(&second_config, &second_state).unwrap();
    });
}
