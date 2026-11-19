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
fn platform_adapter_receives_validated_update_and_deletion_paths() {
    let folder = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(folder.path().join("objects")).unwrap();
    let old = metadata(b"old");
    let new = metadata(b"new");
    blobs.put(old, &b"old"[..]).unwrap();
    blobs.put(new, &b"new"[..]).unwrap();
    let mut store = Store::open(folder.path().join("state.db"), Membership {
        share: [12; 16], generation: 1, members: [PEER].into(),
    }).unwrap();
    let created = event(1, content_entry(old.hash));
    store.apply_verified(created.clone()).unwrap();
    let first = FileIntent {
        object: OBJECT, event: created.id, previous: None, desired: Some(old),
    };
    store.stage_file(&blobs, &first).unwrap();
    let mut empty = Target::default();
    assert_eq!(store.recover_file(&blobs, &mut empty, OBJECT).unwrap(), Recovery::Applied);

    let updated = event(2, content_entry(new.hash));
    store.apply_verified(updated.clone()).unwrap();
    let revision = FileIntent {
        object: OBJECT, event: updated.id, previous: Some(old), desired: Some(new),
    };
    store.stage_file(&blobs, &revision).unwrap();
    assert_eq!(store.recover_updated_file_with(&blobs, OBJECT, |path, intent| {
        assert_eq!(path, &[String::from("song.tja")]);
        assert_eq!(intent, &revision);
        Ok(Target { content: Some(b"local edit".to_vec()), writes: 0 })
    }).unwrap(), Recovery::Diverged);
    assert_eq!(store.applied_file(OBJECT).unwrap().unwrap().event, created.id);
    assert_eq!(store.recover_updated_file_with(&blobs, OBJECT, |path, intent| {
        assert_eq!(path, &[String::from("song.tja")]);
        assert_eq!(intent, &revision);
        Ok(Target { content: Some(b"old".to_vec()), writes: 0 })
    }).unwrap(), Recovery::Applied);
    assert_eq!(store.applied_file(OBJECT).unwrap().unwrap().contents, Some(new));

    let deleted = Event {
        id: EventId { peer: PEER, sequence: 3 },
        context: [(PEER, 2)].into(),
        object: OBJECT,
        change: Change::Revise { base: updated.id, entry: None },
    };
    store.apply_verified(deleted.clone()).unwrap();
    let tombstone = FileIntent {
        object: OBJECT, event: deleted.id, previous: Some(new), desired: None,
    };
    store.stage_file(&blobs, &tombstone).unwrap();
    assert_eq!(store.recover_deleted_file_with(&blobs, OBJECT, |path, intent| {
        assert_eq!(path, &[String::from("song.tja")]);
        assert_eq!(intent, &tombstone);
        Ok(Target { content: Some(b"new".to_vec()), writes: 0 })
    }).unwrap(), Recovery::Applied);
    assert_eq!(store.applied_file(OBJECT).unwrap().unwrap().contents, None);
}

