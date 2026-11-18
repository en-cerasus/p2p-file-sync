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
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("song.tja");
    std::fs::write(&source, b"first").unwrap();
    let key = PeerSigningKey::from_bytes(&[11; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([4; 16], 1, [(peer, Role::Writer)].into());
    let blobs = BlobStore::open(directory.path().join("completed")).unwrap();
    let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();

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
