use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::{
    auth::Roster,
    blob::{Blob, BlobStore},
    catchup::{
        outgoing_page, receive_page, CatchupError, ReceiveReport, MAX_PAGE_BYTES, MAX_PAGE_EVENTS,
    },
    chunks::{Manifest, CHUNK_BYTES, MAX_CHUNKS},
    replica::{ContentHash, EntryKind, EventId, VersionVector},
    store::Store,
};

const MAGIC: [u8; 4] = *b"P2P1";
const PULL: u8 = 1;
const PAGE: u8 = 2;
const GET_MANIFEST: u8 = 3;
const MANIFEST: u8 = 4;
const GET_CHUNK: u8 = 5;
const CHUNK: u8 = 6;
const CONFIRM: u8 = 7;
const CONFIRMED: u8 = 8;
const FINISH: u8 = 9;
const FINISHED: u8 = 10;
const CONTENT_UNAVAILABLE: u8 = 11;
const GET_CONFIRMATIONS: u8 = 12;
const CONFIRMATIONS: u8 = 13;
const MAX_CONFIRMATION_FRAME: usize = 320;
const CONFIRMATION_PAGE: usize = 128;
const MAX_CONFIRMATIONS_FRAME: usize = 8 + CONFIRMATION_PAGE * (4 + MAX_CONFIRMATION_FRAME);
const MAX_MANIFEST_BYTES: usize = 8 + MAX_CHUNKS * 32;
const MAX_FRONTIER_PEERS: usize = 1024;
const FRONTIER_ENTRY_BYTES: usize = 40;
const MAX_PAGE_FRAME: usize = MAX_PAGE_BYTES + MAX_PAGE_EVENTS * 4 + 1;

#[derive(Debug)]
pub enum WireError {
    Io(io::Error),
    Catchup(CatchupError),
    InvalidFrame,
    NoProgress,
    UnavailableContent,
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
    write_page(stream, store, &request)
}

