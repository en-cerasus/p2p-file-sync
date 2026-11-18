use std::{
    io::{self, Seek},
    path::Path,
};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File, OpenOptions};

use crate::{
    auth::{AuthError, PeerSigningKey, Roster},
    blob::{Blob, BlobStore},
    chunks::Manifest,
    local_target::open_root,
    names::{collision_key, NameError},
    projection::Projection,
    replica::{Apply, Change, Entry, EntryKind, Event, EventId, ObjectId},
    store::{Store, StoreError},
};

#[derive(Debug)]
pub enum PublishError {
    Io(io::Error),
    Store(StoreError),
    Auth(AuthError),
    InvalidName(NameError),
    InvalidParent,
    NameConflict,
    SequenceExhausted,
    StaleBase,
    MissingBaseContent,
    Retry,
    TooDeep,
}

/// Publish a regular file explicitly selected by the local user. The new
/// event is signed only after its content is safely in the completed store.
pub fn publish_file(
    source: &Path,
    store: &mut Store,
    roster: &Roster,
    key: &PeerSigningKey,
    completed: &BlobStore,
    parent: Option<ObjectId>,
    name: &str,
) -> Result<EventId, PublishError> {
    collision_key(name).map_err(PublishError::InvalidName)?;
    let peer = authorize(store, roster, key)?;
    let blob = store_file(source, completed)?;
    publish_entry(
        store,
        roster,
        key,
        peer,
        Entry {
            parent,
            name: name.into(),
            kind: EntryKind::File(blob.hash),
        },
    )
}

#[derive(Debug, Default)]
pub struct TreeSummary {
    pub files: usize,
    pub directories: usize,
    pub skipped: usize,
}

/// Publish the contents of a user-selected root. Operations are idempotent
/// per entry and a failure leaves already published entries available.
pub fn publish_tree(
    root: &Path,
    store: &mut Store,
    roster: &Roster,
    key: &PeerSigningKey,
    completed: &BlobStore,
) -> Result<TreeSummary, PublishError> {
    let peer = authorize(store, roster, key)?;
    let directory = open_root(root).map_err(PublishError::Io)?;
    let mut context = TreePublisher {
        store,
        roster,
        key,
        peer,
        completed,
        summary: TreeSummary::default(),
    };
    context.visit(&directory, None, 0)?;
    Ok(context.summary)
}

struct TreePublisher<'a> {
    store: &'a mut Store,
    roster: &'a Roster,
    key: &'a PeerSigningKey,
    peer: crate::replica::PeerId,
    completed: &'a BlobStore,
    summary: TreeSummary,
}

impl TreePublisher<'_> {
    fn visit(
        &mut self,
        directory: &Dir,
        parent: Option<ObjectId>,
        depth: usize,
    ) -> Result<(), PublishError> {
        if depth >= 128 {
            return Err(PublishError::TooDeep);
        }
        let mut children = directory
            .entries()
            .map_err(PublishError::Io)?
            .collect::<io::Result<Vec<_>>>()
            .map_err(PublishError::Io)?;
        children.sort_by_cached_key(|entry| entry.file_name());
        for child in children {
            let kind = child.file_type().map_err(PublishError::Io)?;
            if !kind.is_dir() && !kind.is_file() {
                self.summary.skipped += 1;
                continue;
            }
            let name = child
                .file_name()
                .into_string()
                .map_err(|_| PublishError::InvalidName(NameError::InvalidComponent))?;
            if crate::names::recovery_name(&name) {
                self.summary.skipped += 1;
                continue;
            }
            collision_key(&name).map_err(PublishError::InvalidName)?;
            if kind.is_dir() {
                let child = directory
                    .open_dir_nofollow(&name)
                    .map_err(PublishError::Io)?;
                let id = self.add(Entry {
                    parent,
                    name,
                    kind: EntryKind::Directory,
                })?;
                let object = self
                    .store
                    .replica()
                    .get(id)
                    .ok_or(PublishError::Retry)?
                    .object;
                self.summary.directories += 1;
                self.visit(&child, Some(object), depth + 1)?;
            } else {
                let file = open_regular(directory, name.as_ref()).map_err(PublishError::Io)?;
                let blob = store_open_file(file, self.completed)?;
                self.add(Entry {
                    parent,
                    name,
                    kind: EntryKind::File(blob.hash),
                })?;
                self.summary.files += 1;
            }
        }
        Ok(())
    }

    fn add(&mut self, entry: Entry) -> Result<EventId, PublishError> {
        publish_entry(self.store, self.roster, self.key, self.peer, entry)
    }
}

