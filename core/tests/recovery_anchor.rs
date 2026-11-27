use p2p_sync_core::{
    auth::{PeerSigningKey, Role, Roster},
    recovery::{sign_anchor, verify_anchor, verify_anchor_advance, verify_remote_author, RecoveryError},
    replica::{Change, Entry, EntryKind, Event, EventId, VersionVector},
    store::Store,
};

fn event(peer: [u8; 32], sequence: u64, deletion: bool) -> Event {
    Event {
        id: EventId { peer, sequence },
        context: if sequence == 1 {
            VersionVector::new()
        } else {
            [(peer, sequence - 1)].into()
        },
        object: [3; 16],
        change: if deletion {
            Change::Revise {
                base: EventId {
                    peer,
                    sequence: sequence - 1,
                },
                entry: None,
            }
        } else {
            Change::Create(Entry {
                parent: None,
                name: "kept.txt".into(),
                kind: EntryKind::File([8; 32]),
            })
        },
    }
}

#[test]
fn peer_anchor_detects_database_and_witness_rollback_then_accepts_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sync.db");
    let old_db = dir.path().join("old.db");
    let witness = dir.path().join("witness");
    let old_witness = dir.path().join("old-witness");
    std::fs::create_dir(&old_witness).unwrap();
    let writer = PeerSigningKey::from_bytes(&[21; 32]);
    let reader = PeerSigningKey::from_bytes(&[22; 32]);
    let author = writer.verifying_key().to_bytes();
    let trusted = reader.verifying_key().to_bytes();
    let roster = Roster::new(
        [4; 16],
        1,
        [(author, Role::Writer), (trusted, Role::Reader)].into(),
    );
    let first = roster
        .sign_event(&writer, &event(author, 1, false))
        .unwrap();
    let deletion = roster.sign_event(&writer, &event(author, 2, true)).unwrap();

    {
        let mut store = Store::open(&db, roster.membership().clone()).unwrap();
        store.enable_event_witness(&witness).unwrap();
        roster.receive(&mut store, &first).unwrap();
    }
    std::fs::copy(&db, &old_db).unwrap();
    for entry in std::fs::read_dir(&witness).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), old_witness.join(entry.file_name())).unwrap();
    }
    let anchor = {
        let mut store = Store::open(&db, roster.membership().clone()).unwrap();
        store.enable_event_witness(&witness).unwrap();
        roster.receive(&mut store, &deletion).unwrap();
        sign_anchor(&roster, &mut store, &reader).unwrap()
    };

    std::fs::copy(&old_db, &db).unwrap();
    std::fs::remove_dir_all(&witness).unwrap();
    std::fs::rename(&old_witness, &witness).unwrap();
    let mut restored = Store::open(&db, roster.membership().clone()).unwrap();
    restored.enable_event_witness(&witness).unwrap();
    assert!(
        matches!(verify_anchor(&roster, &mut restored, &anchor, trusted), Err(RecoveryError::Behind(id)) if id == EventId { peer: author, sequence: 2 })
    );

    // Replaying the missing signed deletion restores the trusted prefix.
    roster.receive(&mut restored, &deletion).unwrap();
    verify_anchor(&roster, &mut restored, &anchor, trusted).unwrap();
}