#[test]
fn platform_move_adapter_receives_the_recorded_source_and_destination() {
    struct MoveTarget {
        source: Option<Vec<u8>>,
        destination: Option<Vec<u8>>,
    }
    impl FileTarget for MoveTarget {
        fn inspect(&mut self, _: ObjectId) -> io::Result<Option<Blob>> {
            Ok(self.destination.as_deref().map(metadata))
        }
        fn replace_if_current(&mut self, intent: &FileIntent, replacement: Option<File>) -> io::Result<bool> {
            if self.inspect(intent.object)? != intent.previous { return Ok(false); }
            self.destination = replacement.map(|mut file| {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok::<_, io::Error>(bytes)
            }).transpose()?;
            Ok(true)
        }
    }
    impl MoveFileTarget for MoveTarget {
        fn already_moved(&mut self, previous: Blob, desired: Blob, _: bool) -> io::Result<bool> {
            let current = self.destination.as_deref().map(metadata);
            Ok(self.source.is_none() && (current == Some(previous) || current == Some(desired)))
        }
        fn move_if_current(&mut self, previous: Blob, _: bool) -> io::Result<bool> {
            if self.source.as_deref().map(metadata) != Some(previous) || self.destination.is_some() {
                return Ok(false);
            }
            self.destination = self.source.take();
            Ok(true)
        }
    }

    let folder = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(folder.path().join("objects")).unwrap();
    let content = metadata(b"old");
    let replacement = metadata(b"edited after moving");
    blobs.put(content, &b"old"[..]).unwrap();
    blobs.put(replacement, &b"edited after moving"[..]).unwrap();
    let mut store = Store::open(folder.path().join("state.db"), Membership {
        share: [12; 16], generation: 1, members: [PEER].into(),
    }).unwrap();
    let created = event(1, content_entry(content.hash));
    store.apply_verified(created.clone()).unwrap();
    let first = FileIntent {
        object: OBJECT, event: created.id, previous: None, desired: Some(content),
    };
    store.stage_file(&blobs, &first).unwrap();
    store.recover_file(&blobs, &mut Target::default(), OBJECT).unwrap();
    let moved = Event {
        id: EventId { peer: PEER, sequence: 2 },
        context: [(PEER, 1)].into(),
        object: OBJECT,
        change: Change::Revise { base: created.id, entry: Some(Entry {
            name: "moved.tja".into(), ..content_entry(replacement.hash)
        }) },
    };
    store.apply_verified(moved.clone()).unwrap();
    let intent = FileIntent {
        object: OBJECT, event: moved.id, previous: Some(content), desired: Some(replacement),
    };
    store.stage_file(&blobs, &intent).unwrap();
    assert_eq!(store.recover_moved_file_with(&blobs, OBJECT, |before, after, pending, same_slot| {
        assert_eq!(before, &[String::from("song.tja")]);
        assert_eq!(after, &[String::from("moved.tja")]);
        assert_eq!(pending, &intent);
        assert!(!same_slot);
        Ok(MoveTarget { source: Some(b"old".to_vec()), destination: None })
    }).unwrap(), Recovery::Applied);
    let applied = store.applied_file(OBJECT).unwrap().unwrap();
    assert_eq!(applied.event, moved.id);
    assert_eq!(applied.contents, Some(replacement));
    assert_eq!(applied.path.unwrap(), ["moved.tja"]);
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
    assert!(store.applied_file(OBJECT).unwrap().is_none());
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
    let applied = reopened.applied_file(OBJECT).unwrap().unwrap();
    assert_eq!(applied.event, f.intent.event);
    assert_eq!(applied.contents, f.intent.desired);
    assert_eq!(applied.path.unwrap(), ["song.tja"]);
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
        .execute_batch("DROP TABLE signed_confirmation; DROP TABLE file_archive; DROP TABLE applied_file; DROP TABLE file_intent; PRAGMA user_version = 1;")
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

#[test]
fn pending_file_does_not_follow_a_parent_renamed_after_staging() {
    let f = Fixture::new();
    let root = f.folder.path().join("destination");
    std::fs::create_dir(&root).unwrap();
    let mut store = f.open();
    let directory = [8; 16];
    let object = [9; 16];
    let directory_event = EventId {
        peer: PEER,
        sequence: 3,
    };
    store
        .apply_verified(Event {
            id: directory_event,
            context: [(PEER, 2)].into(),
            object: directory,
            change: Change::Create(Entry {
                parent: None,
                name: "before".into(),
                kind: EntryKind::Directory,
            }),
        })
        .unwrap();
    let file_event = EventId {
        peer: PEER,
        sequence: 4,
    };
    let blob = metadata(b"new");
    store
        .apply_verified(Event {
            id: file_event,
            context: [(PEER, 3)].into(),
            object,
            change: Change::Create(Entry {
                parent: Some(directory),
                ..content_entry(blob.hash)
            }),
        })
        .unwrap();
    let intent = FileIntent {
        object,
        event: file_event,
        previous: None,
        desired: Some(blob),
    };
    store.stage_file(&f.blobs, &intent).unwrap();

    store
        .apply_verified(Event {
            id: EventId {
                peer: PEER,
                sequence: 5,
            },
            context: [(PEER, 4)].into(),
            object: directory,
            change: Change::Revise {
                base: directory_event,
                entry: Some(Entry {
                    parent: None,
                    name: "after".into(),
                    kind: EntryKind::Directory,
                }),
            },
        })
        .unwrap();

    drop(store);
    let mut store = f.open();
    assert_eq!(
        store.recover_new_file(&f.blobs, &root, object).unwrap(),
        Recovery::Superseded
    );
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    assert_eq!(store.pending_files().unwrap(), [intent]);
    assert!(store.applied_file(object).unwrap().is_none());
}

#[cfg(windows)]
#[test]
fn windows_deletion_keeps_changed_or_open_files_and_removes_the_verified_version() {
    use std::os::windows::fs::OpenOptionsExt;

    let f = Fixture::new();
    let root = f.folder.path().join("destination");
    std::fs::create_dir(&root).unwrap();
    let mut store = f.open();
    let create = FileIntent {
        previous: None,
        ..f.intent.clone()
    };
    store.stage_file(&f.blobs, &create).unwrap();
    assert_eq!(
        store.recover_new_file(&f.blobs, &root, OBJECT).unwrap(),
        Recovery::Applied
    );
    let path = root.join("song.tja");

    let id = EventId {
        peer: PEER,
        sequence: 3,
    };
    store
        .apply_verified(Event {
            id,
            context: [(PEER, 2)].into(),
            object: OBJECT,
            change: Change::Revise {
                base: create.event,
                entry: None,
            },
        })
        .unwrap();
    let deletion = FileIntent {
        object: OBJECT,
        event: id,
        previous: create.desired,
        desired: None,
    };
    store.stage_file(&f.blobs, &deletion).unwrap();

    std::fs::write(&path, b"user edit").unwrap();
    assert_eq!(
        store.recover_deleted_file(&f.blobs, &root, OBJECT).unwrap(),
        Recovery::Diverged
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"user edit");
    std::fs::write(&path, b"new").unwrap();

    let writer = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(7)
        .open(&path)
        .unwrap();
    assert_eq!(
        store.recover_deleted_file(&f.blobs, &root, OBJECT).unwrap(),
        Recovery::Diverged
    );
    drop(writer);
    assert_eq!(
        store.recover_deleted_file(&f.blobs, &root, OBJECT).unwrap(),
        Recovery::Applied
    );
    assert!(!path.exists());
    assert_eq!(store.applied_file(OBJECT).unwrap().unwrap().contents, None);
    assert!(f.blobs.open_verified(create.desired.unwrap()).is_ok());
}

#[cfg(windows)]
#[test]
fn update_recovers_after_old_path_was_moved_and_does_not_clobber_a_new_occupant() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    let root = f.folder.path().join("destination");
    std::fs::create_dir(&root).unwrap();
    let mut store = f.open();
    let old = metadata(b"new");
    let first = FileIntent {
        previous: None,
        ..f.intent.clone()
    };
    store.stage_file(&f.blobs, &first).unwrap();
    store.recover_new_file(&f.blobs, &root, OBJECT).unwrap();
    let target = root.join("song.tja");

    let desired = metadata(b"updated");
    f.blobs.put(desired, &b"updated"[..]).unwrap();
    let next = event(3, content_entry(desired.hash));
    store.apply_verified(next.clone()).unwrap();
    let pending = FileIntent {
        object: OBJECT,
        event: next.id,
        previous: Some(old),
        desired: Some(desired),
    };
    store.stage_file(&f.blobs, &pending).unwrap();

    let mut digest = Sha256::new();
    digest.update(OBJECT);
    digest.update(next.id.peer);
    digest.update(next.id.sequence.to_be_bytes());
    let backup = root.join(format!(".p2p-update-{:x}", digest.finalize()));
    std::fs::rename(&target, &backup).unwrap();
    drop(store);
    std::fs::write(&target, b"local file").unwrap();
    let mut store = f.open();
    assert_eq!(
        store.recover_updated_file(&f.blobs, &root, OBJECT).unwrap(),
        Recovery::Diverged
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"local file");
    assert_eq!(std::fs::read(&backup).unwrap(), b"new");
    std::fs::remove_file(&target).unwrap();

    assert_eq!(
        store.recover_updated_file(&f.blobs, &root, OBJECT).unwrap(),
        Recovery::Applied
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"updated");
    assert!(!backup.exists());
    assert!(f.blobs.open_verified(old).is_ok());
}