fn write_page(stream: &mut impl Write, store: &mut Store, request: &[u8]) -> Result<(), WireError> {
    let remote = decode_frontier(request)?;
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

/// Serve metadata for a version that the caller has authorized for this
/// authenticated peer. This API never looks up arbitrary requested hashes.
pub fn serve_manifest(
    stream: &mut (impl Read + Write),
    manifest: &Manifest,
) -> Result<(), WireError> {
    let request = read_frame(stream, GET_MANIFEST, 32)?;
    if request != manifest.file().hash {
        return Err(WireError::UnavailableContent);
    }
    write_manifest(stream, manifest)
}

fn write_manifest(stream: &mut impl Write, manifest: &Manifest) -> Result<(), WireError> {
    let mut response = Vec::with_capacity(8 + manifest.hashes().len() * 32);
    response.extend_from_slice(&manifest.file().length.to_be_bytes());
    for hash in manifest.hashes() {
        response.extend_from_slice(hash);
    }
    write_frame(stream, MANIFEST, &response)
}

pub fn pull_manifest(
    stream: &mut (impl Read + Write),
    store: &Store,
    event: EventId,
) -> Result<Manifest, WireError> {
    if store.signed_event(event).is_none() {
        return Err(WireError::UnavailableContent);
    }
    let Some(EntryKind::File(expected)) = store
        .replica()
        .get(event)
        .and_then(|event| event.change.entry())
        .map(|entry| &entry.kind)
    else {
        return Err(WireError::UnavailableContent);
    };
    write_frame(stream, GET_MANIFEST, expected)?;
    let (kind, length) = read_header(stream)?;
    let response = match kind {
        MANIFEST => read_body(stream, length, MAX_MANIFEST_BYTES)?,
        CONTENT_UNAVAILABLE if length == 0 => return Err(WireError::UnavailableContent),
        _ => return Err(WireError::InvalidFrame),
    };
    let Some((length, hashes)) = response.split_first_chunk::<8>() else {
        return Err(WireError::InvalidFrame);
    };
    let mut chunks = hashes.chunks_exact(32);
    let hashes: Vec<ContentHash> = (&mut chunks).map(|hash| hash.try_into().unwrap()).collect();
    if !chunks.remainder().is_empty() {
        return Err(WireError::InvalidFrame);
    }
    Manifest::new(
        Blob {
            hash: *expected,
            length: u64::from_be_bytes(*length),
        },
        hashes,
    )
    .map_err(|_| WireError::InvalidFrame)
}

/// Serve a chunk of an authorized version using its immutable source.
/// The caller opens that source once and can reuse it for successive requests.
pub fn serve_chunk(
    stream: &mut (impl Read + Write),
    manifest: &Manifest,
    source: &mut (impl Read + Seek),
) -> Result<(), WireError> {
    let request = read_frame(stream, GET_CHUNK, 36)?;
    write_chunk(stream, manifest, source, &request)
}

fn write_chunk(
    stream: &mut impl Write,
    manifest: &Manifest,
    source: &mut (impl Read + Seek),
    request: &[u8],
) -> Result<(), WireError> {
    if request.len() != 36 {
        return Err(WireError::InvalidFrame);
    }
    if request[..32] != manifest.file().hash {
        return Err(WireError::UnavailableContent);
    }
    let index = u32::from_be_bytes(request[32..].try_into().unwrap()) as usize;
    let chunk = manifest.chunk(index).ok_or(WireError::UnavailableContent)?;
    source.seek(SeekFrom::Start(index as u64 * CHUNK_BYTES))?;
    let mut data = vec![0; chunk.length as usize];
    source.read_exact(&mut data)?;
    if crate::sha256(&data) != chunk.hash {
        return Err(WireError::UnavailableContent);
    }
    write_frame(stream, CHUNK, &data)
}

pub fn pull_chunk(
    stream: &mut (impl Read + Write),
    manifest: &Manifest,
    cache: &BlobStore,
    index: usize,
) -> Result<(), WireError> {
    let chunk = manifest.chunk(index).ok_or(WireError::UnavailableContent)?;
    let mut request = [0; 36];
    request[..32].copy_from_slice(&manifest.file().hash);
    request[32..].copy_from_slice(&(index as u32).to_be_bytes());
    write_frame(stream, GET_CHUNK, &request)?;
    let response = read_frame(stream, CHUNK, chunk.length as usize)?;
    manifest.receive_chunk(cache, index, response.as_slice())?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionProgress {
    Continue,
    Finished,
}

/// Per-connection file selection. Run only after the caller authenticates
/// the peer for this exact share generation. A reconnect selects a manifest
/// again and may continue requesting only missing chunks.
#[derive(Default)]
pub struct ShareSession {
    selected: Option<(Manifest, std::fs::File)>,
}

impl ShareSession {
    pub fn serve_next(
        &mut self,
        stream: &mut (impl Read + Write),
        store: &mut Store,
        completed: &BlobStore,
        roster: &Roster,
    ) -> Result<SessionProgress, WireError> {
        if store.membership() != roster.membership() {
            return Err(CatchupError::Authentication(crate::auth::AuthError::WrongScope).into());
        }
        let (kind, length) = read_header(stream)?;
        match kind {
            PULL => {
                let body = read_body(stream, length, MAX_FRONTIER_PEERS * FRONTIER_ENTRY_BYTES)?;
                write_page(stream, store, &body)?;
            }
            GET_MANIFEST => {
                self.selected = None;
                let body = read_body(stream, length, 32)?;
                let hash: ContentHash = body.try_into().map_err(|_| WireError::InvalidFrame)?;
                store.refresh().map_err(CatchupError::Store)?;
                if !store.has_signed_content(hash) {
                    write_frame(stream, CONTENT_UNAVAILABLE, &[])?;
                    return Ok(SessionProgress::Continue);
                }
                let mut file = match completed.open_candidate(hash) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        write_frame(stream, CONTENT_UNAVAILABLE, &[])?;
                        return Ok(SessionProgress::Continue);
                    }
                    Err(error) => return Err(error.into()),
                };
                let metadata = file.metadata()?;
                if !metadata.is_file() {
                    return Err(WireError::UnavailableContent);
                }
                let manifest = Manifest::build(&mut file, metadata.len())?;
                if manifest.file().hash != hash {
                    return Err(WireError::UnavailableContent);
                }
                write_manifest(stream, &manifest)?;
                self.selected = Some((manifest, file));
            }
            GET_CHUNK => {
                let body = read_body(stream, length, 36)?;
                let (manifest, file) = self
                    .selected
                    .as_mut()
                    .ok_or(WireError::UnavailableContent)?;
                write_chunk(stream, manifest, file, &body)?;
            }
            CONFIRM => {
                let body = read_body(stream, length, MAX_CONFIRMATION_FRAME)?;
                match roster.receive_confirmation(store, &body) {
                    Ok(()) => {}
                    // A valid signer may have seen a successor this server
                    // has not imported yet. It can be retried next session.
                    Err(crate::auth::AuthError::Store(
                        crate::store::StoreError::InvalidConfirmation,
                    )) if roster.verify_confirmation(&body).is_ok_and(|confirmation| {
                        store.replica().get(confirmation.successor).is_none()
                    }) => {}
                    Err(error) => return Err(CatchupError::Authentication(error).into()),
                }
                write_frame(stream, CONFIRMED, &[])?;
            }
            GET_CONFIRMATIONS => {
                let body = read_body(stream, length, 8)?;
                let cursor =
                    i64::from_be_bytes(body.try_into().map_err(|_| WireError::InvalidFrame)?);
                if cursor < 0 {
                    return Err(WireError::InvalidFrame);
                }
                store.refresh().map_err(CatchupError::Store)?;
                let page = store
                    .signed_confirmations_page(cursor, CONFIRMATION_PAGE)
                    .map_err(CatchupError::Store)?;
                let mut response = Vec::new();
                response
                    .extend_from_slice(&page.last().map_or(cursor, |(id, _)| *id).to_be_bytes());
                for (_, wire) in page {
                    if wire.is_empty()
                        || wire.len() > MAX_CONFIRMATION_FRAME
                        || roster.verify_confirmation(&wire).is_err()
                    {
                        return Err(WireError::InvalidFrame);
                    }
                    response.extend_from_slice(&(wire.len() as u32).to_be_bytes());
                    response.extend_from_slice(&wire);
                }
                write_frame(stream, CONFIRMATIONS, &response)?;
            }
            FINISH if length == 0 => {
                write_frame(stream, FINISHED, &[])?;
                return Ok(SessionProgress::Finished);
            }
            _ => return Err(WireError::InvalidFrame),
        }
        Ok(SessionProgress::Continue)
    }
}

