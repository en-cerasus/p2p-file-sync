use std::{
    collections::BTreeSet,
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
    Cancelled,
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
    UnavailableContent,
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
    sync_cancellable(stream, store, roster, destination, &|| false)
}

pub(crate) fn sync_cancellable(
    stream: &mut (impl Read + Write),
    store: &mut Store,
    roster: &Roster,
    destination: Destination<'_>,
    cancelled: &dyn Fn() -> bool,
) -> Result<SyncReport, SyncError> {
    check_cancelled(cancelled)?;
    let events_added = fetch_events(stream, store, roster)?;
    let report = publish_heads(
        stream,
        store,
        &destination,
        SyncReport {
            events_added,
            ..Default::default()
        },
        cancelled,
    )?;
    cache_versions(
        stream,
        store,
        destination.chunks,
        destination.completed,
        false,
    )?;
    Ok(report)
}

/// Update a serving peer's cache without materializing into a local folder.
pub(crate) fn sync_serving_cache(
    stream: &mut (impl Read + Write),
    store: &mut Store,
    roster: &Roster,
    chunks: &BlobStore,
    completed: &BlobStore,
) -> Result<usize, SyncError> {
    let events_added = fetch_events(stream, store, roster)?;
    cache_versions(stream, store, chunks, completed, true)?;
    Ok(events_added)
}

fn fetch_events(
    stream: &mut (impl Read + Write),
    store: &mut Store,
    roster: &Roster,
) -> Result<usize, SyncError> {
    let mut added = 0;
    for _ in 0..MAX_PAGES_PER_PASS {
        let (events, more) = pull_page(stream, store, roster).map_err(SyncError::Wire)?;
        added += events.added;
        if !more {
            if events.deferred != 0 {
                return Err(SyncError::IncompleteEvents);
            }
            return Ok(added);
        }
    }
    Err(SyncError::IncompleteEvents)
}

