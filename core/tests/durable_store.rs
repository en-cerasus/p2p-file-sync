use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use p2p_sync_core::{
    auth::{PeerSigningKey, Role, Roster},
    replica::*,
    retention::*,
    store::*,
};
use rusqlite::Connection;

const WRITER: PeerId = [1; 32];
const OFFLINE: PeerId = [2; 32];
const OBJECT: ObjectId = [3; 16];

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let folder = std::env::temp_dir().join(format!(
            "p2p-sync-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&folder).unwrap();
        Self(folder)
    }

    fn path(&self) -> PathBuf {
        self.0.join("sync.db")
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let path: &Path = &self.0;
        let _ = std::fs::remove_file(path.join("sync.db"));
        let _ = std::fs::remove_dir(path);
    }
}

fn membership() -> Membership {
    Membership {
        share: [4; 16],
        generation: 1,
        members: [WRITER, OFFLINE].into(),
    }
}

#[test]
fn signed_deletion_witness_rejects_an_older_database() {
    let folder = tempfile::tempdir().unwrap();
    let database = folder.path().join("sync.db");
    let backup = folder.path().join("before-deletion.db");
    let witness = folder.path().join("witness");
    let key = PeerSigningKey::from_bytes(&[77; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([4; 16], 1, [(peer, Role::Writer)].into());
    let created = Event {
        id: EventId { peer, sequence: 1 },
        context: VersionVector::new(),
        object: OBJECT,
        change: Change::Create(entry(5)),
    };
    let deleted = Event {
        id: EventId { peer, sequence: 2 },
        context: [(peer, 1)].into(),
        object: OBJECT,
        change: Change::Revise { base: created.id, entry: None },
    };
    {
        let mut store = Store::open(&database, roster.membership().clone()).unwrap();
        store.enable_tombstone_witness(&witness).unwrap();
        roster.receive(&mut store, &roster.sign_event(&key, &created).unwrap()).unwrap();
    }
    std::fs::copy(&database, &backup).unwrap();
    {
        let mut store = Store::open(&database, roster.membership().clone()).unwrap();
        store.enable_tombstone_witness(&witness).unwrap();
        roster.receive(&mut store, &roster.sign_event(&key, &deleted).unwrap()).unwrap();
    }
    assert_eq!(std::fs::read_dir(&witness).unwrap().filter(|entry| {
        entry.as_ref().unwrap().file_name().to_string_lossy().ends_with(".witness")
    }).count(), 1);
    std::fs::copy(&backup, &database).unwrap();
    let mut restored = Store::open(&database, roster.membership().clone()).unwrap();
    assert!(matches!(restored.enable_tombstone_witness(&witness), Err(StoreError::RollbackDetected)));
}

#[test]
fn interrupted_deletion_commit_stops_reopening_until_recovery() {
    let folder = tempfile::tempdir().unwrap();
    let database = folder.path().join("sync.db");
    let witness = folder.path().join("witness");
    let key = PeerSigningKey::from_bytes(&[78; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([4; 16], 1, [(peer, Role::Writer)].into());
    let created = Event {
        id: EventId { peer, sequence: 1 },
        context: VersionVector::new(),
        object: OBJECT,
        change: Change::Create(entry(5)),
    };
    let deleted = Event {
        id: EventId { peer, sequence: 2 },
        context: [(peer, 1)].into(),
        object: OBJECT,
        change: Change::Revise { base: created.id, entry: None },
    };
    let mut store = Store::open(&database, roster.membership().clone()).unwrap();
    store.enable_tombstone_witness(&witness).unwrap();
    roster.receive(&mut store, &roster.sign_event(&key, &created).unwrap()).unwrap();
    let fault = Connection::open(&database).unwrap();
    fault.execute_batch("CREATE TRIGGER interrupt_delete BEFORE INSERT ON journal
                        WHEN (SELECT COUNT(*) FROM journal) > 0
                        BEGIN SELECT RAISE(ABORT, 'injected commit failure'); END;").unwrap();
    assert!(roster.receive(&mut store, &roster.sign_event(&key, &deleted).unwrap()).is_err());
    drop(store);
    assert_eq!(std::fs::read_dir(&witness).unwrap().filter(|entry| {
        entry.as_ref().unwrap().file_name().to_string_lossy().ends_with(".witness")
    }).count(), 1);
    let mut reopened = Store::open(&database, roster.membership().clone()).unwrap();
    assert!(matches!(reopened.enable_tombstone_witness(&witness), Err(StoreError::RollbackDetected)));
}

#[test]
fn signed_confirmations_survive_reopening_for_later_peer_exchange() {
    let db = Database::new();
    let writer = PeerSigningKey::from_bytes(&[51; 32]);
    let reader = PeerSigningKey::from_bytes(&[52; 32]);
    let writer_id = writer.verifying_key().to_bytes();
    let reader_id = reader.verifying_key().to_bytes();
    let roster = Roster::new(
        [4; 16],
        1,
        [(writer_id, Role::Writer), (reader_id, Role::Reader)].into(),
    );
    let first = Event {
        id: EventId {
            peer: writer_id,
            sequence: 1,
        },
        context: VersionVector::new(),
        object: OBJECT,
        change: Change::Create(entry(5)),
    };
    let next = Event {
        id: EventId {
            peer: writer_id,
            sequence: 2,
        },
        context: [(writer_id, 1)].into(),
        object: OBJECT,
        change: Change::Revise {
            base: first.id,
            entry: Some(entry(6)),
        },
    };
    let confirmation = Confirmation {
        generation: 1,
        peer: reader_id,
        object: OBJECT,
        successor: next.id,
        sent_through: 0,
    };
    let wire = roster.sign_confirmation(&reader, &confirmation).unwrap();
    {
        let mut store = Store::open(db.path(), roster.membership().clone()).unwrap();
        for event in [&first, &next] {
            let signed = roster.sign_event(&writer, event).unwrap();
            roster.receive(&mut store, &signed).unwrap();
        }
        roster.receive_confirmation(&mut store, &wire).unwrap();
        roster.receive_confirmation(&mut store, &wire).unwrap();
    }
    let reopened = Store::open(db.path(), roster.membership().clone()).unwrap();
    let page = reopened.signed_confirmations_page(0, 128).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(
        roster.verify_confirmation(&page[0].1).unwrap(),
        confirmation
    );
    assert_eq!(
        reopened
            .protections()
            .next()
            .unwrap()
            .peer_states(reopened.replica().clock())
            .find(|(peer, _)| *peer == reader_id)
            .unwrap()
            .1,
        PeerState::Applied,
    );
}

fn entry(hash: u8) -> Entry {
    Entry {
        parent: None,
        name: "song.tja".into(),
        kind: EntryKind::File([hash; 32]),
    }
}

fn create() -> Event {
    Event {
        id: EventId {
            peer: WRITER,
            sequence: 1,
        },
        context: VersionVector::new(),
        object: OBJECT,
        change: Change::Create(entry(5)),
    }
}

fn change(peer: PeerId, sequence: u64, value: Option<Entry>) -> Event {
    Event {
        id: EventId { peer, sequence },
        context: [(WRITER, 1)].into(),
        object: OBJECT,
        change: Change::Revise {
            base: create().id,
            entry: value,
        },
    }
}

#[test]
fn replays_conflict_and_protection_after_reopening() {
    let db = Database::new();
    let edit = change(WRITER, 2, Some(entry(6)));
    let delete = change(OFFLINE, 1, None);
    {
        let mut store = Store::open(db.path(), membership()).unwrap();
        assert_eq!(store.apply_verified(create()).unwrap(), Apply::Added);
        assert_eq!(store.apply_verified(edit.clone()).unwrap(), Apply::Added);
        for peer in [WRITER, OFFLINE] {
            store
                .confirm_verified(Confirmation {
                    generation: 1,
                    object: OBJECT,
                    successor: edit.id,
                    peer,
                    sent_through: if peer == WRITER { 2 } else { 1 },
                })
                .unwrap();
        }
    }

    let mut store = Store::open(db.path(), membership()).unwrap();
    let protection = store.protections().next().unwrap();
    assert_eq!(protection.content(), [5; 32]);
    assert_eq!(
        protection
            .peer_states(store.replica().clock())
            .collect::<Vec<_>>(),
        [
            (WRITER, PeerState::Applied),
            (OFFLINE, PeerState::AwaitingEvents { through: 1 })
        ],
    );
    assert!(!protection.can_release(store.replica().clock()));

    assert_eq!(store.apply_verified(delete.clone()).unwrap(), Apply::Added);
    drop(store);
    let reopened = Store::open(db.path(), membership()).unwrap();
    assert!(reopened.replica().has_conflict(OBJECT));
    assert_eq!(
        reopened.replica().heads(OBJECT).collect::<Vec<_>>(),
        [&edit, &delete]
    );
    assert_eq!(reopened.replica().get(create().id), Some(&create()));
}

#[test]
fn concurrent_handles_refresh_and_failed_commit_does_not_advance_index() {
    let db = Database::new();
    let mut first = Store::open(db.path(), membership()).unwrap();
    let mut second = Store::open(db.path(), membership()).unwrap();
    assert_eq!(first.apply_verified(create()).unwrap(), Apply::Added);
    assert_eq!(second.apply_verified(create()).unwrap(), Apply::Duplicate);

    let fault = Connection::open(db.path()).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER reject_journal BEFORE INSERT ON journal
         BEGIN SELECT RAISE(ABORT, 'injected journal write failure'); END;",
        )
        .unwrap();
    let edit = change(WRITER, 2, Some(entry(6)));
    assert!(matches!(
        second.apply_verified(edit.clone()),
        Err(StoreError::Sql(_))
    ));
    assert!(second.replica().get(edit.id).is_none());
    assert_eq!(second.protections().count(), 0);

    fault.execute_batch("DROP TRIGGER reject_journal").unwrap();
    assert_eq!(second.apply_verified(edit.clone()).unwrap(), Apply::Added);
    first.refresh().unwrap();
    assert_eq!(first.replica().get(edit.id), Some(&edit));
    assert_eq!(first.protections().count(), 1);
}

#[test]
fn refuses_wrong_membership_and_corrupted_journal() {
    let db = Database::new();
    let mut store = Store::open(db.path(), membership()).unwrap();
    store.apply_verified(create()).unwrap();
    drop(store);
    let mut other = membership();
    other.generation += 1;
    assert!(matches!(
        Store::open(db.path(), other),
        Err(StoreError::WrongMembership)
    ));

    let connection = Connection::open(db.path()).unwrap();
    connection
        .execute("UPDATE journal SET record = X'FF' WHERE position = 1", [])
        .unwrap();
    assert!(matches!(
        Store::open(db.path(), membership()),
        Err(StoreError::Decode(_))
    ));
}