#[test]
#[cfg(windows)]
fn move_with_update_recovers_at_each_filesystem_boundary() {
    use sha2::{Digest, Sha256};

    enum InterruptedAfter {
        Move,
        Backup,
        Publish,
    }
    for phase in [
        InterruptedAfter::Move,
        InterruptedAfter::Backup,
        InterruptedAfter::Publish,
    ] {
        let f = Fixture::new();
        let root = f.folder.path().join("destination");
        std::fs::create_dir_all(root.join("folder")).unwrap();
        let mut store = f.open();
        let first = FileIntent {
            previous: None,
            ..f.intent.clone()
        };
        store.stage_file(&f.blobs, &first).unwrap();
        store.recover_new_file(&f.blobs, &root, OBJECT).unwrap();

        let parent = [7; 16];
        store
            .apply_verified(Event {
                object: parent,
                change: Change::Create(Entry {
                    parent: None,
                    name: "folder".into(),
                    kind: EntryKind::Directory,
                }),
                ..event(3, content_entry([0; 32]))
            })
            .unwrap();
        let desired = metadata(b"new contents");
        f.blobs.put(desired, &b"new contents"[..]).unwrap();
        let next = Event {
            change: Change::Revise {
                base: first.event,
                entry: Some(Entry {
                    parent: Some(parent),
                    name: "moved.tja".into(),
                    kind: EntryKind::File(desired.hash),
                }),
            },
            ..event(4, content_entry(desired.hash))
        };
        let intent = FileIntent {
            object: OBJECT,
            event: next.id,
            previous: first.desired,
            desired: Some(desired),
        };
        store.apply_verified(next).unwrap();
        store.stage_file(&f.blobs, &intent).unwrap();

        let source = root.join("song.tja");
        let destination = root.join("folder/moved.tja");
        let mut digest = Sha256::new();
        digest.update(OBJECT);
        digest.update(intent.event.peer);
        digest.update(intent.event.sequence.to_be_bytes());
        let backup = root
            .join("folder")
            .join(format!(".p2p-update-{:x}", digest.finalize()));
        std::fs::rename(&source, &destination).unwrap();
        if matches!(phase, InterruptedAfter::Backup | InterruptedAfter::Publish) {
            std::fs::rename(&destination, &backup).unwrap();
        }
        if matches!(phase, InterruptedAfter::Publish) {
            std::fs::write(&destination, b"new contents").unwrap();
        }
        drop(store);

        let mut store = f.open();
        let outcome = store.recover_moved_file(&f.blobs, &root, OBJECT).unwrap();
        let expected = if matches!(phase, InterruptedAfter::Publish) {
            Recovery::AlreadyApplied
        } else {
            Recovery::Applied
        };
        assert_eq!(outcome, expected);
        assert_eq!(std::fs::read(&destination).unwrap(), b"new contents");
        assert!(!source.exists());
        assert!(!backup.exists());
        assert!(store.pending_files().unwrap().is_empty());
        let applied = store.applied_file(OBJECT).unwrap().unwrap();
        assert_eq!(applied.event, intent.event);
        assert_eq!(
            applied.path,
            Some(vec!["folder".into(), "moved.tja".into()])
        );
    }
}

