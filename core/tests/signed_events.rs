use p2p_sync_core::{auth::*, replica::*, store::Store};

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
