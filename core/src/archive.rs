use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};

use crate::{
    blob::{Blob, BlobStore},
    materialize::AppliedFile,
    replica::{ContentHash, EntryKind, EventId, ObjectId, PeerId},
    retention::{PeerState, Protection},
    store::{decode, decode_column, encode, Store, StoreError},
};

pub const RETENTION_SECONDS: i64 = 48 * 60 * 60;

#[derive(Debug, Serialize)]
pub struct SyncDetails {
    pub originals: Vec<ProtectedOriginal>,
    pub conflicts: Vec<FileConflict>,
    pub history: Vec<ArchivedFile>,
}

#[derive(Debug, Serialize)]
pub struct FileConflict {
    pub object: String,
    pub versions: Vec<ConflictVersion>,
}

#[derive(Debug, Serialize)]
pub struct ConflictVersion {
    pub peer: String,
    pub sequence: u64,
    pub name: Option<String>,
    pub kind: &'static str,
    pub bytes: Option<u64>,
}

impl Store {
    pub fn file_conflicts(&self, blobs: &BlobStore) -> io::Result<Vec<FileConflict>> {
        let replica = self.replica();
        replica
            .objects()
            .filter(|&object| replica.has_conflict(object))
            .map(|object| {
                let versions = replica
                    .heads(object)
                    .map(|event| {
                        let entry = event.change.entry();
                        let (kind, bytes) = match entry.map(|entry| &entry.kind) {
                            Some(EntryKind::File(hash)) => {
                                let bytes = match blobs.open_candidate(*hash) {
                                    Ok(file) => {
                                        let metadata = file.metadata()?;
                                        metadata.is_file().then_some(metadata.len())
                                    }
                                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                                    Err(error) => return Err(error),
                                };
                                ("file", bytes)
                            }
                            Some(EntryKind::Directory) => ("directory", None),
                            None => ("deleted", None),
                        };
                        Ok(ConflictVersion {
                            peer: event
                                .id
                                .peer
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect(),
                            sequence: event.id.sequence,
                            name: entry.map(|entry| entry.name.clone()),
                            kind,
                            bytes,
                        })
                    })
                    .collect::<io::Result<_>>()?;
                Ok(FileConflict {
                    object: object.iter().map(|byte| format!("{byte:02x}")).collect(),
                    versions,
                })
            })
            .collect()
    }
}

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
        self.protections()
            .filter(|protection| protection.is_active(self.replica()))
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
        archives_on(&self.connection, now)
    }
}

