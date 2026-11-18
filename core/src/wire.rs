use std::io::{self, Read, Write};

use crate::{
    auth::Roster,
    catchup::{
        outgoing_page, receive_page, CatchupError, ReceiveReport, MAX_PAGE_BYTES, MAX_PAGE_EVENTS,
    },
    replica::VersionVector,
    store::Store,
};

const MAGIC: [u8; 4] = *b"P2P1";
const PULL: u8 = 1;
const PAGE: u8 = 2;
const MAX_FRONTIER_PEERS: usize = 1024;
const FRONTIER_ENTRY_BYTES: usize = 40;
const MAX_PAGE_FRAME: usize = MAX_PAGE_BYTES + MAX_PAGE_EVENTS * 4 + 1;

#[derive(Debug)]
pub enum WireError {
    Io(io::Error),
    Catchup(CatchupError),
    InvalidFrame,
    NoProgress,
}

impl From<io::Error> for WireError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<CatchupError> for WireError {
    fn from(value: CatchupError) -> Self {
        Self::Catchup(value)
    }
}

/// Runs over an already authenticated and encrypted stream. Connections
/// with framing errors must be closed by the caller.
pub fn serve_pull(stream: &mut (impl Read + Write), store: &mut Store) -> Result<(), WireError> {
    let request = read_frame(stream, PULL, MAX_FRONTIER_PEERS * FRONTIER_ENTRY_BYTES)?;
    let remote = decode_frontier(&request)?;
    let page = outgoing_page(store, &remote)?;
    let mut response = Vec::new();
    response.push(u8::from(page.has_more));
    for event in page.events {
        response.extend_from_slice(&(event.len() as u32).to_be_bytes());
        response.extend_from_slice(event);
    }
    write_frame(stream, PAGE, &response)
}

/// Request and apply one page. Repeated calls use the local durable frontier
/// after each successful response, including after a reconnection.
pub fn pull_page(
    stream: &mut (impl Read + Write),
    store: &mut Store,
    roster: &Roster,
) -> Result<(ReceiveReport, bool), WireError> {
    store.refresh().map_err(CatchupError::Store)?;
    let frontier = store.replica().clock();
    if frontier.len() > MAX_FRONTIER_PEERS {
        return Err(WireError::InvalidFrame);
    }
    let mut request = Vec::with_capacity(frontier.len() * FRONTIER_ENTRY_BYTES);
    for (peer, sequence) in frontier {
        request.extend_from_slice(peer);
        request.extend_from_slice(&sequence.to_be_bytes());
    }
    write_frame(stream, PULL, &request)?;

    let response = read_frame(stream, PAGE, MAX_PAGE_FRAME)?;
    let (events, has_more) = decode_page(&response)?;
    let report = receive_page(store, roster, &events)?;
    if has_more && report.added == 0 {
        return Err(WireError::NoProgress);
    }
    Ok((report, has_more))
}

fn read_frame(stream: &mut impl Read, kind: u8, max_bytes: usize) -> Result<Vec<u8>, WireError> {
    let mut header = [0; 9];
    stream.read_exact(&mut header)?;
    if header[..4] != MAGIC || header[4] != kind {
        return Err(WireError::InvalidFrame);
    }
    let length = u32::from_be_bytes(header[5..].try_into().unwrap()) as usize;
    if length > max_bytes {
        return Err(WireError::InvalidFrame);
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok(body)
}

fn write_frame(stream: &mut impl Write, kind: u8, body: &[u8]) -> Result<(), WireError> {
    let mut header = [0; 9];
    header[..4].copy_from_slice(&MAGIC);
    header[4] = kind;
    let length = u32::try_from(body.len()).map_err(|_| WireError::InvalidFrame)?;
    header[5..].copy_from_slice(&length.to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn decode_frontier(body: &[u8]) -> Result<VersionVector, WireError> {
    let mut entries = body.chunks_exact(FRONTIER_ENTRY_BYTES);
    let mut result = VersionVector::new();
    for item in &mut entries {
        let peer = item[..32].try_into().unwrap();
        let sequence = u64::from_be_bytes(item[32..].try_into().unwrap());
        if sequence == 0 || result.insert(peer, sequence).is_some() {
            return Err(WireError::InvalidFrame);
        }
    }
    if !entries.remainder().is_empty() {
        return Err(WireError::InvalidFrame);
    }
    Ok(result)
}

fn decode_page(body: &[u8]) -> Result<(Vec<&[u8]>, bool), WireError> {
    let Some((&flag, mut payload)) = body.split_first() else {
        return Err(WireError::InvalidFrame);
    };
    if flag > 1 {
        return Err(WireError::InvalidFrame);
    }
    let mut events = Vec::new();
    let mut bytes = 0;
    while !payload.is_empty() {
        let Some((header, rest)) = payload.split_first_chunk::<4>() else {
            return Err(WireError::InvalidFrame);
        };
        let length = u32::from_be_bytes(*header) as usize;
        let Some((event, remaining)) = rest.split_at_checked(length) else {
            return Err(WireError::InvalidFrame);
        };
        if event.is_empty() || events.len() == MAX_PAGE_EVENTS || length > MAX_PAGE_BYTES - bytes {
            return Err(WireError::InvalidFrame);
        }
        bytes += length;
        events.push(event);
        payload = remaining;
    }
    Ok((events, flag == 1))
}
