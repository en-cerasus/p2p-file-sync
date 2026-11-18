use std::{
    io::{self, Seek},
    path::Path,
};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::OpenOptions;

use crate::{
    auth::{AuthError, PeerSigningKey, Roster},
    blob::BlobStore,
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
    Retry,
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
    let name_key = collision_key(name).map_err(PublishError::InvalidName)?;
    let peer = key.verifying_key().to_bytes();
    roster.check_writer(&peer).map_err(PublishError::Auth)?;
    if store.membership() != roster.membership() {
        return Err(PublishError::Auth(AuthError::WrongScope));
    }
    let mut input = open_selected_file(source).map_err(PublishError::Io)?;
    let length = input.metadata().map_err(PublishError::Io)?.len();
    let manifest = Manifest::build(&mut input, length).map_err(PublishError::Io)?;
    let blob = manifest.file();
    input.rewind().map_err(PublishError::Io)?;
    completed.put(blob, &mut input).map_err(PublishError::Io)?;

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
                .is_some_and(|entry| entry.kind == EntryKind::File(blob.hash))
        {
            return Ok(existing.id);
        }
        return Err(PublishError::NameConflict);
    }

    drop(occupied);
    let sequence = replica
        .clock()
        .get(&peer)
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(PublishError::SequenceExhausted)?;
    let mut object = [0; 16];
    getrandom::fill(&mut object)
        .map_err(|error| PublishError::Io(io::Error::other(error.to_string())))?;
    let event = Event {
        id: EventId { peer, sequence },
        context: replica.clock().clone(),
        object,
        change: Change::Create(Entry {
            parent,
            name: name.into(),
            kind: EntryKind::File(blob.hash),
        }),
    };
    let wire = roster.sign_event(key, &event).map_err(PublishError::Auth)?;
    match roster.receive(store, &wire).map_err(PublishError::Auth)? {
        Apply::Added | Apply::Duplicate => Ok(event.id),
        Apply::MissingDependencies(_) => Err(PublishError::Retry),
    }
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
