use std::{
    io::{Read, Write},
    path::Path,
};

use crate::{
    auth::Roster,
    blob::BlobStore,
    local_target::{open_or_create_directories, open_root},
    materialize::{FileIntent, MaterializeError, Recovery},
    projection::{PathIssue, Projection},
    replica::{EntryKind, EventId, ObjectId},
    store::{Store, StoreError},
    wire::{pull_chunk, pull_manifest, pull_page, WireError},
};

const MAX_PAGES_PER_PASS: usize = 256;

pub struct Destination<'a> {
    pub root: &'a Path,
    pub chunks: &'a BlobStore,
    pub completed: &'a BlobStore,
}

#[derive(Debug)]
pub enum SyncError {
    Wire(WireError),
    IncompleteEvents,
    Materialize(MaterializeError),
    Store(StoreError),
    Content(std::io::Error),
}

#[derive(Debug, Eq, PartialEq)]
pub enum FileResult {
    Published,
    Removed,
    Updated,
    Present,
    BlockedPath(PathIssue),
    ExistingContent,
    Superseded,
    PendingRecovery,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub events_added: usize,
    pub files: Vec<(ObjectId, FileResult)>,
}

/// Fetch a peer's event history and publish verified file heads into this
/// share's destination. The supplied stream must already be authenticated
/// for this share. Existing paths with different content remain untouched.
pub fn sync_new_files(
    stream: &mut (impl Read + Write),
    store: &mut Store,
    roster: &Roster,
    destination: Destination<'_>,
) -> Result<SyncReport, SyncError> {
    let mut report = SyncReport::default();
    for _ in 0..MAX_PAGES_PER_PASS {
        let (events, more) = pull_page(stream, store, roster).map_err(SyncError::Wire)?;
        report.events_added += events.added;
        if !more {
            if events.deferred != 0 {
                return Err(SyncError::IncompleteEvents);
            }
            return publish_heads(stream, store, destination, report);
        }
    }
    Err(SyncError::IncompleteEvents)
}

