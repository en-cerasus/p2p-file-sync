use std::{fs::File, io, path::Path};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{
    blob::{Blob, BlobStore},
    local_target::{open_root, NewFileTarget},
    projection::Projection,
    replica::{EntryKind, EventId, ObjectId, Replica},
    store::{decode, decode_column, encode, Store, StoreError},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileIntent {
    pub object: ObjectId,
    pub event: EventId,
    pub previous: Option<Blob>,
    pub desired: Option<Blob>,
}

#[derive(Debug)]
pub enum MaterializeError {
    Store(StoreError),
    Io(io::Error),
    InvalidIntent,
    AlreadyPending,
}

impl From<StoreError> for MaterializeError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<rusqlite::Error> for MaterializeError {
    fn from(value: rusqlite::Error) -> Self {
        StoreError::from(value).into()
    }
}

impl From<io::Error> for MaterializeError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// The adapter holds its filesystem coordination guard across the read and
/// write. Object IDs are resolved inside a trusted shared root by the adapter.
pub trait FileTarget {
    fn inspect(&mut self, object: ObjectId) -> io::Result<Option<Blob>>;

    /// Applies the change only if the target still matches the expected
    /// previous version, and reports false if it changed. The replacement
    /// file, when present, begins at offset zero.
    fn replace_if_current(
        &mut self,
        intent: &FileIntent,
        replacement: Option<File>,
    ) -> io::Result<bool>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recovery {
    NonePending,
    Applied,
    AlreadyApplied,
    Diverged,
    Superseded,
}

impl Store {
    /// Persist intent only after both versions are available and verified.
    /// The logical event must already be present in the causal index.
    pub fn stage_file(
        &mut self,
        blobs: &BlobStore,
        intent: &FileIntent,
    ) -> Result<(), MaterializeError> {
        for blob in [intent.previous, intent.desired].into_iter().flatten() {
            blobs.open_verified(blob)?;
        }
        let (tx, replica) = self.begin_indexed_write()?;
        if !matches_head(replica, intent) {
            return Err(MaterializeError::InvalidIntent);
        }
        if let Some(existing) = load_intent(&tx, intent.object)? {
            return if existing == *intent {
                Ok(())
            } else {
                Err(MaterializeError::AlreadyPending)
            };
        }
        tx.execute(
            "INSERT INTO file_intent (object, payload) VALUES (?1, ?2)",
            params![intent.object.as_slice(), encode(intent)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn pending_files(&self) -> Result<Vec<FileIntent>, MaterializeError> {
        let mut statement = self
            .connection
            .prepare("SELECT object, payload FROM file_intent ORDER BY object")?;
        let mut rows = statement.query([])?;
        let mut intents = Vec::new();
        while let Some(row) = rows.next()? {
            let object: Vec<u8> = row.get(0)?;
            let intent: FileIntent = decode(&decode_column(row, 1)?)?;
            if object != intent.object {
                return Err(MaterializeError::InvalidIntent);
            }
            intents.push(intent);
        }
        Ok(intents)
    }

    /// Reconcile a committed intent with the current filesystem state. The
    /// transaction serializes recovery across handles. Divergence preserves
    /// both the underlying file and the intent for conflict handling.
    pub fn recover_file(
        &mut self,
        blobs: &BlobStore,
        target: &mut impl FileTarget,
        object: ObjectId,
    ) -> Result<Recovery, MaterializeError> {
        let (tx, replica) = self.begin_indexed_write()?;
        let Some(intent) = load_intent(&tx, object)? else {
            return Ok(Recovery::NonePending);
        };

        let allow_write = matches_head(replica, &intent);
        reconcile(tx, blobs, target, &intent, allow_write)
    }

    /// Publish a new file under a user-selected local root. This backend
    /// never overwrites an existing path, including one created concurrently.
    pub fn recover_new_file(
        &mut self,
        blobs: &BlobStore,
        root: &Path,
        object: ObjectId,
    ) -> Result<Recovery, MaterializeError> {
        let (tx, replica) = self.begin_indexed_write()?;
        let Some(intent) = load_intent(&tx, object)? else {
            return Ok(Recovery::NonePending);
        };
        if intent.previous.is_some() || intent.desired.is_none() {
            return Err(MaterializeError::InvalidIntent);
        }
        if !matches_head(replica, &intent) {
            return Ok(Recovery::Superseded);
        }
        let projection = Projection::new(replica);
        let path = projection
            .path(object)
            .map_err(|_| MaterializeError::InvalidIntent)?;
        let mut target = NewFileTarget::open(&open_root(root)?, &path)?;
        reconcile(tx, blobs, &mut target, &intent, true)
    }
}

fn reconcile(
    tx: rusqlite::Transaction<'_>,
    blobs: &BlobStore,
    target: &mut impl FileTarget,
    intent: &FileIntent,
    allow_write: bool,
) -> Result<Recovery, MaterializeError> {
    let object = intent.object;
    if let Some(previous) = intent.previous {
        blobs.open_verified(previous)?;
    }
    let replacement = intent
        .desired
        .map(|blob| blobs.open_verified(blob))
        .transpose()?;
    let current = target.inspect(object)?;
    let result = if current == intent.desired {
        Recovery::AlreadyApplied
    } else if current != intent.previous {
        return Ok(Recovery::Diverged);
    } else if !allow_write {
        return Ok(Recovery::Superseded);
    } else if target.replace_if_current(intent, replacement)? {
        Recovery::Applied
    } else {
        return Ok(Recovery::Diverged);
    };

    tx.execute(
        "DELETE FROM file_intent WHERE object = ?1",
        [object.as_slice()],
    )?;
    tx.commit()?;
    Ok(result)
}

fn load_intent(
    connection: &rusqlite::Connection,
    object: ObjectId,
) -> Result<Option<FileIntent>, MaterializeError> {
    let bytes = connection
        .query_row(
            "SELECT payload FROM file_intent WHERE object = ?1",
            [object.as_slice()],
            |row| decode_column(row, 0),
        )
        .optional()?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let intent: FileIntent = decode(&bytes)?;
    if intent.object != object {
        return Err(MaterializeError::InvalidIntent);
    }
    Ok(Some(intent))
}

fn matches_head(replica: &Replica, intent: &FileIntent) -> bool {
    if replica.has_conflict(intent.object) {
        return false;
    }
    let Some(event) = replica
        .heads(intent.object)
        .find(|head| head.id == intent.event)
    else {
        return false;
    };
    match (
        event.change.entry().map(|entry| &entry.kind),
        intent.desired,
    ) {
        (Some(EntryKind::File(hash)), Some(blob)) => {
            blob.hash == *hash && Projection::new(replica).path(intent.object).is_ok()
        }
        (None, None) => true,
        _ => false,
    }
}
