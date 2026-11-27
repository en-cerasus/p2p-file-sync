use std::{
    io::{self, Cursor, Read, Write},
    sync::mpsc::{sync_channel, Receiver, SyncSender},
};

use p2p_sync_core::{
    auth::{PeerSigningKey, Role, Roster},
    blob::BlobStore,
    recovery::{verify_anchor, RecoveryError},
    replica::{Change, Entry, EntryKind, Event, EventId, VersionVector},
    store::Store,
    wire::{finish_session, pull_recovery_anchor, SessionProgress, ShareSession},
};

struct Pipe {
    incoming: Receiver<Vec<u8>>,
    outgoing: SyncSender<Vec<u8>>,
    current: Cursor<Vec<u8>>,
}

fn pipe_pair() -> (Pipe, Pipe) {
    let (left_tx, left_rx) = sync_channel(4);
    let (right_tx, right_rx) = sync_channel(4);
    (
        Pipe {
            incoming: left_rx,
            outgoing: right_tx,
            current: Cursor::new(Vec::new()),
        },
        Pipe {
            incoming: right_rx,
            outgoing: left_tx,
            current: Cursor::new(Vec::new()),
        },
    )
}

impl Read for Pipe {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.current.read(output)?;
            if count != 0 {
                return Ok(count);
            }
            self.current = Cursor::new(
                self.incoming
                    .recv()
                    .map_err(|_| io::ErrorKind::UnexpectedEof)?,
            );
        }
    }
}

impl Write for Pipe {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.outgoing
            .send(input.to_vec())
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn signed_anchor_moves_over_existing_session_and_exposes_missing_history() {
    let folder = tempfile::tempdir().unwrap();
    let key = PeerSigningKey::from_bytes(&[29; 32]);
    let peer = key.verifying_key().to_bytes();
    let roster = Roster::new([5; 16], 1, [(peer, Role::Writer)].into());
    let event = Event {
        id: EventId { peer, sequence: 1 },
        context: VersionVector::new(),
        object: [7; 16],
        change: Change::Create(Entry {
            parent: None,
            name: "song.tja".into(),
            kind: EntryKind::File([8; 32]),
        }),
    };
    let signed = roster.sign_event(&key, &event).unwrap();
    let mut sender =
        Store::open(folder.path().join("sender.db"), roster.membership().clone()).unwrap();
    roster.receive(&mut sender, &signed).unwrap();
    let blobs = BlobStore::open(folder.path().join("files")).unwrap();
    let (mut client_stream, mut server_stream) = pipe_pair();

    std::thread::scope(|threads| {
        let server = threads.spawn(|| {
            let mut session = ShareSession::default();
            assert_eq!(
                session
                    .serve_next_signed(&mut server_stream, &mut sender, &blobs, &roster, Some(&key))
                    .unwrap(),
                SessionProgress::Continue
            );
            assert_eq!(
                session
                    .serve_next_signed(&mut server_stream, &mut sender, &blobs, &roster, Some(&key))
                    .unwrap(),
                SessionProgress::Finished
            );
        });
        let anchor = pull_recovery_anchor(&mut client_stream, &roster, peer).unwrap();
        finish_session(&mut client_stream).unwrap();
        server.join().unwrap();

        let mut restored = Store::open(
            folder.path().join("restored.db"),
            roster.membership().clone(),
        )
        .unwrap();
        assert!(
            matches!(verify_anchor(&roster, &mut restored, &anchor, peer), Err(RecoveryError::Behind(id)) if id == event.id)
        );
        roster.receive(&mut restored, &signed).unwrap();
        verify_anchor(&roster, &mut restored, &anchor, peer).unwrap();
    });
}
