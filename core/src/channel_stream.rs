//! Bounded message-to-byte adapter for a reliable, ordered DataChannel.
//! The WebRTC driver sends and receives the message endpoints; the existing
//! sync protocol sees a blocking Read + Write stream.

use std::{
    io::{self, Cursor, Read, Write},
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
};

pub const FRAME_BYTES: usize = 16 * 1024;
const QUEUED_FRAMES: usize = 16;

pub struct ChannelStream {
    incoming: Receiver<Vec<u8>>,
    outgoing: SyncSender<Vec<u8>>,
    current: Cursor<Vec<u8>>,
}

pub struct ChannelIngress(SyncSender<Vec<u8>>);
pub struct ChannelEgress(Receiver<Vec<u8>>);

/// Keep ingress and egress on the WebRTC driver side. Dropping either endpoint
/// terminates blocked reads or writes on the sync side.
pub fn bridge() -> (ChannelStream, ChannelIngress, ChannelEgress) {
    let (incoming_tx, incoming_rx) = mpsc::sync_channel(QUEUED_FRAMES);
    let (outgoing_tx, outgoing_rx) = mpsc::sync_channel(QUEUED_FRAMES);
    (
        ChannelStream {
            incoming: incoming_rx,
            outgoing: outgoing_tx,
            current: Cursor::new(Vec::new()),
        },
        ChannelIngress(incoming_tx),
        ChannelEgress(outgoing_rx),
    )
}

impl ChannelIngress {
    /// Feed one reliable, ordered binary DataChannel message.
    pub fn try_receive(&self, frame: &[u8]) -> io::Result<()> {
        if frame.is_empty() || frame.len() > FRAME_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid channel frame"));
        }
        self.0.try_send(frame.to_vec()).map_err(|error| match error {
            TrySendError::Full(_) => io::ErrorKind::WouldBlock.into(),
            TrySendError::Disconnected(_) => io::ErrorKind::BrokenPipe.into(),
        })
    }
}

impl ChannelEgress {
    /// Returns None after the sync side closes its output.
    pub fn next(&self) -> Option<Vec<u8>> {
        self.0.recv().ok()
    }
}

impl Read for ChannelStream {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() { return Ok(0); }
        loop {
            let count = self.current.read(output)?;
            if count > 0 { return Ok(count); }
            match self.incoming.recv() {
                Ok(frame) if !frame.is_empty() && frame.len() <= FRAME_BYTES => {
                    self.current = Cursor::new(frame);
                }
                Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid channel frame")),
                Err(_) => return Ok(0),
            }
        }
    }
}

impl Write for ChannelStream {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() { return Ok(0); }
        let count = input.len().min(FRAME_BYTES);
        self.outgoing.send(input[..count].to_vec())
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_writes_and_reassembles_reads() {
        let (mut stream, ingress, egress) = bridge();
        let bytes = vec![42; FRAME_BYTES + 7];
        let first = stream.write(&bytes).unwrap();
        assert_eq!(first, FRAME_BYTES);
        stream.write_all(&bytes[first..]).unwrap();
        assert_eq!(egress.next().unwrap(), bytes[..FRAME_BYTES]);
        assert_eq!(egress.next().unwrap(), bytes[FRAME_BYTES..]);
        ingress.try_receive(&[1, 2, 3]).unwrap();
        let mut first_read = [0; 2];
        stream.read_exact(&mut first_read).unwrap();
        assert_eq!(first_read, [1, 2]);
        let mut last_read = [0; 1];
        stream.read_exact(&mut last_read).unwrap();
        assert_eq!(last_read, [3]);
        drop(ingress);
        assert_eq!(stream.read(&mut last_read).unwrap(), 0);
        drop(egress);
        assert_eq!(stream.write(&[9]).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn rejects_invalid_inbound_messages() {
        let (_, ingress, _) = bridge();
        assert_eq!(ingress.try_receive(&[]).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(ingress.try_receive(&vec![0; FRAME_BYTES + 1]).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