#[test]
fn obsolete_pending_update_is_retired_only_for_a_known_disk_version() {
    for (bytes, expected_version) in [
        (b"new".as_slice(), 2),
        (b"pending".as_slice(), 3),
        (b"local edit".as_slice(), 0),
    ] {
        let f = Fixture::new();
        let root = f.folder.path().join("destination");
        std::fs::create_dir(&root).unwrap();
        let mut store = f.open();
        let first = FileIntent {
            previous: None,
            ..f.intent.clone()
        };
        store.stage_file(&f.blobs, &first).unwrap();
        store.recover_new_file(&f.blobs, &root, OBJECT).unwrap();
        let file = root.join("song.tja");

        let pending_blob = metadata(b"pending");
        f.blobs.put(pending_blob, &b"pending"[..]).unwrap();
        let pending_event = event(3, content_entry(pending_blob.hash));
        store.apply_verified(pending_event.clone()).unwrap();
        let pending = FileIntent {
            object: OBJECT,
            event: pending_event.id,
            previous: first.desired,
            desired: Some(pending_blob),
        };
        store.stage_file(&f.blobs, &pending).unwrap();
        let newest = event(4, content_entry(metadata(b"newest").hash));
        store.apply_verified(newest).unwrap();
        std::fs::write(&file, bytes).unwrap();

        assert_eq!(
            store
                .retire_superseded_file(&f.blobs, &root, OBJECT)
                .unwrap(),
            expected_version != 0
        );
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        if expected_version == 0 {
            assert_eq!(store.pending_files().unwrap(), [pending]);
        } else {
            assert!(store.pending_files().unwrap().is_empty());
            assert_eq!(
                store.applied_file(OBJECT).unwrap().unwrap().event.sequence,
                expected_version
            );
            assert!(f.blobs.open_verified(pending_blob).is_ok());
        }
    }
}