#[test]
fn anchor_requires_trusted_signer_valid_signature_and_same_generation() {
    let dir = tempfile::tempdir().unwrap();
    let writer = PeerSigningKey::from_bytes(&[23; 32]);
    let reader = PeerSigningKey::from_bytes(&[24; 32]);
    let author = writer.verifying_key().to_bytes();
    let trusted = reader.verifying_key().to_bytes();
    let roles = [(author, Role::Writer), (trusted, Role::Reader)].into();
    let roster = Roster::new([4; 16], 1, roles);
    let mut store = Store::open(dir.path().join("sync.db"), roster.membership().clone()).unwrap();
    roster
        .receive(
            &mut store,
            &roster
                .sign_event(&writer, &event(author, 1, false))
                .unwrap(),
        )
        .unwrap();
    let anchor = sign_anchor(&roster, &mut store, &reader).unwrap();
    verify_anchor(&roster, &mut store, &anchor, trusted).unwrap();
    assert!(matches!(
        verify_anchor(&roster, &mut store, &anchor, author),
        Err(RecoveryError::UntrustedSigner)
    ));
    let mut corrupted = anchor.clone();
    let last = corrupted.len() - 1;
    corrupted[last] ^= 1;
    assert!(verify_anchor(&roster, &mut store, &corrupted, trusted).is_err());
    let other_generation = Roster::new(
        [4; 16],
        2,
        [(author, Role::Writer), (trusted, Role::Reader)].into(),
    );
    assert!(verify_anchor(&other_generation, &mut store, &anchor, trusted).is_err());
}

#[test]
fn anchor_rejects_different_signed_history_at_same_frontier() {
    let dir = tempfile::tempdir().unwrap();
    let writer = PeerSigningKey::from_bytes(&[25; 32]);
    let author = writer.verifying_key().to_bytes();
    let roster = Roster::new([4; 16], 1, [(author, Role::Writer)].into());
    let mut original =
        Store::open(dir.path().join("original.db"), roster.membership().clone()).unwrap();
    let mut divergent =
        Store::open(dir.path().join("divergent.db"), roster.membership().clone()).unwrap();
    let first = event(author, 1, false);
    let mut changed = first.clone();
    if let Change::Create(entry) = &mut changed.change {
        entry.name = "other.txt".into();
    }
    roster
        .receive(&mut original, &roster.sign_event(&writer, &first).unwrap())
        .unwrap();
    roster
        .receive(
            &mut divergent,
            &roster.sign_event(&writer, &changed).unwrap(),
        )
        .unwrap();
    let anchor = sign_anchor(&roster, &mut original, &writer).unwrap();
    assert!(
        matches!(verify_anchor(&roster, &mut divergent, &anchor, author), Err(RecoveryError::Diverged(id)) if id == first.id)
    );
}

#[test]
fn remote_author_cannot_claim_less_history_than_a_receiver_already_has() {
    let dir = tempfile::tempdir().unwrap();
    let writer = PeerSigningKey::from_bytes(&[26; 32]);
    let peer = writer.verifying_key().to_bytes();
    let roster = Roster::new([4; 16], 1, [(peer, Role::Writer)].into());
    let first = roster.sign_event(&writer, &event(peer, 1, false)).unwrap();
    let deletion = roster.sign_event(&writer, &event(peer, 2, true)).unwrap();
    let mut old = Store::open(dir.path().join("old.db"), roster.membership().clone()).unwrap();
    let mut current =
        Store::open(dir.path().join("current.db"), roster.membership().clone()).unwrap();
    roster.receive(&mut old, &first).unwrap();
    roster.receive(&mut current, &first).unwrap();
    let old_anchor = sign_anchor(&roster, &mut old, &writer).unwrap();
    verify_remote_author(&roster, &mut current, &old_anchor, peer).unwrap();
    roster.receive(&mut current, &deletion).unwrap();
    assert!(matches!(
        verify_remote_author(&roster, &mut current, &old_anchor, peer),
        Err(RecoveryError::RemoteBehind(id)) if id == EventId { peer, sequence: 2 }
    ));
    let current_anchor = sign_anchor(&roster, &mut current, &writer).unwrap();
    verify_remote_author(&roster, &mut old, &current_anchor, peer).unwrap();
    verify_anchor_advance(&roster, &mut current, &old_anchor, &current_anchor, peer).unwrap();
    assert!(matches!(
        verify_anchor_advance(&roster, &mut current, &current_anchor, &old_anchor, peer),
        Err(RecoveryError::StaleAnchor(id)) if id == EventId { peer, sequence: 2 }
    ));
}
