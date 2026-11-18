use std::{
    io::{self, Cursor, Read, Write},
    net::{TcpListener, TcpStream},
    time::Duration,
};

use p2p_sync_core::{auth::*, replica::*, store::Store, wire::*};

#[test]
fn two_stores_exchange_events_over_loopback_and_send_no_duplicate_page() {
    let key = PeerSigningKey::from_bytes(&[19; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([1; 16], 1, [(peer, Role::Writer)].into());
    let membership = roster.membership().clone();
    let event = Event {
        id: EventId { peer, sequence: 1 },
        context: VersionVector::new(),
        object: [5; 16],
        change: Change::Create(Entry {
            parent: None,
            name: "song.tja".into(),
            kind: EntryKind::File([7; 32]),
        }),
    };
    let signed = roster.sign_event(&key, &event).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut store = Store::open(":memory:", membership).unwrap();
        assert_eq!(
            roster_for(peer).receive(&mut store, &signed).unwrap(),
            Apply::Added
        );
        let (mut stream, _) = listener.accept().unwrap();
        configure(&stream);
        serve_pull(&mut stream, &mut store).unwrap();
        serve_pull(&mut stream, &mut store).unwrap();
    });

    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    configure(&stream);
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
    let (report, more) = pull_page(&mut stream, &mut store, &roster).unwrap();
    assert_eq!(report.added, 1);
    assert!(!more);
    assert_eq!(store.replica().get(event.id), Some(&event));
    assert_eq!(
        pull_page(&mut stream, &mut store, &roster).unwrap().0.added,
        0
    );
    server.join().unwrap();
}

fn roster_for(peer: PeerId) -> Roster {
    Roster::new([1; 16], 1, [(peer, Role::Writer)].into())
}

fn configure(stream: &TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
}

struct Scripted(Cursor<Vec<u8>>);

