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
    let mut stream = Scripted(Cursor::new(request));
    assert!(matches!(
        ShareSession::default().serve_next(&mut stream, &mut store, &blobs, &roster),
        Err(WireError::UnavailableContent)
    ));
}
