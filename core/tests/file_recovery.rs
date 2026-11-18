use std::{
    fs::File,
    io::{self, Read},
};

use p2p_sync_core::{
    blob::{Blob, BlobStore},
    materialize::*,
    replica::*,
    sha256,
    store::{Membership, Store},
};
use rusqlite::Connection;

const OBJECT: ObjectId = [5; 16];
const PEER: PeerId = [2; 32];

#[derive(Default)]
struct Target {
    content: Option<Vec<u8>>,
    writes: usize,
}

impl FileTarget for Target {
    fn inspect(&mut self, _: ObjectId) -> io::Result<Option<Blob>> {
        Ok(self.content.as_deref().map(metadata))
    }

    fn replace_if_current(
        &mut self,
        intent: &FileIntent,
        replacement: Option<File>,
    ) -> io::Result<bool> {
        if self.inspect(intent.object)? != intent.previous {
            return Ok(false);
        }
        self.content = replacement
            .map(|mut file| {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok::<_, io::Error>(bytes)
            })
            .transpose()?;
        self.writes += 1;
        Ok(true)
    }
}

struct Fixture {
    folder: tempfile::TempDir,
    blobs: BlobStore,
    intent: FileIntent,
}

impl Fixture {
    fn new() -> Self {
        let folder = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(folder.path().join("objects")).unwrap();
        let old = metadata(b"old");
        let new = metadata(b"new");
        blobs.put(old, &b"old"[..]).unwrap();
        blobs.put(new, &b"new"[..]).unwrap();

        let root = event(1, content_entry(old.hash));
        let next = event(2, content_entry(new.hash));
        let intent = FileIntent {
            object: OBJECT,
            event: next.id,
            previous: Some(old),
            desired: Some(new),
        };
        let result = Self {
            folder,
            blobs,
            intent,
        };
        let mut store = result.open();
        store.apply_verified(root).unwrap();
        store.apply_verified(next).unwrap();
        result
    }

    fn open(&self) -> Store {
        Store::open(
            self.folder.path().join("sync.db"),
            Membership {
                share: [12; 16],
                generation: 1,
                members: [PEER].into(),
            },
        )
        .unwrap()
    }
}

fn event(sequence: u64, entry: Entry) -> Event {
    Event {
        id: EventId {
            peer: PEER,
            sequence,
        },
        context: if sequence == 1 {
            VersionVector::new()
        } else {
            [(PEER, sequence - 1)].into()
        },
        object: OBJECT,
        change: if sequence == 1 {
            Change::Create(entry)
        } else {
            Change::Revise {
                base: EventId {
                    peer: PEER,
                    sequence: sequence - 1,
                },
                entry: Some(entry),
            }
        },
    }
}

fn content_entry(hash: ContentHash) -> Entry {
    Entry {
        parent: None,
        name: "song.tja".into(),
        kind: EntryKind::File(hash),
    }
}

fn metadata(bytes: &[u8]) -> Blob {
    Blob {
        hash: sha256(bytes),
        length: bytes.len() as u64,
    }
}

#[test]
fn resumes_after_filesystem_write_when_sql_completion_failed() {
    let f = Fixture::new();
    let mut store = f.open();
    store.stage_file(&f.blobs, &f.intent).unwrap();
    store.stage_file(&f.blobs, &f.intent).unwrap();
    let fault = Connection::open(f.folder.path().join("sync.db")).unwrap();
    fault
        .execute_batch(
            "CREATE TRIGGER reject_completion BEFORE DELETE ON file_intent
         BEGIN SELECT RAISE(ABORT, 'injected failure after filesystem write'); END;",
        )
        .unwrap();
    let mut target = Target {
        content: Some(b"old".to_vec()),
        writes: 0,
    };

    assert!(store.recover_file(&f.blobs, &mut target, OBJECT).is_err());
    assert_eq!(target.content.as_deref(), Some(&b"new"[..]));
    assert_eq!(store.pending_files().unwrap(), [f.intent.clone()]);
    drop(store);
    fault
        .execute_batch("DROP TRIGGER reject_completion")
        .unwrap();

    let mut reopened = f.open();
    assert_eq!(
        reopened
            .recover_file(&f.blobs, &mut target, OBJECT)
            .unwrap(),
        Recovery::AlreadyApplied
    );
    assert_eq!(target.writes, 1);
    assert!(reopened.pending_files().unwrap().is_empty());
    assert!(f.blobs.open_verified(f.intent.previous.unwrap()).is_ok());
}