fn publish_entry(
    store: &mut Store,
    roster: &Roster,
    key: &PeerSigningKey,
    peer: crate::replica::PeerId,
    entry: Entry,
) -> Result<EventId, PublishError> {
    let parent = entry.parent;
    let name_key = collision_key(&entry.name).map_err(PublishError::InvalidName)?;

    store.refresh().map_err(PublishError::Store)?;
    let replica = store.replica();
    let projection = Projection::new(replica);
    if let Some(parent) = parent {
        if projection.path(parent).is_err()
            || !replica.heads(parent).any(|head| {
                head.change
                    .entry()
                    .is_some_and(|entry| entry.kind == EntryKind::Directory)
            })
        {
            return Err(PublishError::InvalidParent);
        }
    }
    let mut occupied = replica.objects().filter_map(|object| {
        replica.heads(object).find(|head| {
            head.change.entry().is_some_and(|entry| {
                entry.parent == parent
                    && collision_key(&entry.name).is_ok_and(|key| key == name_key)
            })
        })
    });
    if let Some(existing) = occupied.next() {
        if occupied.next().is_none()
            && !replica.has_conflict(existing.object)
            && store.signed_event(existing.id).is_some()
            && existing
                .change
                .entry()
                .is_some_and(|existing| existing.kind == entry.kind)
        {
            return Ok(existing.id);
        }
        return Err(PublishError::NameConflict);
    }

    drop(occupied);
    let mut object = [0; 16];
    getrandom::fill(&mut object)
        .map_err(|error| PublishError::Io(io::Error::other(error.to_string())))?;
    let change = Change::Create(entry);
    append_change(store, roster, key, peer, object, change)
}

/// Publish a new version of an existing regular file, or a deletion when
/// source is absent. The previous contents remain in the blob store.
pub fn revise_file(
    source: Option<&Path>,
    store: &mut Store,
    roster: &Roster,
    key: &PeerSigningKey,
    completed: &BlobStore,
    base: EventId,
) -> Result<EventId, PublishError> {
    let peer = authorize(store, roster, key)?;
    let replacement = source.map(|path| store_file(path, completed)).transpose()?;
    store.refresh().map_err(PublishError::Store)?;
    let replica = store.replica();
    let original = replica.get(base).ok_or(PublishError::StaleBase)?;
    let object = original.object;
    let mut heads = replica.heads(object);
    if heads.next().map(|event| event.id) != Some(base)
        || heads.next().is_some()
        || store.signed_event(base).is_none()
        || Projection::new(replica).path(object).is_err()
    {
        return Err(PublishError::StaleBase);
    }
    drop(heads);
    let Some(
        entry @ Entry {
            kind: EntryKind::File(old_hash),
            ..
        },
    ) = original.change.entry()
    else {
        return Err(PublishError::StaleBase);
    };
    let mut old = completed
        .open_candidate(*old_hash)
        .map_err(PublishError::Io)?;
    let metadata = old.metadata().map_err(PublishError::Io)?;
    if !metadata.is_file() {
        return Err(PublishError::MissingBaseContent);
    }
    let old_manifest = Manifest::build(&mut old, metadata.len()).map_err(PublishError::Io)?;
    if old_manifest.file().hash != *old_hash {
        return Err(PublishError::MissingBaseContent);
    }
    if replacement.is_some_and(|blob| blob.hash == *old_hash) {
        return Ok(base);
    }

    let updated = replacement.map(|blob| Entry {
        kind: EntryKind::File(blob.hash),
        ..entry.clone()
    });
    let change = Change::Revise {
        base,
        entry: updated,
    };
    append_change(store, roster, key, peer, object, change)
}

fn append_change(
    store: &mut Store,
    roster: &Roster,
    key: &PeerSigningKey,
    peer: crate::replica::PeerId,
    object: ObjectId,
    change: Change,
) -> Result<EventId, PublishError> {
    let context = store.replica().clock().clone();
    let sequence = context
        .get(&peer)
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(PublishError::SequenceExhausted)?;
    let event = Event {
        id: EventId { peer, sequence },
        context,
        object,
        change,
    };
    let wire = roster.sign_event(key, &event).map_err(PublishError::Auth)?;
    match roster.receive(store, &wire).map_err(PublishError::Auth)? {
        Apply::Added | Apply::Duplicate => Ok(event.id),
        Apply::MissingDependencies(_) => Err(PublishError::Retry),
    }
}

fn authorize(
    store: &Store,
    roster: &Roster,
    key: &PeerSigningKey,
) -> Result<crate::replica::PeerId, PublishError> {
    let peer = key.verifying_key().to_bytes();
    roster.check_writer(&peer).map_err(PublishError::Auth)?;
    if store.membership() != roster.membership() {
        return Err(PublishError::Auth(AuthError::WrongScope));
    }
    Ok(peer)
}

fn store_file(source: &Path, completed: &BlobStore) -> Result<Blob, PublishError> {
    let input = open_selected_file(source).map_err(PublishError::Io)?;
    store_open_file(input, completed)
}

fn store_open_file(mut input: File, completed: &BlobStore) -> Result<Blob, PublishError> {
    let length = input.metadata().map_err(PublishError::Io)?.len();
    let manifest = Manifest::build(&mut input, length).map_err(PublishError::Io)?;
    let blob = manifest.file();
    input.rewind().map_err(PublishError::Io)?;
    completed.put(blob, &mut input).map_err(PublishError::Io)?;
    Ok(blob)
}

fn open_selected_file(path: &Path) -> io::Result<cap_std::fs::File> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing filename"))?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory = open_root(parent)?;
    open_regular(&directory, name)
}

fn open_regular(directory: &Dir, name: &std::ffi::OsStr) -> io::Result<File> {
    if !directory.symlink_metadata(name)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }
    Ok(file)
}
