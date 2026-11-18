use p2p_sync_core::{auth::*, catchup::*, replica::*, store::Store};

#[test]
fn reconnect_replays_only_missing_signed_events_and_recovers_reverse_delivery() {
    let key = PeerSigningKey::from_bytes(&[11; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([1; 16], 1, [(peer, Role::Writer)].into());
    let mut sender = Store::open(":memory:", roster.membership().clone()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("receiver.db");
    let mut receiver = Store::open(&db, roster.membership().clone()).unwrap();

    for sequence in 1..=(MAX_PAGE_EVENTS as u64 + 2) {
        let event = Event {
            id: EventId { peer, sequence },
            context: sender.replica().clock().clone(),
            object: [8; 16],
            change: if sequence == 1 {
                Change::Create(Entry {
                    parent: None,
                    name: "song.tja".into(),
                    kind: EntryKind::File([1; 32]),
                })
            } else {
                Change::Revise {
                    base: EventId {
                        peer,
                        sequence: sequence - 1,
                    },
                    entry: Some(Entry {
                        parent: None,
                        name: "song.tja".into(),
                        kind: EntryKind::File([sequence as u8; 32]),
                    }),
                }
            },
        };
        assert_eq!(
            roster
                .receive(&mut sender, &roster.sign_event(&key, &event).unwrap())
                .unwrap(),
            Apply::Added
        );
    }

    let first = outgoing_page(&mut sender, receiver.replica().clock()).unwrap();
    assert!(first.has_more);
    assert_eq!(first.events.len(), MAX_PAGE_EVENTS);

    let unexpected = receive_page(&mut receiver, &roster, &first.events[1..2]).unwrap();
    assert_eq!(unexpected.deferred, 1);
    assert_eq!(
        unexpected.missing_dependencies,
        [EventId { peer, sequence: 1 }].into()
    );
    assert!(receiver.replica().clock().is_empty());

    let report = receive_page(
        &mut receiver,
        &roster,
        &[first.events[2], first.events[1], first.events[0]],
    )
    .unwrap();
    assert_eq!((report.added, report.deferred), (3, 0));
    assert_eq!(
        receive_page(&mut receiver, &roster, &first.events[..1])
            .unwrap()
            .duplicates,
        1
    );
    drop(receiver);

    let mut receiver = Store::open(&db, roster.membership().clone()).unwrap();
    let rest = outgoing_page(&mut sender, receiver.replica().clock()).unwrap();
    assert_eq!(rest.events.len(), MAX_PAGE_EVENTS - 1);
    assert!(!rest.has_more);
    assert_eq!(
        receive_page(&mut receiver, &roster, &rest.events)
            .unwrap()
            .added,
        MAX_PAGE_EVENTS - 1
    );
    assert_eq!(sender.replica().clock(), receiver.replica().clock());
    assert!(outgoing_page(&mut sender, receiver.replica().clock())
        .unwrap()
        .events
        .is_empty());
}