/// Submit a signed application confirmation and wait until it is recorded.
pub fn send_confirmation(stream: &mut (impl Read + Write), wire: &[u8]) -> Result<(), WireError> {
    if wire.len() > MAX_CONFIRMATION_FRAME {
        return Err(WireError::InvalidFrame);
    }
    write_frame(stream, CONFIRM, wire)?;
    read_frame(stream, CONFIRMED, 0)?;
    Ok(())
}

/// Exchange all currently stored signed acknowledgements after event catch-up.
/// Repeated delivery is safe; the store keeps only the highest signed frontier.
pub fn exchange_confirmations(
    stream: &mut (impl Read + Write),
    store: &mut Store,
    roster: &Roster,
) -> Result<(), WireError> {
    let mut cursor = 0_i64;
    let mut complete = false;
    for _ in 0..256 {
        write_frame(stream, GET_CONFIRMATIONS, &cursor.to_be_bytes())?;
        let body = read_frame(stream, CONFIRMATIONS, MAX_CONFIRMATIONS_FRAME)?;
        let Some((next, mut rest)) = body.split_first_chunk::<8>() else {
            return Err(WireError::InvalidFrame);
        };
        let next = i64::from_be_bytes(*next);
        if next < cursor {
            return Err(WireError::InvalidFrame);
        }
        let mut count = 0;
        while !rest.is_empty() {
            let Some((header, payload)) = rest.split_first_chunk::<4>() else {
                return Err(WireError::InvalidFrame);
            };
            let length = u32::from_be_bytes(*header) as usize;
            let Some((wire, remaining)) = payload.split_at_checked(length) else {
                return Err(WireError::InvalidFrame);
            };
            if length == 0 || length > MAX_CONFIRMATION_FRAME || count == CONFIRMATION_PAGE {
                return Err(WireError::InvalidFrame);
            }
            roster
                .receive_confirmation(store, wire)
                .map_err(CatchupError::Authentication)?;
            rest = remaining;
            count += 1;
        }
        if (count > 0 && next <= cursor) || (count == 0 && next != cursor) {
            return Err(WireError::InvalidFrame);
        }
        cursor = next;
        if count < CONFIRMATION_PAGE {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(WireError::InvalidFrame);
    }
    let mut cursor = 0_i64;
    let mut complete = false;
    for _ in 0..256 {
        let page = store
            .signed_confirmations_page(cursor, CONFIRMATION_PAGE)
            .map_err(CatchupError::Store)?;
        if page.is_empty() {
            complete = true;
            break;
        }
        for (id, wire) in page {
            roster
                .verify_confirmation(&wire)
                .map_err(CatchupError::Authentication)?;
            send_confirmation(stream, &wire)?;
            cursor = id;
        }
    }
    if !complete {
        return Err(WireError::InvalidFrame);
    }
    Ok(())
}

pub fn finish_session(stream: &mut (impl Read + Write)) -> Result<(), WireError> {
    write_frame(stream, FINISH, &[])?;
    read_frame(stream, FINISHED, 0)?;
    Ok(())
}

fn read_header(stream: &mut impl Read) -> Result<(u8, usize), WireError> {
    let mut header = [0; 9];
    stream.read_exact(&mut header)?;
    if header[..4] != MAGIC {
        return Err(WireError::InvalidFrame);
    }
    Ok((
        header[4],
        u32::from_be_bytes(header[5..].try_into().unwrap()) as usize,
    ))
}

fn read_body(stream: &mut impl Read, length: usize, limit: usize) -> Result<Vec<u8>, WireError> {
    if length > limit {
        return Err(WireError::InvalidFrame);
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    Ok(body)
}

fn read_frame(stream: &mut impl Read, kind: u8, max_bytes: usize) -> Result<Vec<u8>, WireError> {
    let (actual, length) = read_header(stream)?;
    if actual != kind {
        return Err(WireError::InvalidFrame);
    }
    read_body(stream, length, max_bytes)
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
