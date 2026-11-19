use p2p_sync_core::{auth::*, replica::*, store::{Store, StoreError}};

fn event(key: &PeerSigningKey) -> Event {
    Event {
        id: EventId {
            peer: key.verifying_key().to_bytes(),
            sequence: 1,
        },
        context: VersionVector::new(),
        object: [8; 16],
        change: Change::Create(Entry {
            parent: None,
            name: "譜面.tja".into(),
            kind: EntryKind::File([9; 32]),
        }),
    }
}

#[test]
fn only_the_authorized_generation_can_apply_and_forward_an_event() {
    let key = PeerSigningKey::from_bytes(&[2; 32]);
    let roster = Roster::new([3; 16], 4, [(event(&key).id.peer, Role::Writer)].into());
    let item = event(&key);
    let wire = roster.sign_event(&key, &item).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sync.db");
    let mut store = Store::open(&db, roster.membership().clone()).unwrap();

    let mut altered = wire.clone();
    altered[wire.len() - 70] ^= 1;
    assert!(matches!(
        roster.receive(&mut store, &altered),
        Err(AuthError::BadSignature)
    ));
    assert!(store.replica().get(item.id).is_none());
    for (share, generation) in [([7; 16], 4), ([3; 16], 5)] {
        let other = Roster::new(share, generation, [(item.id.peer, Role::Writer)].into());
        assert!(matches!(
            other.verify_event(&wire),
            Err(AuthError::WrongScope)
        ));
    }

    assert_eq!(roster.receive(&mut store, &wire).unwrap(), Apply::Added);
    assert_eq!(roster.receive(&mut store, &wire).unwrap(), Apply::Duplicate);
    drop(store);
    let reopened = Store::open(&db, roster.membership().clone()).unwrap();
    let forwarded = reopened.signed_event(item.id).unwrap();
    assert_eq!(forwarded, wire);
    assert_eq!(roster.verify_event(forwarded).unwrap(), item);
}

#[test]
fn an_old_writer_backup_cannot_reuse_a_delete_event_id_to_revive_a_file() {
    let key = PeerSigningKey::from_bytes(&[13; 32]);
    let created = event(&key);
    let roster = Roster::new([3; 16], 4, [(created.id.peer, Role::Writer)].into());
    let deleted = Event {
        id: EventId { sequence: 2, ..created.id },
        context: [(created.id.peer, 1)].into(),
        object: created.object,
        change: Change::Revise { base: created.id, entry: None },
    };
    let revived = Event {
        change: Change::Revise {
            base: created.id,
            entry: Some(Entry {
                parent: None,
                name: "譜面.tja".into(),
                kind: EntryKind::File([10; 32]),
            }),
        },
        ..deleted.clone()
    };
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("recipient.db");
    let mut recipient = Store::open(&database, roster.membership().clone()).unwrap();
    let mut restored_backup = Store::open(":memory:", roster.membership().clone()).unwrap();
    let first = roster.sign_event(&key, &created).unwrap();
    assert_eq!(roster.receive(&mut recipient, &first).unwrap(), Apply::Added);
    assert_eq!(roster.receive(&mut restored_backup, &first).unwrap(), Apply::Added);
    let tombstone = roster.sign_event(&key, &deleted).unwrap();
    assert_eq!(roster.receive(&mut recipient, &tombstone).unwrap(), Apply::Added);
    let fork = roster.sign_event(&key, &revived).unwrap();
    assert_eq!(roster.receive(&mut restored_backup, &fork).unwrap(), Apply::Added);
    assert!(matches!(
        roster.receive(&mut recipient, &fork),
        Err(AuthError::Store(StoreError::Rejected(Reject::EventIdReuse)))
    ));
    drop(recipient);
    let recipient = Store::open(&database, roster.membership().clone()).unwrap();
    assert_eq!(recipient.replica().heads(created.object).next().unwrap().id, deleted.id);
    assert_eq!(recipient.replica().heads(created.object).count(), 1);
}

#[test]
fn valid_signatures_from_readers_cannot_publish_changes() {
    let key = PeerSigningKey::from_bytes(&[2; 32]);
    let candidate = event(&key);
    let signer = Roster::new([3; 16], 4, [(candidate.id.peer, Role::Writer)].into());
    let roster = Roster::new([3; 16], 4, [(candidate.id.peer, Role::Reader)].into());
    let wire = signer.sign_event(&key, &candidate).unwrap();
    assert!(matches!(
        roster.sign_event(&key, &candidate),
        Err(AuthError::NotWriter)
    ));
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
    assert!(matches!(
        roster.receive(&mut store, &wire),
        Err(AuthError::NotWriter)
    ));
    assert!(store.replica().clock().is_empty());
}

#[test]
fn reader_confirmations_are_bound_to_their_signer_and_enable_retention_progress() {
    use p2p_sync_core::retention::{Confirmation, PeerState};
    let writer = PeerSigningKey::from_bytes(&[6; 32]);
    let reader = PeerSigningKey::from_bytes(&[7; 32]);
    let root = event(&writer);
    let reader_id = reader.verifying_key().to_bytes();
    let roles = [(root.id.peer, Role::Writer), (reader_id, Role::Reader)].into();
    let roster = Roster::new([3; 16], 4, roles);
    let folder = tempfile::tempdir().unwrap();
    let db = folder.path().join("confirmation.db");
    let mut store = Store::open(&db, roster.membership().clone()).unwrap();
    roster
        .receive(&mut store, &roster.sign_event(&writer, &root).unwrap())
        .unwrap();
    let successor = Event {
        id: EventId {
            sequence: 2,
            ..root.id
        },
        context: [(root.id.peer, 1)].into(),
        object: root.object,
        change: Change::Revise {
            base: root.id,
            entry: None,
        },
    };
    roster
        .receive(&mut store, &roster.sign_event(&writer, &successor).unwrap())
        .unwrap();

    let acknowledgement = Confirmation {
        generation: 4,
        peer: reader_id,
        object: root.object,
        successor: successor.id,
        sent_through: 0,
    };
    let wire = roster.sign_confirmation(&reader, &acknowledgement).unwrap();
    assert!(matches!(
        roster.sign_confirmation(&writer, &acknowledgement),
        Err(AuthError::InvalidConfirmation)
    ));
    assert!(matches!(
        roster.verify_event(&wire),
        Err(AuthError::InvalidFrame)
    ));
    let mut tampered = wire.clone();
    *tampered.last_mut().unwrap() ^= 1;
    assert!(matches!(
        roster.receive_confirmation(&mut store, &tampered),
        Err(AuthError::BadSignature)
    ));

    let newer = Roster::new(
        [3; 16],
        5,
        [(root.id.peer, Role::Writer), (reader_id, Role::Reader)].into(),
    );
    assert!(matches!(
        newer.verify_confirmation(&wire),
        Err(AuthError::WrongScope)
    ));
    roster.receive_confirmation(&mut store, &wire).unwrap();
    drop(store);

    let store = Store::open(&db, roster.membership().clone()).unwrap();
    let protected = store.protections().next().unwrap();
    assert_eq!(
        protected
            .peer_states(store.replica().clock())
            .find(|(peer, _)| *peer == reader_id),
        Some((reader_id, PeerState::Applied))
    );
    assert!(!protected.can_release(store.replica().clock()));
}
