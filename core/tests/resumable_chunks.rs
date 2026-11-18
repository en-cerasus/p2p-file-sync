use std::io::{self, Read};

use p2p_sync_core::{
    blob::{Blob, BlobStore},
    chunks::{Manifest, CHUNK_BYTES, MAX_CHUNKS},
    sha256,
};

#[test]
fn resumes_from_verified_chunks_after_reopening_and_publishes_only_the_complete_file() {
    let root = tempfile::tempdir().unwrap();
    let chunk_dir = root.path().join("chunks");
    let completed = BlobStore::open(root.path().join("completed")).unwrap();
    let first = vec![7; CHUNK_BYTES as usize];
    let last = b"short tail";
    let data = [first.as_slice(), last].concat();
    let manifest = Manifest::build(data.as_slice(), data.len() as u64).unwrap();
    assert_eq!(manifest.file().hash, sha256(&data));
    assert_eq!(manifest.chunk(1).unwrap().length, last.len() as u64);

    {
        let cache = BlobStore::open(&chunk_dir).unwrap();
        assert_eq!(manifest.missing_chunks(&cache).unwrap(), [0, 1]);
        manifest.receive_chunk(&cache, 1, last.as_slice()).unwrap();
        assert!(manifest.finish(&cache, &completed).is_err());
        assert_eq!(
            completed.open_verified(manifest.file()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    let cache = BlobStore::open(&chunk_dir).unwrap();
    assert_eq!(manifest.missing_chunks(&cache).unwrap(), [0]);
    let mut corrupt = first.clone();
    corrupt[0] ^= 1;
    assert!(manifest
        .receive_chunk(&cache, 0, corrupt.as_slice())
        .is_err());
    assert_eq!(manifest.missing_chunks(&cache).unwrap(), [0]);
    manifest.receive_chunk(&cache, 0, first.as_slice()).unwrap();
    assert!(manifest.missing_chunks(&cache).unwrap().is_empty());
    manifest.finish(&cache, &completed).unwrap();
    manifest.finish(&cache, &completed).unwrap();
    let mut result = Vec::new();
    completed
        .open_verified(manifest.file())
        .unwrap()
        .read_to_end(&mut result)
        .unwrap();
    assert_eq!(result, data);
}

#[test]
fn rejects_inconsistent_manifests_and_accepts_empty_files() {
    let root = tempfile::tempdir().unwrap();
    let cache = BlobStore::open(root.path().join("chunks")).unwrap();
    let completed = BlobStore::open(root.path().join("completed")).unwrap();
    let empty = Manifest::build(&[][..], 0).unwrap();
    empty.finish(&cache, &completed).unwrap();
    assert!(completed.open_verified(empty.file()).is_ok());
    assert!(Manifest::build(&[1][..], 0).is_err());
    assert!(Manifest::build(&[][..], CHUNK_BYTES * (MAX_CHUNKS as u64 + 1)).is_err());

    let actual = Manifest::build(&b"abc"[..], 3).unwrap();
    assert!(Manifest::new(actual.file(), vec![]).is_err());
    let wrong_file = Blob {
        hash: sha256(b"bad"),
        length: 3,
    };
    let mismatched = Manifest::new(wrong_file, actual.hashes().to_vec()).unwrap();
    mismatched.receive_chunk(&cache, 0, &b"abc"[..]).unwrap();
    assert!(mismatched.finish(&cache, &completed).is_err());
    assert_eq!(
        completed.open_verified(wrong_file).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}