pub(crate) fn archives_on(
    connection: &Connection,
    now: SystemTime,
) -> Result<Vec<ArchivedFile>, StoreError> {
    let threshold = unix_seconds(now)?.saturating_sub(RETENTION_SECONDS);
    let mut statement = connection.prepare(
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

impl Store {
    /// Expire history metadata, without releasing protected conflict bases
    /// or deleting any physical blob.
    pub fn prune_archives(&self, now: SystemTime) -> Result<usize, StoreError> {
        let threshold = unix_seconds(now)?.saturating_sub(RETENTION_SECONDS);
        Ok(self.connection.execute(
            "DELETE FROM file_archive WHERE recorded_at <= ?1",
            [threshold],
        )?)
    }

    /// The sender holds its state-directory lock for the duration of the
    /// sweep, excluding any operation that writes blobs or publishes events.
    pub(crate) fn sweep_sender_archives(
        &mut self,
        blobs: &BlobStore,
        now: SystemTime,
    ) -> Result<(), StoreError> {
        self.refresh()?;
        let threshold = unix_seconds(now)?.saturating_sub(RETENTION_SECONDS);
        let mut retained: BTreeSet<_> =
            self.active_protections().map(Protection::content).collect();
        retained.extend(self.replica().objects().flat_map(|object| {
            self.replica()
                .heads(object)
                .filter_map(|event| match event.change.entry()?.kind {
                    EntryKind::File(hash) => Some(hash),
                    EntryKind::Directory => None,
                })
        }));
        let mut expired = BTreeMap::<ContentHash, Vec<i64>>::new();
        {
            let mut statement = self
                .connection
                .prepare("SELECT id, recorded_at, object, payload FROM file_archive")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let record: ArchivedFile = decode(&decode_column(row, 3)?)?;
                if row.get::<_, i64>(1)? != record.archived_at
                    || row.get::<_, Vec<u8>>(2)? != record.object
                {
                    return Err(StoreError::InvalidRecord);
                }
                if record.archived_at > threshold {
                    retained.insert(record.content.hash);
                } else {
                    expired
                        .entry(record.content.hash)
                        .or_default()
                        .push(row.get(0)?);
                }
            }
        }
        let tx = self.connection.transaction()?;
        for (hash, rows) in expired {
            if retained.contains(&hash) {
                continue;
            }
            blobs.remove_archived(hash).map_err(StoreError::Io)?;
            for id in rows {
                tx.execute("DELETE FROM file_archive WHERE id = ?1", [id])?;
            }
        }
        Ok(tx.commit()?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::{PeerSigningKey, Role, Roster},
        publish::{publish_file, revise_file},
        retention::Confirmation,
        sha256,
    };
    use std::{fs, time::Duration};

    #[test]
    fn expired_sender_blob_is_kept_until_confirmed_and_no_longer_live() {
        let folder = tempfile::tempdir().unwrap();
        let source = folder.path().join("source.tja");
        let blobs = BlobStore::open(folder.path().join("files")).unwrap();
        let key = PeerSigningKey::from_bytes(&[9; 32]);
        let writer = key.verifying_key().to_bytes();
        let reader = PeerSigningKey::from_bytes(&[14; 32])
            .verifying_key()
            .to_bytes();
        let roster = Roster::new(
            [2; 16],
            1,
            [(writer, Role::Writer), (reader, Role::Reader)].into(),
        );
        let mut store = Store::open(":memory:", roster.membership().clone()).unwrap();
        let blob = Blob {
            hash: sha256(b"original"),
            length: 8,
        };
        fs::write(&source, b"original").unwrap();
        let first = publish_file(
            &source,
            &mut store,
            &roster,
            &key,
            &blobs,
            None,
            "first.tja",
        )
        .unwrap();
        let second = publish_file(
            &source,
            &mut store,
            &roster,
            &key,
            &blobs,
            None,
            "second.tja",
        )
        .unwrap();
        let deleted = revise_file(None, &mut store, &roster, &key, &blobs, first).unwrap();
        let after_expiry = || SystemTime::now() + Duration::from_secs(RETENTION_SECONDS as u64 + 1);
        store
            .sweep_sender_archives(&blobs, SystemTime::now())
            .unwrap();
        assert_eq!(store.archives(SystemTime::now()).unwrap().len(), 1);
        store.sweep_sender_archives(&blobs, after_expiry()).unwrap();
        assert!(blobs.open_verified(blob).is_ok());
        let object = store.replica().get(deleted).unwrap().object;
        store
            .confirm_verified(Confirmation {
                generation: 1,
                peer: reader,
                object,
                successor: deleted,
                sent_through: 0,
            })
            .unwrap();
        store.sweep_sender_archives(&blobs, after_expiry()).unwrap();
        assert!(blobs.open_verified(blob).is_ok());
        let deleted = revise_file(None, &mut store, &roster, &key, &blobs, second).unwrap();
        let object = store.replica().get(deleted).unwrap().object;
        store.sweep_sender_archives(&blobs, after_expiry()).unwrap();
        assert!(blobs.open_verified(blob).is_ok());
        store
            .confirm_verified(Confirmation {
                generation: 1,
                peer: reader,
                object,
                successor: deleted,
                sent_through: 0,
            })
            .unwrap();
        store.sweep_sender_archives(&blobs, after_expiry()).unwrap();
        store.sweep_sender_archives(&blobs, after_expiry()).unwrap();
        assert_eq!(
            blobs.open_verified(blob).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(store.replica().get(deleted).is_some());
        assert!(store.archives(SystemTime::now()).unwrap().is_empty());
    }
}