#[cfg(windows)]
#[test]
fn superseded_update_restores_known_backup_and_then_applies_newest_version() {
    use sha2::{Digest, Sha256};
    let f = Fixture::new();
    let root = f.folder.path().join("destination");
    std::fs::create_dir(&root).unwrap();
    let mut store = f.open();
    let first = FileIntent {
        previous: None,
        ..f.intent.clone()
    };
    store.stage_file(&f.blobs, &first).unwrap();
    store.recover_new_file(&f.blobs, &root, OBJECT).unwrap();

    let skipped_blob = metadata(b"skipped");
    f.blobs.put(skipped_blob, &b"skipped"[..]).unwrap();
    let skipped = event(3, content_entry(skipped_blob.hash));
    store.apply_verified(skipped.clone()).unwrap();
    store
        .stage_file(
            &f.blobs,
            &FileIntent {
                object: OBJECT,
                event: skipped.id,
                previous: first.desired,
                desired: Some(skipped_blob),
            },
        )
        .unwrap();

    let mut digest = Sha256::new();
    digest.update(OBJECT);
    digest.update(skipped.id.peer);
    digest.update(skipped.id.sequence.to_be_bytes());
    let backup = root.join(format!(".p2p-update-{:x}", digest.finalize()));
    let path = root.join("song.tja");
    std::fs::rename(&path, &backup).unwrap();

    let desired = metadata(b"latest");
    f.blobs.put(desired, &b"latest"[..]).unwrap();
    let latest = event(4, content_entry(desired.hash));
    store.apply_verified(latest.clone()).unwrap();
    drop(store);

    let mut store = f.open();
    assert!(store
        .retire_superseded_file(&f.blobs, &root, OBJECT)
        .unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    assert!(!backup.exists());
    assert_eq!(
        store.applied_file(OBJECT).unwrap().unwrap().event,
        first.event
    );

    store
        .stage_file(
            &f.blobs,
            &FileIntent {
                object: OBJECT,
                event: latest.id,
                previous: first.desired,
                desired: Some(desired),
            },
        )
        .unwrap();
    assert_eq!(
        store.recover_updated_file(&f.blobs, &root, OBJECT).unwrap(),
        Recovery::Applied
    );
    assert_eq!(std::fs::read(path).unwrap(), b"latest");
}

#[test]
fn archives_expire_without_dropping_a_protected_old_blob() {
    use p2p_sync_core::archive::RETENTION_SECONDS;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let f = Fixture::new();
    let mut store = f.open();
    let first = FileIntent {
        previous: None,
        ..f.intent.clone()
    };
    store.stage_file(&f.blobs, &first).unwrap();
    let mut target = Target::default();
    assert_eq!(
        store.recover_file(&f.blobs, &mut target, OBJECT).unwrap(),
        Recovery::Applied
    );
    assert!(store.archives(SystemTime::now()).unwrap().is_empty());

    let latest = metadata(b"latest");
    f.blobs.put(latest, &b"latest"[..]).unwrap();
    let event = event(3, content_entry(latest.hash));
    store.apply_verified(event.clone()).unwrap();
    store
        .stage_file(
            &f.blobs,
            &FileIntent {
                object: OBJECT,
                event: event.id,
                previous: first.desired,
                desired: Some(latest),
            },
        )
        .unwrap();
    assert_eq!(
        store.recover_file(&f.blobs, &mut target, OBJECT).unwrap(),
        Recovery::Applied
    );
    let history = store.archives(SystemTime::now()).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].event, first.event);
    assert_eq!(history[0].content, first.desired.unwrap());
    let expires =
        UNIX_EPOCH + Duration::from_secs((history[0].archived_at + RETENTION_SECONDS) as u64);
    drop(store);

    let store = f.open();
    assert_eq!(
        store.archives(expires - Duration::from_secs(1)).unwrap(),
        history
    );
    assert!(store.archives(expires).unwrap().is_empty());
    assert_eq!(store.prune_archives(expires).unwrap(), 1);
    assert_eq!(store.prune_archives(expires).unwrap(), 0);
    assert!(store
        .protections()
        .any(|protection| protection.original() == first.event));
    assert!(f.blobs.open_verified(first.desired.unwrap()).is_ok());
}

