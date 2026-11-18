use p2p_sync_core::{
    auth::{PeerSigningKey, Role, Roster},
    blob::{Blob, BlobStore},
    publish::{publish_file, PublishError},
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
