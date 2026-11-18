use p2p_sync_core::{
    auth::{PeerSigningKey, Role, Roster},
    blob::{Blob, BlobStore},
    publish::{publish_file, revise_file, PublishError},
    replica::EntryKind,
    sha256,
    store::Store,
};

#[test]
fn selected_files_are_verified_and_signed_and_names_are_collision_checked() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("selected.tja");
    let data = b"TITLE:Selected chart";
    std::fs::write(&source, data).unwrap();

    let key = PeerSigningKey::from_bytes(&[11; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([4; 16], 1, [(peer, Role::Writer)].into());
    let blobs = BlobStore::open(directory.path().join("completed")).unwrap();
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
    let id = publish_file(&source, &mut store, &roster, &key, &blobs, None, "譜面.tja").unwrap();
    let event = roster
        .verify_event(store.signed_event(id).unwrap())
        .unwrap();
    let hash = sha256(data);
    assert_eq!(event.change.entry().unwrap().kind, EntryKind::File(hash));
    assert!(blobs
        .open_verified(Blob {
            hash,
            length: data.len() as u64
        })
        .is_ok());
    assert_eq!(
        publish_file(&source, &mut store, &roster, &key, &blobs, None, "譜面.TJA").unwrap(),
        id
    );

    std::fs::write(&source, b"different file").unwrap();
    assert!(matches!(
        publish_file(&source, &mut store, &roster, &key, &blobs, None, "譜面.TJA"),
        Err(PublishError::NameConflict)
    ));
    assert_eq!(store.replica().clock()[&peer], 1);

    let readonly = Roster::new([4; 16], 1, [(peer, Role::Reader)].into());
    assert!(matches!(
        publish_file(
            &source,
            &mut store,
            &readonly,
            &key,
            &blobs,
            None,
            "別の譜面.tja"
        ),
        Err(PublishError::Auth(_))
    ));
}

#[test]
fn updates_and_tombstones_share_the_same_object_and_retain_previous_versions() {
    use p2p_sync_core::generation::{GenerationTransition, TransitionError};
    use std::time::SystemTime;
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("song.tja");
    std::fs::write(&source, b"first").unwrap();
    let key = PeerSigningKey::from_bytes(&[11; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([4; 16], 1, [(peer, Role::Writer)].into());
    let blobs = BlobStore::open(directory.path().join("completed")).unwrap();
    let database = directory.path().join("sync.db");
    let mut store = Store::open(&database, roster.membership().clone()).unwrap();

    let first = publish_file(&source, &mut store, &roster, &key, &blobs, None, "song.tja").unwrap();
    let object = store.replica().get(first).unwrap().object;
    assert_eq!(
        revise_file(Some(&source), &mut store, &roster, &key, &blobs, first).unwrap(),
        first
    );

    std::fs::write(&source, b"second").unwrap();
    let second = revise_file(Some(&source), &mut store, &roster, &key, &blobs, first).unwrap();
    assert_eq!(store.replica().get(second).unwrap().object, object);
    assert!(matches!(
        revise_file(None, &mut store, &roster, &key, &blobs, first),
        Err(PublishError::StaleBase)
    ));

    let last = revise_file(None, &mut store, &roster, &key, &blobs, second).unwrap();
    let removal = roster
        .verify_event(store.signed_event(last).unwrap())
        .unwrap();
    assert_eq!(removal.object, object);
    assert!(removal.change.entry().is_none());
    assert_eq!(store.replica().clock()[&peer], 3);
    drop(store);
    let mut store = Store::open(&database, roster.membership().clone()).unwrap();
    let history = store.archives(SystemTime::now()).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].event, second);
    assert_eq!(history[0].replaced_by, last);
    assert_eq!(history[1].event, first);
    assert_eq!(history[1].replaced_by, second);
    assert_eq!(history[1].path, Some(vec!["song.tja".into()]));
    assert_eq!(store.protections().count(), 2);
    assert!(store
        .protections()
        .all(|protection| protection.can_release(store.replica().clock())));
    let checkpoint = store.generation_checkpoint(SystemTime::now()).unwrap();
    assert_eq!(checkpoint.heads, [last]);
    assert!(checkpoint
        .events
        .iter()
        .any(|event| event.id == last && event.change.entry().is_none()));
    assert!(checkpoint.events.iter().any(|event| event.id == first));
    assert_eq!(checkpoint.archives, history);
    let mut bytes = checkpoint.encode().unwrap();
    let administrator = PeerSigningKey::from_bytes(&[27; 32]);
    let transition = GenerationTransition {
        share: checkpoint.share,
        previous: checkpoint.generation,
        next: checkpoint.generation + 1,
        checkpoint: sha256(&bytes),
        members: [(peer, Role::Writer)].into(),
    };
    let wire = transition.sign(&administrator).unwrap();
    let verified =
        GenerationTransition::verify(&wire, &administrator.verifying_key(), checkpoint.share, 1)
            .unwrap();
    assert_eq!(verified.verify_checkpoint(&bytes), Ok(checkpoint));
    *bytes.last_mut().unwrap() ^= 1;
    assert_eq!(
        verified.verify_checkpoint(&bytes),
        Err(TransitionError::WrongCheckpoint)
    );
    assert_eq!(std::fs::read(source).unwrap(), b"second");
    for contents in [b"first".as_slice(), b"second".as_slice()] {
        assert!(blobs
            .open_verified(Blob {
                hash: sha256(contents),
                length: contents.len() as u64
            })
            .is_ok());
    }
}

#[test]
fn shared_move_keeps_the_file_identity_and_hash_and_refuses_collisions() {
    use p2p_sync_core::{
        projection::Projection,
        publish::{publish_tree, relocate_file},
    };
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path().join("share");
    std::fs::create_dir_all(root.join("folder")).unwrap();
    std::fs::write(root.join("song.tja"), b"song").unwrap();
    std::fs::write(root.join("folder/taken.tja"), b"taken").unwrap();
    let key = PeerSigningKey::from_bytes(&[15; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([8; 16], 1, [(peer, Role::Writer)].into());
    let blobs = BlobStore::open(folder.path().join("files")).unwrap();
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
    publish_tree(&root, &mut store, &roster, &key, &blobs).unwrap();
    let (object, base) = store
        .replica()
        .objects()
        .find_map(|id| {
            let head = store.replica().heads(id).next()?;
            (head.change.entry()?.name == "song.tja").then_some((id, head.id))
        })
        .unwrap();
    let parent = store
        .replica()
        .objects()
        .find(|&id| {
            Projection::new(store.replica())
                .path(id)
                .is_ok_and(|path| path == ["folder"])
        })
        .unwrap();
    assert!(matches!(
        relocate_file(&mut store, &roster, &key, base, Some(parent), "TAKEN.tja"),
        Err(PublishError::NameConflict)
    ));
    let id = relocate_file(&mut store, &roster, &key, base, Some(parent), "renamed.tja").unwrap();
    assert_eq!(store.replica().get(id).unwrap().object, object);
    assert_eq!(
        store
            .replica()
            .get(base)
            .unwrap()
            .change
            .entry()
            .unwrap()
            .kind,
        store
            .replica()
            .get(id)
            .unwrap()
            .change
            .entry()
            .unwrap()
            .kind,
    );
    assert_eq!(
        Projection::new(store.replica()).path(object).unwrap(),
        ["folder", "renamed.tja"]
    );
    assert_eq!(store.protections().count(), 0);
    assert_eq!(std::fs::read(root.join("song.tja")).unwrap(), b"song");
}

#[test]
fn tree_publication_is_repeatable_and_skips_directory_links() {
    use p2p_sync_core::{projection::Projection, publish::publish_tree};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir_all(source.join("譜面")).unwrap();
    std::fs::create_dir(source.join("空")).unwrap();
    std::fs::write(source.join("譜面/song.tja"), b"song").unwrap();

    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.tja"), b"do not publish").unwrap();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&outside, source.join("link")).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&outside, source.join("link")).is_ok();

    let key = PeerSigningKey::from_bytes(&[11; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([4; 16], 1, [(peer, Role::Writer)].into());
    let blobs = BlobStore::open(root.path().join("completed")).unwrap();
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
    for _ in 0..2 {
        let report = publish_tree(&source, &mut store, &roster, &key, &blobs).unwrap();
        assert_eq!(
            (report.files, report.directories, report.skipped),
            (1, 2, usize::from(linked))
        );
        assert_eq!(store.replica().clock()[&peer], 3);
    }
    let projection = Projection::new(store.replica());
    assert!(store
        .replica()
        .objects()
        .any(|object| projection.path(object) == Ok(vec!["譜面", "song.tja"])));
}

#[test]
fn refresh_updates_changed_files_without_deleting_missing_files_or_reviving_tombstones() {
    use p2p_sync_core::publish::{publish_tree, refresh_tree};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let chart = source.join("譜面.tja");
    let absent = source.join("removed-locally.tja");
    std::fs::write(&chart, b"old").unwrap();
    std::fs::write(&absent, b"keep").unwrap();
    let key = PeerSigningKey::from_bytes(&[19; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([4; 16], 1, [(peer, Role::Writer)].into());
    let blobs = BlobStore::open(root.path().join("blobs")).unwrap();
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
    publish_tree(&source, &mut store, &roster, &key, &blobs).unwrap();

    std::fs::write(&chart, b"new").unwrap();
    std::fs::remove_file(absent).unwrap();
    refresh_tree(&source, &mut store, &roster, &key, &blobs).unwrap();
    refresh_tree(&source, &mut store, &roster, &key, &blobs).unwrap();
    assert_eq!(store.replica().clock()[&peer], 3);
    let head = p2p_sync_core::replica::EventId { peer, sequence: 3 };
    assert_eq!(
        store
            .replica()
            .get(head)
            .unwrap()
            .change
            .entry()
            .unwrap()
            .kind,
        EntryKind::File(sha256(b"new"))
    );
    assert_eq!(store.replica().objects().count(), 2);

    revise_file(None, &mut store, &roster, &key, &blobs, head).unwrap();
    assert!(matches!(
        refresh_tree(&source, &mut store, &roster, &key, &blobs),
        Err(PublishError::NameConflict)
    ));
    assert_eq!(store.replica().clock()[&peer], 4);
}