fn publish_heads(
    stream: &mut (impl Read + Write),
    store: &mut Store,
    destination: &Destination<'_>,
    mut report: SyncReport,
    cancelled: &dyn Fn() -> bool,
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
                    let moving = applied.as_ref().and_then(|item| item.contents).is_some()
                        && applied
                            .as_ref()
                            .and_then(|item| item.path.as_ref())
                            .is_none_or(|previous| !previous.iter().map(String::as_str).eq(path));
                    if moving && !cfg!(windows) {
                        report.files.push((object, FileResult::Superseded));
                        continue;
                    }
                    files.push((
                        object,
                        head.id,
                        applied.and_then(|item| item.contents),
                        true,
                        moving,
                    ));
                }
                Err(PathIssue::Deleted(_)) => {
                    if let Some(previous) = applied.and_then(|item| item.contents) {
                        files.push((object, first.id, Some(previous), false, false));
                    }
                }
                Err(issue) => report.files.push((object, FileResult::BlockedPath(issue))),
            }
        }
    }

    if !directories.is_empty() {
        let root = open_root(destination.root).map_err(SyncError::Content)?;
        for components in directories {
            check_cancelled(cancelled)?;
            open_or_create_directories(&root, &components).map_err(SyncError::Content)?;
        }
    }

    for (object, event, previous, live, moving) in files {
        check_cancelled(cancelled)?;
        let (previous, moving) = if store
            .retire_superseded_file(destination.completed, destination.root, object)
            .map_err(SyncError::Materialize)?
        {
            let applied = store.applied_file(object).map_err(SyncError::Materialize)?;
            let previous = applied.as_ref().and_then(|file| file.contents);
            let moving = live
                && previous.is_some()
                && applied.and_then(|file| file.path).is_some_and(|path| {
                    Projection::new(store.replica())
                        .path(object)
                        .is_ok_and(|current| {
                            !current.into_iter().eq(path.iter().map(String::as_str))
                        })
                });
            (previous, moving)
        } else {
            (previous, moving)
        };
        #[cfg(windows)]
        if moving {
            let Some(previous) = previous else {
                report.files.push((object, FileResult::Superseded));
                continue;
            };
            let desired = match fetch_content(
                stream,
                store,
                destination.chunks,
                destination.completed,
                event,
            ) {
                Ok(blob) => blob,
                Err(SyncError::Wire(WireError::UnavailableContent)) => {
                    report.files.push((object, FileResult::UnavailableContent));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let intent = FileIntent {
                object,
                event,
                previous: Some(previous),
                desired: Some(desired),
            };
            check_cancelled(cancelled)?;
            match store.stage_file(destination.completed, &intent) {
                Ok(()) => {
                    let outcome = store
                        .recover_moved_file(destination.completed, destination.root, object)
                        .map_err(SyncError::Materialize)?;
                    report.files.push((object, update_result(outcome)));
                }
                Err(MaterializeError::AlreadyPending | MaterializeError::InvalidIntent) => {
                    report.files.push((object, FileResult::PendingRecovery));
                }
                Err(error) => return Err(SyncError::Materialize(error)),
            }
            continue;
        }
        #[cfg(not(windows))]
        debug_assert!(!moving);
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
            match fetch_content(
                stream,
                store,
                destination.chunks,
                destination.completed,
                event,
            ) {
                Ok(blob) => Some(blob),
                Err(SyncError::Wire(WireError::UnavailableContent)) => {
                    report.files.push((object, FileResult::UnavailableContent));
                    continue;
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let is_update = previous.is_some() && previous != desired;
        check_cancelled(cancelled)?;
        #[cfg(not(windows))]
        if is_update {
            // This platform has no conditional existing-file adapter yet.
            // Keep the verified content cached, but do not create an intent
            // that cannot be recovered and would block a later revision.
            report.files.push((object, FileResult::PendingRecovery));
            continue;
        }
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
            unreachable!("non-Windows transitions returned before staging");
        }
        let outcome = store
            .recover_new_file(destination.completed, destination.root, object)
            .map_err(SyncError::Materialize)?;
        report.files.push((object, file_result(outcome)));
    }
    Ok(report)
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<(), SyncError> {
    if cancelled() {
        return Err(SyncError::Cancelled);
    }
    Ok(())
}

fn cache_versions(
    stream: &mut (impl Read + Write),
    store: &Store,
    chunks: &BlobStore,
    completed: &BlobStore,
    include_live: bool,
) -> Result<(), SyncError> {
    let replica = store.replica();
    let originals = store
        .active_protections()
        .map(|protection| (protection.original(), protection.content()));
    let candidates = replica
        .objects()
        .filter(|&object| include_live || replica.has_conflict(object))
        .flat_map(|object| replica.heads(object))
        .filter_map(|event| match event.change.entry()?.kind {
            EntryKind::File(hash) => Some((event.id, hash)),
            EntryKind::Directory => None,
        });
    let mut requested = BTreeSet::new();
    for (event, hash) in originals.chain(candidates) {
        if store.signed_event(event).is_none() || !requested.insert(hash) {
            continue;
        }
        match completed.open_candidate(hash) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(SyncError::Content(error)),
        }
        match fetch_content(stream, store, chunks, completed, event) {
            Ok(_) | Err(SyncError::Wire(WireError::UnavailableContent)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn fetch_content(
    stream: &mut (impl Read + Write),
    store: &Store,
    chunks: &BlobStore,
    completed: &BlobStore,
    event: EventId,
) -> Result<crate::blob::Blob, SyncError> {
    let Some(&EntryKind::File(hash)) = store
        .replica()
        .get(event)
        .filter(|_| store.signed_event(event).is_some())
        .and_then(|event| event.change.entry())
        .map(|entry| &entry.kind)
    else {
        return Err(SyncError::Wire(WireError::UnavailableContent));
    };
    match completed.open_candidate(hash) {
        Ok(file) => {
            let length = file.metadata().map_err(SyncError::Content)?.len();
            drop(file);
            let blob = crate::blob::Blob { hash, length };
            completed.open_verified(blob).map_err(SyncError::Content)?;
            return Ok(blob);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(SyncError::Content(error)),
    }
    let manifest = pull_manifest(stream, store, event).map_err(SyncError::Wire)?;
    for index in manifest
        .missing_chunks(chunks)
        .map_err(SyncError::Content)?
    {
        pull_chunk(stream, &manifest, chunks, index).map_err(SyncError::Wire)?;
    }
    manifest
        .finish(chunks, completed)
        .map_err(SyncError::Content)?;
    Ok(manifest.file())
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
