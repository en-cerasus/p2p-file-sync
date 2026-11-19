use std::{fs::File, io, path::Path};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{
    archive::record_archive,
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

/// Platform move adapter. It must never replace an occupied destination and
/// must keep the source bytes available when a move cannot be completed.
pub trait MoveFileTarget: FileTarget {
    fn already_moved(&mut self, previous: Blob, desired: Blob, same_slot: bool)
        -> io::Result<bool>;
    fn move_if_current(&mut self, previous: Blob, same_slot: bool) -> io::Result<bool>;
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
        Ok(self.pending_files_with_paths()?.into_iter().map(|(intent, _)| intent).collect())
    }

    /// Include the path saved with the intent. A newer signed head may have a
    /// different path, so recovery must coordinate the saved path instead.
    pub fn pending_files_with_paths(&self) -> Result<Vec<(FileIntent, Option<Vec<String>>)>, MaterializeError> {
        let mut statement = self
            .connection
            .prepare("SELECT object, payload, path FROM file_intent ORDER BY object")?;
        let mut rows = statement.query([])?;
        let mut intents = Vec::new();
        while let Some(row) = rows.next()? {
            let object: Vec<u8> = row.get(0)?;
            let intent: FileIntent = decode(&decode_column(row, 1)?)?;
            if object != intent.object {
                return Err(MaterializeError::InvalidIntent);
            }
            let path: Option<Vec<u8>> = row.get(2)?;
            intents.push((intent, path.as_deref().map(decode).transpose()?));
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

    #[cfg(windows)]
    pub fn recover_updated_file(
        &mut self,
        blobs: &BlobStore,
        root: &Path,
        object: ObjectId,
    ) -> Result<Recovery, MaterializeError> {
        self.recover_updated_file_with(blobs, object, |path, intent| {
            let components: Vec<_> = path.iter().map(String::as_str).collect();
            let target = NewFileTarget::existing(&open_root(root)?, &components)?;
            Ok(UpdateTarget { target, intent: intent.clone() })
        })
    }

    /// Validate the signed head and recorded local version before creating an
    /// OS-specific target for the exact applied path. The adapter must recheck
    /// the old bytes while it holds its filesystem coordination guard.
    pub fn recover_updated_file_with<T: FileTarget>(
        &mut self,
        blobs: &BlobStore,
        object: ObjectId,
        make_target: impl FnOnce(&[String], &FileIntent) -> io::Result<T>,
    ) -> Result<Recovery, MaterializeError> {
        let (tx, replica) = self.begin_indexed_write()?;
        let Some((intent, path)) = load_intent(&tx, object)? else {
            return Ok(Recovery::NonePending);
        };
        let (Some(previous), Some(path)) = (intent.previous, path) else {
            return Err(MaterializeError::InvalidIntent);
        };
        if intent.desired.is_none() {
            return Err(MaterializeError::InvalidIntent);
        }
        let Some(applied) = read_applied(&tx, object)? else {
            return Ok(Recovery::Superseded);
        };
        let projection = Projection::new(replica);
        let current = projection.path(object);
        if !matches_head(replica, &intent)
            || applied.contents != Some(previous)
            || applied.path.as_ref() != Some(&path)
            || !current.is_ok_and(|current| current.into_iter().eq(path.iter().map(String::as_str)))
        {
            return Ok(Recovery::Superseded);
        }
        let mut target = make_target(&path, &intent)?;
        reconcile(tx, blobs, &mut target, &intent, Some(path), true)
    }

    /// Retire an out-of-date intent only when its original path still
    /// matches the verified pre- or post-operation contents.
    pub fn retire_superseded_file(
        &mut self,
        blobs: &BlobStore,
        root: &Path,
        object: ObjectId,
    ) -> Result<bool, MaterializeError> {
        let (tx, replica) = self.begin_indexed_write()?;
        let Some((intent, Some(path))) = load_intent(&tx, object)? else {
            return Ok(false);
        };
        if matches_head(replica, &intent) {
            return Ok(false);
        }
        let mut heads = replica.heads(object);
        let Some(head) = heads.next() else {
            return Ok(false);
        };
        if heads.any(|other| other.change.entry() != head.change.entry()) {
            return Ok(false);
        }
        #[cfg(windows)]
        if let (Some(previous), Some(desired)) = (intent.previous, intent.desired) {
            if let Some(old_path) = read_applied(&tx, object)?
                .filter(|applied| applied.contents == Some(previous))
                .and_then(|applied| applied.path)
                .filter(|old_path| *old_path != path)
            {
                let root = open_root(root)?;
                let old_components: Vec<_> = old_path.iter().map(String::as_str).collect();
                let new_components: Vec<_> = path.iter().map(String::as_str).collect();
                let Ok(mut old) = NewFileTarget::existing(&root, &old_components) else {
                    return Ok(false);
                };
                let same_slot = same_name_slot(&old_path, &path);
                if (same_slot && !old.is_only_spelling().unwrap_or(false))
                    || !old
                        .inspect(object)
                        .is_ok_and(|actual| actual == Some(previous))
                {
                    let Ok(mut new) = NewFileTarget::existing(&root, &new_components) else {
                        return Ok(false);
                    };
                    let moved = if same_slot {
                        new.is_only_spelling()
                    } else {
                        old.name_is_vacant()
                    };
                    if !moved.unwrap_or(false)
                        || !new
                            .inspect(object)
                            .is_ok_and(|actual| actual == Some(desired))
                    {
                        return Ok(false);
                    }
                    blobs.open_verified(desired)?;
                    if desired != previous {
                        new.finish_update(&intent)?;
                    }
                    record_applied(&tx, &intent, Some(path))?;
                }
                remove_intent(&tx, object)?;
                tx.commit()?;
                return Ok(true);
            }
        }
        if head.change.entry().is_some()
            && !Projection::new(replica)
                .path(object)
                .is_ok_and(|current| current.into_iter().eq(path.iter().map(String::as_str)))
        {
            return Ok(false);
        }

        let components: Vec<_> = path.iter().map(String::as_str).collect();
        let mut target = match NewFileTarget::existing(&open_root(root)?, &components) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let current = target.inspect(object)?;
        #[cfg(windows)]
        let current = match (current, intent.previous, intent.desired) {
            (None, Some(previous), Some(_))
                if head.change.entry().is_some()
                    && read_applied(&tx, object)?.is_some_and(|applied| {
                        applied.contents == Some(previous) && applied.path.as_ref() == Some(&path)
                    }) =>
            {
                blobs.open_verified(previous)?;
                if target.restore_update_backup(&intent, previous)? {
                    Some(previous)
                } else {
                    None
                }
            }
            _ => current,
        };
        if current != intent.previous && current != intent.desired {
            return Ok(false);
        }

        for blob in [intent.previous, intent.desired].into_iter().flatten() {
            blobs.open_verified(blob)?;
        }
        #[cfg(windows)]
        if current.is_some() && intent.previous.is_some() && intent.desired.is_some() {
            target.finish_update(&intent)?;
        }
        if current == intent.desired {
            record_applied(&tx, &intent, Some(path))?;
        }
        remove_intent(&tx, object)?;
        tx.commit()?;
        Ok(true)
    }

    /// Retire an Apple intent only while Swift coordinates both saved names.
    /// This keeps a newer head from being blocked by a stale pending change.
    #[cfg(target_vendor = "apple")]
    pub fn retire_superseded_apple(
        &mut self, blobs: &BlobStore, root: &Path, object: ObjectId,
    ) -> Result<bool, MaterializeError> {
        use crate::local_target::AppleExistingTarget;
        let (tx, replica) = self.begin_indexed_write()?;
        let Some((intent, Some(path))) = load_intent(&tx, object)? else { return Ok(false) };
        if matches_head(replica, &intent) { return Ok(false) }
        let mut heads = replica.heads(object);
        let Some(head) = heads.next() else { return Ok(false) };
        if heads.any(|other| other.change.entry() != head.change.entry()) {
            return Ok(false);
        }
        let Some(applied) = read_applied(&tx, object)? else { return Ok(false) };
        let (Some(previous), Some(old_path)) = (intent.previous, applied.path) else {
            return Ok(false);
        };
        if applied.contents != Some(previous) { return Ok(false) }
        if intent.desired.is_none() && old_path != path { return Ok(false) }
        for blob in [intent.previous, intent.desired].into_iter().flatten() {
            blobs.open_verified(blob)?;
        }
        let root = open_root(root)?;
        if old_path != path {
            if same_name_slot(&old_path, &path) { return Ok(false) }
            let before: Vec<_> = old_path.iter().map(String::as_str).collect();
            let after: Vec<_> = path.iter().map(String::as_str).collect();
            let mut old = NewFileTarget::existing(&root, &before)?;
            let mut new = NewFileTarget::existing(&root, &after)?;
            if old.inspect(object)? == Some(previous) && new.name_is_vacant()? {
                remove_intent(&tx, object)?;
            } else if old.name_is_vacant()? && new.inspect(object)? == intent.desired {
                record_applied(&tx, &intent, Some(path))?;
                remove_intent(&tx, object)?;
            } else {
                return Ok(false);
            }
        } else {
            let components: Vec<_> = path.iter().map(String::as_str).collect();
            let mut target = AppleExistingTarget::open(&root, &components, &intent)?;
            let restored = target.restore_original_if_staged()?;
            let current = target.inspect(object)?;
            if restored && current == Some(previous) {
                remove_intent(&tx, object)?;
            } else if current == intent.desired && current.is_some() {
                record_applied(&tx, &intent, Some(path))?;
                remove_intent(&tx, object)?;
            } else {
                return Ok(false);
            }
        }
        tx.commit()?;
        Ok(true)
    }

    #[cfg(windows)]
    pub fn recover_moved_file(
        &mut self,
        blobs: &BlobStore,
        root: &Path,
        object: ObjectId,
    ) -> Result<Recovery, MaterializeError> {
        self.recover_moved_file_with(blobs, object, |old_path, path, intent, _same_slot| {
            let root = open_root(root)?;
            let source_components: Vec<_> = old_path.iter().map(String::as_str).collect();
            let target_components: Vec<_> = path.iter().map(String::as_str).collect();
            Ok(WindowsMoveTarget {
                source: NewFileTarget::existing(&root, &source_components)?,
                target: NewFileTarget::existing(&root, &target_components)?,
                intent: intent.clone(),
            })
        })
    }

    /// Validate both recorded paths and the single signed head before the
    /// platform adapter moves the verified source to an unoccupied name.
    pub fn recover_moved_file_with<T: MoveFileTarget>(
        &mut self,
        blobs: &BlobStore,
        object: ObjectId,
        make_target: impl FnOnce(&[String], &[String], &FileIntent, bool) -> io::Result<T>,
    ) -> Result<Recovery, MaterializeError> {
        let (tx, replica) = self.begin_indexed_write()?;
        let Some((intent, Some(path))) = load_intent(&tx, object)? else {
            return Ok(Recovery::NonePending);
        };
        let (Some(previous), Some(desired)) = (intent.previous, intent.desired) else {
            return Err(MaterializeError::InvalidIntent);
        };
        let Some(applied) = read_applied(&tx, object)? else {
            return Ok(Recovery::Superseded);
        };
        let Some(old_path) = applied.path.filter(|old_path| *old_path != path) else {
            return Ok(Recovery::Superseded);
        };
        let projection = Projection::new(replica);
        if applied.contents != Some(previous)
            || !matches_head(replica, &intent)
            || !projection
                .path(object)
                .is_ok_and(|current| current.into_iter().eq(path.iter().map(String::as_str)))
        {
            return Ok(Recovery::Superseded);
        }
        let Some(old_entry) = replica
            .get(applied.event)
            .and_then(|event| event.change.entry())
        else {
            return Ok(Recovery::Superseded);
        };
        let same_slot = same_name_slot(&old_path, &path);
        if !same_slot && !projection.vacant(old_entry.parent, &old_entry.name) {
            return Ok(Recovery::Diverged);
        }
        let mut target = make_target(&old_path, &path, &intent, same_slot)?;
        let content_ready = target.already_moved(previous, desired, same_slot)?;
        let result = if content_ready {
            Recovery::AlreadyApplied
        } else if target.move_if_current(previous, same_slot)? {
            Recovery::Applied
        } else {
            return Ok(Recovery::Diverged);
        };
        if desired != previous {
            return reconcile(tx, blobs, &mut target, &intent, Some(path), true);
        }
        record_applied(&tx, &intent, Some(path))?;
        remove_intent(&tx, object)?;
        tx.commit()?;
        Ok(result)
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
        self.recover_deleted_file_with(blobs, object, |path, _intent| {
            let components: Vec<_> = path.iter().map(String::as_str).collect();
            NewFileTarget::existing(&open_root(root)?, &components)
        })
    }

    /// Verify a tombstone against the applied file and original parent before
    /// handing the exact path to a platform adapter. The adapter must keep a
    /// coordination guard across its final content check and deletion.
    pub fn recover_deleted_file_with<T: FileTarget>(
        &mut self,
        blobs: &BlobStore,
        object: ObjectId,
        make_target: impl FnOnce(&[String], &FileIntent) -> io::Result<T>,
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
        let mut target = make_target(&path, &intent)?;
        reconcile(tx, blobs, &mut target, &intent, Some(path), true)
    }
}

fn same_name_slot(before: &[String], after: &[String]) -> bool {
    use crate::names::collision_key;

    match (before.split_last(), after.split_last()) {
        (Some((old, old_parent)), Some((new, new_parent))) if old_parent == new_parent => {
            collision_key(old)
                .is_ok_and(|key| collision_key(new).is_ok_and(|new_key| new_key == key))
        }
        _ => false,
    }
}

#[cfg(windows)]
struct UpdateTarget {
    target: NewFileTarget,
    intent: FileIntent,
}

#[cfg(windows)]
impl FileTarget for UpdateTarget {
    fn inspect(&mut self, _: ObjectId) -> io::Result<Option<Blob>> {
        self.target.update_status(&self.intent)
    }

    fn replace_if_current(
        &mut self,
        intent: &FileIntent,
        replacement: Option<File>,
    ) -> io::Result<bool> {
        self.target.replace_if_current(intent, replacement)
    }
}

#[cfg(windows)]
struct WindowsMoveTarget {
    source: NewFileTarget,
    target: NewFileTarget,
    intent: FileIntent,
}

#[cfg(windows)]
impl FileTarget for WindowsMoveTarget {
    fn inspect(&mut self, object: ObjectId) -> io::Result<Option<Blob>> {
        if self.intent.desired == self.intent.previous {
            self.target.inspect(object)
        } else {
            self.target.update_status(&self.intent)
        }
    }

    fn replace_if_current(
        &mut self,
        intent: &FileIntent,
        replacement: Option<File>,
    ) -> io::Result<bool> {
        self.target.replace_if_current(intent, replacement)
    }
}

#[cfg(windows)]
impl MoveFileTarget for WindowsMoveTarget {
    fn already_moved(&mut self, previous: Blob, desired: Blob, same_slot: bool)
        -> io::Result<bool> {
        let already_renamed = if same_slot {
            self.target.is_only_spelling()? || self.source.name_is_vacant()?
        } else {
            self.source.name_is_vacant()?
        };
        if !already_renamed {
            return Ok(false);
        }
        if desired == previous {
            return Ok(self.target.inspect(self.intent.object)? == Some(previous));
        }
        let status = self.target.update_status(&self.intent)?;
        Ok(status == Some(previous) || status == Some(desired))
    }

    fn move_if_current(&mut self, previous: Blob, same_slot: bool) -> io::Result<bool> {
        self.source.move_verified(&self.target, previous, same_slot)
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

    record_applied(&tx, intent, path)?;
    remove_intent(&tx, object)?;
    tx.commit()?;
    Ok(result)
}

fn record_applied(
    tx: &rusqlite::Transaction<'_>,
    intent: &FileIntent,
    path: Option<Vec<String>>,
) -> Result<(), MaterializeError> {
    if let Some(previous) = read_applied(tx, intent.object)? {
        record_archive(
            tx,
            intent.object,
            previous,
            intent.event,
            intent.desired.map(|blob| blob.hash),
        )?;
    }
    let applied = AppliedFile {
        event: intent.event,
        contents: intent.desired,
        path,
    };
    tx.execute(
        "INSERT INTO applied_file (object, payload) VALUES (?1, ?2)
         ON CONFLICT(object) DO UPDATE SET payload = excluded.payload",
        params![intent.object.as_slice(), encode(&applied)?],
    )?;
    Ok(())
}

fn remove_intent(tx: &rusqlite::Transaction<'_>, object: ObjectId) -> Result<(), MaterializeError> {
    tx.execute(
        "DELETE FROM file_intent WHERE object = ?1",
        [object.as_slice()],
    )?;
    Ok(())
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