#[test]
fn protected_storage_counts_unique_content_and_reports_missing_blobs() {
    let f = Fixture::new();
    let mut store = f.open();
    let old = metadata(b"old");
    let other = [78; 16];
    let create = Event {
        object: other,
        ..event(3, content_entry(old.hash))
    };
    let create = Event {
        change: Change::Create(content_entry(old.hash)),
        ..create
    };
    store.apply_verified(create.clone()).unwrap();
    let next = Event {
        object: other,
        change: Change::Revise {
            base: create.id,
            entry: None,
        },
        ..event(4, content_entry(old.hash))
    };
    let deletion = next.id;
    store.apply_verified(next).unwrap();

    let report = store.protected_storage(&f.blobs).unwrap();
    assert_eq!((report.files, report.bytes, report.missing), (1, 3, 0));
    let originals = store.protected_originals(&f.blobs).unwrap();
    assert_eq!(originals.len(), 1);
    assert_eq!(originals[0].hash, old.hash);
    assert_eq!(originals[0].bytes, Some(3));
    assert!(originals[0].names.contains(&content_entry(old.hash).name));
    assert!(!originals[0].conflict);
    assert_eq!(originals[0].waiting.len(), 1);
    assert_eq!(originals[0].waiting[0].peer, "02".repeat(32));
    assert_eq!(
        originals[0].waiting[0].state,
        p2p_sync_core::archive::WaitingFor::Confirmation
    );
    let empty = BlobStore::open(f.folder.path().join("empty-blobs")).unwrap();
    let report = store.protected_storage(&empty).unwrap();
    assert_eq!((report.files, report.bytes, report.missing), (0, 0, 1));
    assert_eq!(store.protected_originals(&empty).unwrap()[0].bytes, None);

    for (object, successor) in [(OBJECT, f.intent.event), (other, deletion)] {
        store
            .confirm_verified(p2p_sync_core::retention::Confirmation {
                generation: 1,
                peer: PEER,
                object,
                successor,
                sent_through: 5,
            })
            .unwrap();
    }
    let originals = store.protected_originals(&f.blobs).unwrap();
    assert_eq!(
        originals[0].waiting[0].state,
        p2p_sync_core::archive::WaitingFor::Events
    );
    store
        .apply_verified(Event {
            object: [79; 16],
            change: Change::Create(content_entry(old.hash)),
            ..event(5, content_entry(old.hash))
        })
        .unwrap();
    assert!(store.protected_originals(&f.blobs).unwrap().is_empty());
}