impl Read for Scripted {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Scripted {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Ok(buffer.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn rejects_oversized_and_truncated_frames_without_applying_any_event() {
    let roster = roster_for([4; 32]);
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
    let mut oversized = Scripted(Cursor::new(
        [b"P2P1".as_slice(), &[2], &u32::MAX.to_be_bytes()].concat(),
    ));
    assert!(matches!(
        pull_page(&mut oversized, &mut store, &roster),
        Err(WireError::InvalidFrame)
    ));
    let mut truncated = Scripted(Cursor::new(
        [b"P2P1".as_slice(), &[2], &2_u32.to_be_bytes(), &[0]].concat(),
    ));
    assert!(
        matches!(pull_page(&mut truncated, &mut store, &roster), Err(WireError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
    );
    assert!(store.replica().clock().is_empty());
}

#[test]
fn transfers_missing_chunks_after_a_network_reconnection() {
    use p2p_sync_core::{
        blob::BlobStore,
        chunks::{Manifest, CHUNK_BYTES},
    };
    let key = PeerSigningKey::from_bytes(&[20; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = roster_for(peer);
    let first = vec![7; CHUNK_BYTES as usize];
    let data = [first.as_slice(), b"short tail"].concat();
    let sender_data = data.clone();
    let manifest = Manifest::build(data.as_slice(), data.len() as u64).unwrap();
    let item = Event {
        id: EventId { peer, sequence: 1 },
        context: VersionVector::new(),
        object: [5; 16],
        change: Change::Create(Entry {
            parent: None,
            name: "song.tja".into(),
            kind: EntryKind::File(manifest.file().hash),
        }),
    };
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
    roster
        .receive(&mut store, &roster.sign_event(&key, &item).unwrap())
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut source = Cursor::new(sender_data);
        let (mut stream, _) = listener.accept().unwrap();
        configure(&stream);
        serve_manifest(&mut stream, &manifest).unwrap();
        serve_chunk(&mut stream, &manifest, &mut source).unwrap();
        drop(stream);
        let (mut stream, _) = listener.accept().unwrap();
        configure(&stream);
        serve_chunk(&mut stream, &manifest, &mut source).unwrap();
    });

    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("chunks");
    let completed = BlobStore::open(dir.path().join("files")).unwrap();
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    configure(&stream);
    let manifest = pull_manifest(&mut stream, &store, item.id).unwrap();
    {
        let cache = BlobStore::open(&cache_dir).unwrap();
        pull_chunk(&mut stream, &manifest, &cache, 0).unwrap();
        assert!(manifest.finish(&cache, &completed).is_err());
    }
    drop(stream);
    let cache = BlobStore::open(&cache_dir).unwrap();
    assert_eq!(manifest.missing_chunks(&cache).unwrap(), [1]);

    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    configure(&stream);
    pull_chunk(&mut stream, &manifest, &cache, 1).unwrap();
    manifest.finish(&cache, &completed).unwrap();
    let mut result = Vec::new();
    completed
        .open_verified(manifest.file())
        .unwrap()
        .read_to_end(&mut result)
        .unwrap();
    assert_eq!(result, data);
    server.join().unwrap();
}

#[test]
fn share_session_refuses_content_without_a_signed_event_even_when_blob_exists() {
    use p2p_sync_core::{
        blob::{Blob, BlobStore},
        sha256,
    };
    let folder = tempfile::tempdir().unwrap();
    let blobs = BlobStore::open(folder.path()).unwrap();
    let data = b"unshared file";
    let hash = sha256(data);
    blobs
        .put(
            Blob {
                hash,
                length: data.len() as u64,
            },
            &data[..],
        )
        .unwrap();
    let roster = roster_for([9; 32]);
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
    let request = [b"P2P1".as_slice(), &[3], &32_u32.to_be_bytes(), &hash].concat();
    let response_offset = request.len();
    let mut stream = Cursor::new(request);
    assert!(matches!(
        ShareSession::default().serve_next(&mut stream, &mut store, &blobs, &roster),
        Ok(SessionProgress::Continue)
    ));
    assert_eq!(&stream.into_inner()[response_offset..], b"P2P1\x0b\0\0\0\0");
}

#[test]
fn a_fresh_peer_caches_all_conflicting_versions_without_publishing_a_winner() {
    use p2p_sync_core::{
        blob::{Blob, BlobStore},
        projection::PathIssue,
        publish::{resolve_file_conflict, PublishError},
        sha256,
        sync::{sync_new_files, Destination, FileResult},
    };
    let folder = tempfile::tempdir().unwrap();
    let source = BlobStore::open(folder.path().join("source")).unwrap();
    let chunks = BlobStore::open(folder.path().join("chunks")).unwrap();
    let completed = BlobStore::open(folder.path().join("completed")).unwrap();
    let destination = folder.path().join("destination");
    std::fs::create_dir(&destination).unwrap();
    let first_key = PeerSigningKey::from_bytes(&[40; 32]);
    let second_key = PeerSigningKey::from_bytes(&[41; 32]);
    let first_peer = first_key.verifying_key().to_bytes();
    let second_peer = second_key.verifying_key().to_bytes();
    let roster = Roster::new(
        [1; 16],
        1,
        [(first_peer, Role::Writer), (second_peer, Role::Writer)].into(),
    );
    let object = [5; 16];
    let base = EventId {
        peer: first_peer,
        sequence: 1,
    };
    let mut sender =
        Store::open(folder.path().join("sender.db"), roster.membership().clone()).unwrap();
    let mut receiver = Store::open(
        folder.path().join("receiver.db"),
        roster.membership().clone(),
    )
    .unwrap();
    let mut expected = Vec::new();
    for (key, id, data) in [
        (&first_key, base, b"base".as_slice()),
        (
            &first_key,
            EventId {
                peer: first_peer,
                sequence: 2,
            },
            b"left".as_slice(),
        ),
        (
            &second_key,
            EventId {
                peer: second_peer,
                sequence: 1,
            },
            b"right".as_slice(),
        ),
    ] {
        let blob = Blob {
            hash: sha256(data),
            length: data.len() as u64,
        };
        source.put(blob, data).unwrap();
        expected.push(blob);
        let entry = Entry {
            parent: None,
            name: "song.tja".into(),
            kind: EntryKind::File(blob.hash),
        };
        let (context, change) = if id == base {
            (VersionVector::new(), Change::Create(entry))
        } else {
            (
                [(first_peer, 1)].into(),
                Change::Revise {
                    base,
                    entry: Some(entry),
                },
            )
        };
        let signed = roster
            .sign_event(
                key,
                &Event {
                    id,
                    context,
                    object,
                    change,
                },
            )
            .unwrap();
        roster.receive(&mut sender, &signed).unwrap();
    }
    for (index, name, data) in [
        (6, "missing.tja", b"missing".as_slice()),
        (7, "remote.tja", b"remote".as_slice()),
        (8, "cached.tja", b"cached".as_slice()),
    ] {
        let blob = Blob {
            hash: sha256(data),
            length: data.len() as u64,
        };
        match index {
            7 => source.put(blob, data).unwrap(),
            8 => completed.put(blob, data).unwrap(),
            _ => {}
        }
        let sequence = u64::from(index) - 3;
        let event = Event {
            id: EventId {
                peer: first_peer,
                sequence,
            },
            context: [(first_peer, sequence - 1), (second_peer, 1)].into(),
            object: [index; 16],
            change: Change::Create(Entry {
                parent: None,
                name: name.into(),
                kind: EntryKind::File(blob.hash),
            }),
        };
        let signed = roster.sign_event(&first_key, &event).unwrap();
        roster.receive(&mut sender, &signed).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::scope(|threads| {
        let roster = &roster;
        let server = threads.spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            configure(&stream);
            let mut session = ShareSession::default();
            while session
                .serve_next(&mut stream, &mut sender, &source, roster)
                .unwrap()
                == SessionProgress::Continue
            {}
        });
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
        configure(&stream);
        let report = sync_new_files(
            &mut stream,
            &mut receiver,
            roster,
            Destination {
                root: &destination,
                chunks: &chunks,
                completed: &completed,
            },
        )
        .unwrap();
        finish_session(&mut stream).unwrap();
        server.join().unwrap();
        assert_eq!(
            report.files,
            [
                (
                    object,
                    FileResult::BlockedPath(PathIssue::Divergent(object))
                ),
                ([6; 16], FileResult::UnavailableContent),
                ([7; 16], FileResult::Published),
                ([8; 16], FileResult::Published),
            ]
        );
    });
    assert!(!destination.join("song.tja").exists());
    let conflicts = receiver.file_conflicts(&completed).unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].versions.len(), 2);
    for version in &conflicts[0].versions {
        assert_eq!(version.name.as_deref(), Some("song.tja"));
        assert_eq!(version.kind, "file");
        assert!(version.bytes.is_some());
    }
    let empty = BlobStore::open(folder.path().join("empty")).unwrap();
    assert!(receiver.file_conflicts(&empty).unwrap()[0]
        .versions
        .iter()
        .all(|version| version.bytes.is_none()));
    let checkpoint = receiver
        .generation_checkpoint(std::time::SystemTime::now())
        .unwrap();
    assert_eq!(checkpoint.frontier, *receiver.replica().clock());
    let staged = BlobStore::open(folder.path().join("staged")).unwrap();
    let missing = Blob {
        hash: sha256(b"missing"),
        length: 7,
    };
    assert!(matches!(
        checkpoint.stage_contents(&completed, &staged),
        Err(p2p_sync_core::generation::CheckpointDataError::Content { hash, source })
            if hash == missing.hash && source.kind() == io::ErrorKind::NotFound
    ));
    completed.put(missing, &b"missing"[..]).unwrap();
    assert_eq!(checkpoint.stage_contents(&completed, &staged).unwrap(), 6);
    let heads: Vec<_> = checkpoint
        .events
        .iter()
        .filter(|event| event.object == object && checkpoint.heads.contains(&event.id))
        .collect();
    assert_eq!(heads.len(), 2);
    assert_eq!(
        checkpoint
            .protected
            .iter()
            .filter(|reference| reference.original == base)
            .count(),
        2
    );
    assert!(!destination.join("missing.tja").exists());
    assert_eq!(
        std::fs::read(destination.join("remote.tja")).unwrap(),
        b"remote"
    );
    assert_eq!(
        std::fs::read(destination.join("cached.tja")).unwrap(),
        b"cached"
    );
    for blob in expected {
        assert!(completed.open_verified(blob).is_ok());
    }
    let reviewed: std::collections::BTreeSet<_> = receiver
        .replica()
        .heads(object)
        .map(|event| event.id)
        .collect();
    let choice = EventId {
        peer: second_peer,
        sequence: 1,
    };
    assert!(matches!(
        resolve_file_conflict(
            &mut receiver,
            &roster,
            &first_key,
            &completed,
            choice,
            [choice].into()
        ),
        Err(PublishError::StaleBase)
    ));
    let id = resolve_file_conflict(
        &mut receiver,
        &roster,
        &first_key,
        &completed,
        choice,
        reviewed.clone(),
    )
    .unwrap();
    let event = roster
        .verify_event(receiver.signed_event(id).unwrap())
        .unwrap();
    let Change::Resolve { bases, entry } = event.change else {
        panic!("expected a resolution")
    };
    assert_eq!(bases, reviewed);
    assert_eq!(entry.unwrap().kind, EntryKind::File(sha256(b"right")));
    assert!(!receiver.replica().has_conflict(object));
    assert!(receiver
        .protections()
        .any(|protection| protection.content() == sha256(b"left")));
}

#[test]
fn an_applied_file_updates_or_keeps_unapplied_version_without_stale_intent() {
    use p2p_sync_core::{
        blob::{Blob, BlobStore},
        materialize::FileIntent,
        sha256,
        sync::{sync_new_files, Destination, FileResult},
    };
    let scope = tempfile::tempdir().unwrap();
    let destination = scope.path().join("dest");
    std::fs::create_dir(&destination).unwrap();
    let completed = BlobStore::open(scope.path().join("completed")).unwrap();
    let chunks = BlobStore::open(scope.path().join("chunks")).unwrap();
    let old = Blob {
        hash: sha256(b"old"),
        length: 3,
    };
    let new = Blob {
        hash: sha256(b"new"),
        length: 3,
    };
    completed.put(old, &b"old"[..]).unwrap();

    let key = PeerSigningKey::from_bytes(&[24; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = roster_for(peer);
    let mut store =
        Store::open(scope.path().join("state.db"), roster.membership().clone()).unwrap();
    let object = [16; 16];
    let first = Event {
        id: EventId { peer, sequence: 1 },
        context: VersionVector::new(),
        object,
        change: Change::Create(Entry {
            parent: None,
            name: "song.tja".into(),
            kind: EntryKind::File(old.hash),
        }),
    };
    roster
        .receive(&mut store, &roster.sign_event(&key, &first).unwrap())
        .unwrap();
    store
        .stage_file(
            &completed,
            &FileIntent {
                object,
                event: first.id,
                previous: None,
                desired: Some(old),
            },
        )
        .unwrap();
    store
        .recover_new_file(&completed, &destination, object)
        .unwrap();

    let revision = Event {
        id: EventId { peer, sequence: 2 },
        context: [(peer, 1)].into(),
        object,
        change: Change::Revise {
            base: first.id,
            entry: Some(Entry {
                parent: None,
                name: "song.tja".into(),
                kind: EntryKind::File(new.hash),
            }),
        },
    };
    let signed = roster.sign_event(&key, &revision).unwrap();
    let page = [&[0][..], &(signed.len() as u32).to_be_bytes(), &signed].concat();
    let manifest = [&3_u64.to_be_bytes()[..], &new.hash].concat();
    let mut response = Vec::new();
    for (kind, bytes) in [(2, page.as_slice()), (4, manifest.as_slice()), (6, b"new")] {
        response.extend_from_slice(b"P2P1");
        response.push(kind);
        response.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        response.extend_from_slice(bytes);
    }
    let mut stream = Scripted(Cursor::new(response));
    let report = sync_new_files(
        &mut stream,
        &mut store,
        &roster,
        Destination {
            root: &destination,
            chunks: &chunks,
            completed: &completed,
        },
    )
    .unwrap();
    #[cfg(windows)]
    {
        assert_eq!(report.files, [(object, FileResult::Updated)]);
        assert!(store.pending_files().unwrap().is_empty());
        assert_eq!(
            store.applied_file(object).unwrap().unwrap().event,
            revision.id
        );
        assert_eq!(std::fs::read(destination.join("song.tja")).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
    }
    #[cfg(not(windows))]
    {
        assert_eq!(report.files, [(object, FileResult::PendingRecovery)]);
        assert!(store.pending_files().unwrap().is_empty());
        assert_eq!(store.applied_file(object).unwrap().unwrap().event, first.id);
        assert_eq!(std::fs::read(destination.join("song.tja")).unwrap(), b"old");
    }
    assert!(completed.open_verified(old).is_ok());
    assert!(completed.open_verified(new).is_ok());
}
