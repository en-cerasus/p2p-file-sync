use std::{
    io::{Read, Write},
    path::Path,
};

use crate::{
    auth::Roster,
    blob::BlobStore,
    materialize::{FileIntent, MaterializeError, Recovery},
    projection::{PathIssue, Projection},
    replica::{EntryKind, ObjectId},
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
    for object in store.replica().objects() {
        let mut heads = store.replica().heads(object);
        let Some(head) = heads.find(|head| {
            head.change
                .entry()
                .is_some_and(|entry| matches!(entry.kind, EntryKind::File(_)))
        }) else {
            continue;
        };
        match projection.path(object) {
            Ok(_) => files.push((object, head.id)),
            Err(issue) => report.files.push((object, FileResult::BlockedPath(issue))),
        }
    }

    for (object, event) in files {
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
        store
            .stage_file(
                destination.completed,
                &FileIntent {
                    object,
                    event,
                    previous: None,
                    desired: Some(blob),
                },
            )
            .map_err(SyncError::Materialize)?;
        let outcome = store
            .recover_new_file(destination.completed, destination.root, object)
            .map_err(SyncError::Materialize)?;
        report.files.push((object, file_result(outcome)));
    }
    Ok(report)
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
