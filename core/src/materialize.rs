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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppliedFile {
    pub event: EventId,
    pub contents: Option<Blob>,
    pub path: Option<Vec<String>>,
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
        let path = if intent.desired.is_some() {
            let projection = Projection::new(replica);
            Some(
                projection
                    .path(intent.object)
                    .map_err(|_| MaterializeError::InvalidIntent)?
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )
        } else {
            read_applied(&tx, intent.object)?.and_then(|entry| entry.path)
        };
        if let Some((existing, existing_path)) = load_intent(&tx, intent.object)? {
            return if existing == *intent && existing_path == path {
                Ok(())
            } else {
                Err(MaterializeError::AlreadyPending)
            };
        }
        tx.execute(
            "INSERT INTO file_intent (object, payload, path) VALUES (?1, ?2, ?3)",
            params![
                intent.object.as_slice(),
                encode(intent)?,
                path.as_ref().map(encode).transpose()?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Snapshot of the last successfully reconciled filesystem version.
    pub fn applied_file(&self, object: ObjectId) -> Result<Option<AppliedFile>, MaterializeError> {
        read_applied(&self.connection, object)
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
        let Some((intent, path)) = load_intent(&tx, object)? else {
            return Ok(Recovery::NonePending);
        };

        let allow_write = matches_head(replica, &intent);
        reconcile(tx, blobs, target, &intent, path, allow_write)
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
        let Some((intent, path)) = load_intent(&tx, object)? else {
            return Ok(Recovery::NonePending);
        };
        if intent.previous.is_some() || intent.desired.is_none() {
            return Err(MaterializeError::InvalidIntent);
        }
        if !matches_head(replica, &intent) {
            return Ok(Recovery::Superseded);
        }
        let projection = Projection::new(replica);
        let current_path = projection
            .path(object)
            .map_err(|_| MaterializeError::InvalidIntent)?;
        let Some(path) = path else {
            // An intent from schema v2 has no recorded destination.
            return Ok(Recovery::Superseded);
        };
        if !path
            .iter()
            .map(String::as_str)
            .eq(current_path.iter().copied())
        {
            return Ok(Recovery::Superseded);
        }
        let mut target = NewFileTarget::open(&open_root(root)?, &current_path)?;
        reconcile(tx, blobs, &mut target, &intent, Some(path), true)
    }

    /// Apply a tombstone only to the recorded local version at its original
    /// path. Windows locks and removes the exact verified file by handle.
    #[cfg(windows)]
    pub fn recover_deleted_file(
        &mut self,
        blobs: &BlobStore,
        root: &Path,
        object: ObjectId,
    ) -> Result<Recovery, MaterializeError> {
        let (tx, replica) = self.begin_indexed_write()?;
        let Some((intent, path)) = load_intent(&tx, object)? else {
            return Ok(Recovery::NonePending);
        };
        let (Some(previous), Some(path)) = (intent.previous, path) else {
            return Err(MaterializeError::InvalidIntent);
        };
        if intent.desired.is_some() {
            return Err(MaterializeError::InvalidIntent);
        }
        let Some(applied) = read_applied(&tx, object)? else {
            return Ok(Recovery::Superseded);
        };
        if !matches_head(replica, &intent)
            || applied.contents != Some(previous)
            || applied.path.as_ref() != Some(&path)
        {
            return Ok(Recovery::Superseded);
        }
        let Some(entry) = replica
            .get(applied.event)
            .filter(|event| event.object == object)
            .and_then(|event| event.change.entry())
        else {
            return Ok(Recovery::Superseded);
        };
        let projection = Projection::new(replica);
        let Some((name, ancestors)) = path.split_last() else {
            return Ok(Recovery::Superseded);
        };
        let parent_matches = match entry.parent {
            Some(parent) => projection
                .path(parent)
                .is_ok_and(|current| current.into_iter().eq(ancestors.iter().map(String::as_str))),
            None => ancestors.is_empty(),
        };
        if entry.name != *name || !parent_matches || !projection.vacant(entry.parent, name) {
            return Ok(Recovery::Superseded);
        }
        let components: Vec<_> = path.iter().map(String::as_str).collect();
        let mut target = NewFileTarget::existing(&open_root(root)?, &components)?;
        reconcile(tx, blobs, &mut target, &intent, Some(path), true)
    }
}

fn reconcile(
    tx: rusqlite::Transaction<'_>,
    blobs: &BlobStore,
    target: &mut impl FileTarget,
    intent: &FileIntent,
    path: Option<Vec<String>>,
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

    let applied = AppliedFile {
        event: intent.event,
        contents: intent.desired,
        path,
    };
    tx.execute(
        "INSERT INTO applied_file (object, payload) VALUES (?1, ?2)
         ON CONFLICT(object) DO UPDATE SET payload = excluded.payload",
        params![object.as_slice(), encode(&applied)?],
    )?;
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
) -> Result<Option<(FileIntent, Option<Vec<String>>)>, MaterializeError> {
    let bytes = connection
        .query_row(
            "SELECT payload, path FROM file_intent WHERE object = ?1",
            [object.as_slice()],
            |row| Ok((decode_column(row, 0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
        )
        .optional()?;
    let Some((bytes, path)) = bytes else {
        return Ok(None);
    };
    let intent: FileIntent = decode(&bytes)?;
    if intent.object != object {
        return Err(MaterializeError::InvalidIntent);
    }
    let path = path.as_deref().map(decode).transpose()?;
    Ok(Some((intent, path)))
}

fn read_applied(
    connection: &rusqlite::Connection,
    object: ObjectId,
) -> Result<Option<AppliedFile>, MaterializeError> {
    let bytes = connection
        .query_row(
            "SELECT payload FROM applied_file WHERE object = ?1",
            [object.as_slice()],
            |row| decode_column(row, 0),
        )
        .optional()?;
    Ok(bytes.as_deref().map(decode).transpose()?)
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
