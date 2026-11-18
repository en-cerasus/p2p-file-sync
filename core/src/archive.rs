use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Transaction};
use serde::{Deserialize, Serialize};

use crate::{
    blob::{Blob, BlobStore},
    materialize::AppliedFile,
    replica::{ContentHash, EventId, ObjectId, PeerId},
    retention::{PeerState, Protection},
    store::{decode, decode_column, encode, Store, StoreError},
};

pub const RETENTION_SECONDS: i64 = 48 * 60 * 60;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArchivedFile {
    pub object: ObjectId,
    pub event: EventId,
    pub replaced_by: EventId,
    pub content: Blob,
    pub path: Option<Vec<String>>,
    pub archived_at: i64,
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct ProtectedStorage {
    pub files: u64,
    pub bytes: u64,
    pub missing: u64,
}

impl Store {
    pub(crate) fn active_protections(&self) -> impl Iterator<Item = &Protection> {
        let replica = self.replica();
        self.protections().filter(move |protection| {
            !protection.can_release(replica.clock())
                || replica
                    .get(protection.original())
                    .is_some_and(|event| replica.has_conflict(event.object))
        })
    }

    /// List unique contents required by unconfirmed peers or a live conflict.
    pub fn protected_originals(&self, blobs: &BlobStore) -> io::Result<Vec<ProtectedOriginal>> {
        let replica = self.replica();
        let mut originals = BTreeMap::<ContentHash, OriginalReferences>::new();
        for protection in self.active_protections() {
            let references = originals.entry(protection.content()).or_default();
            if let Some(event) = replica.get(protection.original()) {
                references.conflict |= replica.has_conflict(event.object);
                if let Some(entry) = event.change.entry() {
                    references.names.insert(entry.name.clone());
                }
            }
            for (peer, state) in protection.peer_states(replica.clock()) {
                let waiting = match state {
                    PeerState::AwaitingConfirmation => WaitingFor::Confirmation,
                    PeerState::AwaitingEvents { .. } => WaitingFor::Events,
                    PeerState::Applied => continue,
                };
                references
                    .waiting
                    .entry(peer)
                    .and_modify(|state| {
                        *state = (*state).max(waiting);
                    })
                    .or_insert(waiting);
            }
        }
        originals
            .into_iter()
            .map(|(hash, references)| {
                let bytes = match blobs.open_candidate(hash) {
                    Ok(file) => {
                        let metadata = file.metadata()?;
                        metadata.is_file().then_some(metadata.len())
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error),
                };
                Ok(ProtectedOriginal {
                    hash,
                    names: references.names,
                    bytes,
                    conflict: references.conflict,
                    waiting: references
                        .waiting
                        .into_iter()
                        .map(|(peer, state)| WaitingPeer {
                            peer: peer.iter().map(|byte| format!("{byte:02x}")).collect(),
                            state,
                        })
                        .collect(),
                })
            })
            .collect()
    }

    pub fn protected_storage(&self, blobs: &BlobStore) -> io::Result<ProtectedStorage> {
        Ok(ProtectedStorage::from_originals(
            &self.protected_originals(blobs)?,
        ))
    }
}

impl ProtectedStorage {
    pub(crate) fn from_originals(originals: &[ProtectedOriginal]) -> Self {
        let mut result = Self::default();
        for original in originals {
            if let Some(bytes) = original.bytes {
                result.files += 1;
                result.bytes = result.bytes.saturating_add(bytes);
            } else {
                result.missing += 1;
            }
        }
        result
    }
}

impl Store {
    /// Return only entries within the normal history window. Blob ownership
    /// is separate: a file may still be needed for conflict recovery.
    pub fn archives(&self, now: SystemTime) -> Result<Vec<ArchivedFile>, StoreError> {
        let threshold = unix_seconds(now)?.saturating_sub(RETENTION_SECONDS);
        let mut statement = self.connection.prepare(
            "SELECT object, recorded_at, payload FROM file_archive
             WHERE recorded_at > ?1 ORDER BY recorded_at DESC, id DESC",
        )?;
        let mut rows = statement.query([threshold])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            let record: ArchivedFile = decode(&decode_column(row, 2)?)?;
            let object: Vec<u8> = row.get(0)?;
            if object != record.object || record.archived_at != row.get::<_, i64>(1)? {
                return Err(StoreError::InvalidRecord);
            }
            entries.push(record);
        }
        Ok(entries)
    }

    /// Expire history metadata, without releasing protected conflict bases
    /// or deleting any physical blob.
    pub fn prune_archives(&self, now: SystemTime) -> Result<usize, StoreError> {
        let threshold = unix_seconds(now)?.saturating_sub(RETENTION_SECONDS);
        Ok(self.connection.execute(
            "DELETE FROM file_archive WHERE recorded_at <= ?1",
            [threshold],
        )?)
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct ProtectedOriginal {
    pub hash: crate::replica::ContentHash,
    pub names: BTreeSet<String>,
    /// None when the local store lacks the original contents.
    pub bytes: Option<u64>,
    pub conflict: bool,
    pub waiting: Vec<WaitingPeer>,
}

#[derive(Default)]
struct OriginalReferences {
    names: BTreeSet<String>,
    conflict: bool,
    waiting: BTreeMap<PeerId, WaitingFor>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct WaitingPeer {
    pub peer: String,
    pub state: WaitingFor,
}

// When one peer waits on multiple successors, show the least advanced state.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingFor {
    Events,
    Confirmation,
}

pub(crate) fn record_archive(
    tx: &Transaction<'_>,
    object: ObjectId,
    previous: AppliedFile,
    successor: EventId,
    desired: Option<ContentHash>,
) -> Result<(), StoreError> {
    let Some(content) = previous
        .contents
        .filter(|previous| Some(previous.hash) != desired)
    else {
        return Ok(());
    };
    let record = ArchivedFile {
        object,
        content,
        event: previous.event,
        replaced_by: successor,
        path: previous.path,
        archived_at: unix_seconds(SystemTime::now())?,
    };
    tx.execute(
        "INSERT INTO file_archive (object, recorded_at, payload) VALUES (?1, ?2, ?3)",
        params![object.as_slice(), record.archived_at, encode(&record)?],
    )?;
    Ok(())
}

fn unix_seconds(time: SystemTime) -> Result<i64, StoreError> {
    let since_epoch = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::InvalidRecord)?;
    i64::try_from(since_epoch.as_secs()).map_err(|_| StoreError::InvalidRecord)
}