fn publish_heads(
    stream: &mut (impl Read + Write),
    store: &mut Store,
    destination: Destination<'_>,
    mut report: SyncReport,
) -> Result<SyncReport, SyncError> {
    store.refresh().map_err(SyncError::Store)?;
    let projection = Projection::new(store.replica());
    let mut files = Vec::new();
    let mut directories = Vec::new();
    for object in store.replica().objects() {
        let mut heads = store.replica().heads(object);
        let Some(first) = heads.next() else { continue };
        if first
            .change
            .entry()
            .is_some_and(|entry| entry.kind == EntryKind::Directory)
        {
            if let Ok(path) = projection.path(object) {
                directories.push(path);
            }
        }
        let file_head = store.replica().heads(object).find(|head| {
            head.change
                .entry()
                .is_some_and(|entry| matches!(entry.kind, EntryKind::File(_)))
        });

        if file_head.is_some() || first.change.entry().is_none() {
            let applied = store.applied_file(object).map_err(SyncError::Materialize)?;
            match projection.path(object) {
                Ok(path) => {
                    let Some(head) = file_head else { continue };
                    if applied.as_ref().and_then(|item| item.contents).is_some()
                        && applied
                            .as_ref()
                            .and_then(|item| item.path.as_ref())
                            .is_none_or(|previous| !previous.iter().map(String::as_str).eq(path))
                    {
                        report.files.push((object, FileResult::Superseded));
                        continue;
                    }
                    files.push((
                        object,
                        head.id,
                        applied.and_then(|item| item.contents),
                        true,
                    ));
                }
                Err(PathIssue::Deleted(_)) => {
                    if let Some(previous) = applied.and_then(|item| item.contents) {
                        files.push((object, first.id, Some(previous), false));
                    }
                }
                Err(issue) => report.files.push((object, FileResult::BlockedPath(issue))),
            }
        }
    }

    if !directories.is_empty() {
        let root = open_root(destination.root).map_err(SyncError::Content)?;
        for components in directories {
            open_or_create_directories(&root, &components).map_err(SyncError::Content)?;
        }
    }

    for (object, event, previous, live) in files {
        let previous = if store
            .retire_superseded_file(destination.completed, destination.root, object)
            .map_err(SyncError::Materialize)?
        {
            store
                .applied_file(object)
                .map_err(SyncError::Materialize)?
                .and_then(|applied| applied.contents)
        } else {
            previous
        };
        if !live && previous.is_none() {
            report.files.push((object, FileResult::Removed));
            continue;
        }
        #[cfg(windows)]
        if !live {
            match store.recover_deleted_file(destination.completed, destination.root, object) {
                Ok(Recovery::NonePending) => {}
                Ok(outcome) => {
                    report.files.push((object, deletion_result(outcome)));
                    continue;
                }
                Err(MaterializeError::InvalidIntent) => {
                    report.files.push((object, FileResult::PendingRecovery));
                    continue;
                }
                Err(error) => return Err(SyncError::Materialize(error)),
            }
        }
        #[cfg(windows)]
        if live && previous.is_some() {
            match store.recover_updated_file(destination.completed, destination.root, object) {
                Ok(Recovery::NonePending) | Err(MaterializeError::InvalidIntent) => {}
                Ok(outcome) => {
                    report.files.push((object, update_result(outcome)));
                    continue;
                }
                Err(error) => return Err(SyncError::Materialize(error)),
            }
        }
        match store.recover_new_file(destination.completed, destination.root, object) {
            Ok(Recovery::NonePending) => {}
            Ok(outcome) => {
                report.files.push((object, file_result(outcome)));
                continue;
            }
            Err(MaterializeError::InvalidIntent) => {
                report.files.push((object, FileResult::PendingRecovery));
                continue;
            }
            Err(error) => return Err(SyncError::Materialize(error)),
        }

        let desired = if live {
            Some(fetch_content(stream, store, &destination, event)?)
        } else {
            None
        };
        let is_update = previous.is_some() && previous != desired;
        store
            .stage_file(
                destination.completed,
                &FileIntent {
                    object,
                    event,
                    previous: if is_update { previous } else { None },
                    desired,
                },
            )
            .map_err(SyncError::Materialize)?;

        if is_update {
            #[cfg(windows)]
            if !live {
                let outcome = store
                    .recover_deleted_file(destination.completed, destination.root, object)
                    .map_err(SyncError::Materialize)?;
                report.files.push((object, deletion_result(outcome)));
                continue;
            }
            #[cfg(windows)]
            if live {
                let outcome = store
                    .recover_updated_file(destination.completed, destination.root, object)
                    .map_err(SyncError::Materialize)?;
                report.files.push((object, update_result(outcome)));
                continue;
            }
            report.files.push((object, FileResult::PendingRecovery));
            continue;
        }
        let outcome = store
            .recover_new_file(destination.completed, destination.root, object)
            .map_err(SyncError::Materialize)?;
        report.files.push((object, file_result(outcome)));
    }
    Ok(report)
}

fn fetch_content(
    stream: &mut (impl Read + Write),
    store: &Store,
    destination: &Destination<'_>,
    event: EventId,
) -> Result<crate::blob::Blob, SyncError> {
    let manifest = pull_manifest(stream, store, event).map_err(SyncError::Wire)?;
    let blob = manifest.file();
    match destination.completed.open_verified(blob) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            for index in manifest
                .missing_chunks(destination.chunks)
                .map_err(SyncError::Content)?
            {
                pull_chunk(stream, &manifest, destination.chunks, index)
                    .map_err(SyncError::Wire)?;
            }
            manifest
                .finish(destination.chunks, destination.completed)
                .map_err(SyncError::Content)?;
        }
        Err(error) => return Err(SyncError::Content(error)),
    }
    Ok(blob)
}

#[cfg(windows)]
fn update_result(outcome: Recovery) -> FileResult {
    match outcome {
        Recovery::Applied => FileResult::Updated,
        other => file_result(other),
    }
}

#[cfg(windows)]
fn deletion_result(outcome: Recovery) -> FileResult {
    match outcome {
        Recovery::Applied | Recovery::AlreadyApplied => FileResult::Removed,
        other => file_result(other),
    }
}

fn file_result(outcome: Recovery) -> FileResult {
    match outcome {
        Recovery::Applied => FileResult::Published,
        Recovery::AlreadyApplied => FileResult::Present,
        Recovery::Diverged => FileResult::ExistingContent,
        Recovery::Superseded => FileResult::Superseded,
        Recovery::NonePending => FileResult::PendingRecovery,
    }
}
