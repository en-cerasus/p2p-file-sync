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