#[test]
fn preserves_local_edit_and_never_writes_a_superseded_intent() {
    let f = Fixture::new();
    let mut store = f.open();
    store.stage_file(&f.blobs, &f.intent).unwrap();
    let mut target = Target {
        content: Some(b"local edit".to_vec()),
        writes: 0,
    };
    assert_eq!(
        store.recover_file(&f.blobs, &mut target, OBJECT).unwrap(),
        Recovery::Diverged
    );
    assert_eq!(target.content.as_deref(), Some(&b"local edit"[..]));

    let mut other = f.open();
    other
        .apply_verified(event(3, content_entry(metadata(b"newer").hash)))
        .unwrap();
    target.content = Some(b"old".to_vec());
    assert_eq!(
        store.recover_file(&f.blobs, &mut target, OBJECT).unwrap(),
        Recovery::Superseded
    );
    assert_eq!(target.writes, 0);
    assert_eq!(store.pending_files().unwrap(), [f.intent.clone()]);
}

#[test]
fn stages_only_verified_versions_and_migrates_the_initial_schema() {
    let f = Fixture::new();
    let db = f.folder.path().join("sync.db");
    let connection = Connection::open(db).unwrap();
    connection
        .execute_batch("DROP TABLE file_intent; PRAGMA user_version = 1;")
        .unwrap();
    drop(connection);
    let mut store = f.open();
    assert!(store.replica().get(f.intent.event).is_some());
    let mut wrong = f.intent.clone();
    wrong.previous = Some(metadata(b"missing backup"));
    assert!(matches!(
        store.stage_file(&f.blobs, &wrong),
        Err(MaterializeError::Io(_))
    ));
    assert!(store.pending_files().unwrap().is_empty());

    store.stage_file(&f.blobs, &f.intent).unwrap();
    drop(store);
    assert_eq!(f.open().pending_files().unwrap(), [f.intent]);
}

#[test]
fn creates_nested_file_in_selected_root_and_preserves_a_collision() {
    let f = Fixture::new();
    let root = f.folder.path().join("destination");
    std::fs::create_dir_all(root.join("譜面")).unwrap();
    let dir = [6; 16];
    let object = [7; 16];
    let content = metadata(b"new");
    let mut store = f.open();
    store
        .apply_verified(Event {
            id: EventId {
                peer: PEER,
                sequence: 3,
            },
            context: [(PEER, 2)].into(),
            object: dir,
            change: Change::Create(Entry {
                parent: None,
                name: "譜面".into(),
                kind: EntryKind::Directory,
            }),
        })
        .unwrap();
    let id = EventId {
        peer: PEER,
        sequence: 4,
    };
    store
        .apply_verified(Event {
            id,
            context: [(PEER, 3)].into(),
            object,
            change: Change::Create(Entry {
                parent: Some(dir),
                ..content_entry(content.hash)
            }),
        })
        .unwrap();
    let intent = FileIntent {
        object,
        event: id,
        previous: None,
        desired: Some(content),
    };
    store.stage_file(&f.blobs, &intent).unwrap();
    let destination = root.join("譜面").join("song.tja");
    std::fs::write(&destination, b"user content").unwrap();
    assert_eq!(
        store.recover_new_file(&f.blobs, &root, object).unwrap(),
        Recovery::Diverged
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"user content");
    std::fs::remove_file(&destination).unwrap();
    drop(store);

    let mut store = f.open();
    assert_eq!(
        store.recover_new_file(&f.blobs, &root, object).unwrap(),
        Recovery::Applied
    );
    assert_eq!(std::fs::read(&destination).unwrap(), b"new");
    assert_eq!(std::fs::read_dir(root.join("譜面")).unwrap().count(), 1);
    assert!(store.pending_files().unwrap().is_empty());
}
