//! Preserve verified old-generation edits outside the active sync state.
//! Nothing here admits old events into a successor generation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io,
    path::Path,
};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::{
    blob::{Blob, BlobStore},
    host::{load_verified_generation_bundle, HostError},
    projection::Projection,
    replica::{Change, ContentHash, EntryKind, EventId},
    store::{Store, StoreError},
};

#[derive(Debug, Serialize)]
pub struct IsolatedEventSummary {
    pub peer: String,
    pub sequence: u64,
    pub object: String,
    pub path_hint: Option<String>,
    pub change: &'static str,
    pub operation: &'static str,
    pub content_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IsolatedArchiveReport {
    pub share: String,
    pub previous_generation: u64,
    pub next_generation: u64,
    pub events: Vec<IsolatedEventSummary>,
    pub copied_blobs: usize,
    pub missing_blobs: Vec<String>,
}

/// Snapshot the old database, verify its signed history against the trusted
/// transition checkpoint, and export every event beyond that checkpoint.
/// An incomplete archive still records missing blobs instead of inventing data.
pub fn archive_isolated_generation(
    previous_configuration: &Path,
    old_state_root: &Path,
    next_configuration: &Path,
    administrator_public_key: &Path,
    bundle: &Path,
    output: &Path,
) -> Result<IsolatedArchiveReport, HostError> {
    let (checkpoint, roster, _) = load_verified_generation_bundle(
        previous_configuration,
        next_configuration,
        administrator_public_key,
        bundle,
    )?;
    let share = hex(&roster.membership().share);
    let generation = roster.membership().generation;
    let share_root = old_state_root.join(&share);
    let old_state = share_root.join(generation.to_string());
    let database = old_state.join("sync.db");
    let witness = share_root
        .join("witness")
        .join(generation.to_string())
        .join("sender");
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let destination = fs::canonicalize(parent)
        .map_err(HostError::Io)?
        .join(output.file_name().ok_or(HostError::Configuration)?);
    if destination.starts_with(fs::canonicalize(old_state_root).map_err(HostError::Io)?) {
        return Err(HostError::Configuration);
    }
    if !database.is_file() || !witness.is_dir() {
        return Err(HostError::Configuration);
    }
    let state_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(old_state.join("write.lock"))
        .map_err(HostError::Io)?;
    state_lock.lock().map_err(HostError::Io)?;
    fs::create_dir(&destination).map_err(HostError::Io)?;

    // VACUUM INTO reads a transactionally consistent SQLite snapshot while
    // the original connection remains read-only, including on a live peer.
    let snapshot = destination.join("old-sync.db");
    let source = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| HostError::Store(StoreError::Sql(error)))?;
    source
        .execute("VACUUM INTO ?1", [snapshot.to_string_lossy().as_ref()])
        .map_err(|error| HostError::Store(StoreError::Sql(error)))?;
    drop(source);

    let inspection = tempfile::tempdir_in(&destination).map_err(HostError::Io)?;
    let inspection_db = inspection.path().join("sync.db");
    fs::copy(&snapshot, &inspection_db).map_err(HostError::Io)?;
    let inspection_witness = inspection.path().join("witness");
    fs::create_dir(&inspection_witness).map_err(HostError::Io)?;
    for entry in fs::read_dir(&witness).map_err(HostError::Io)? {
        let entry = entry.map_err(HostError::Io)?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "witness")
        {
            if !entry.file_type().map_err(HostError::Io)?.is_file() {
                return Err(HostError::Configuration);
            }
            fs::copy(entry.path(), inspection_witness.join(entry.file_name()))
                .map_err(HostError::Io)?;
        }
    }
    let mut store =
        Store::open(&inspection_db, roster.membership().clone()).map_err(HostError::Store)?;
    store
        .enable_event_witness(&inspection_witness)
        .map_err(HostError::Store)?;
    let checkpoint_events: BTreeMap<_, _> = checkpoint
        .events
        .iter()
        .map(|event| (event.id, event))
        .collect();
    let mut isolated_ids = Vec::<EventId>::new();
    let mut signed = Vec::new();
    for &id in store.event_order() {
        let event = store
            .replica()
            .get(id)
            .ok_or(HostError::Store(StoreError::InvalidRecord))?;
        let wire = store
            .signed_event(id)
            .ok_or(HostError::Store(StoreError::InvalidRecord))?;
        if roster.verify_event(wire).ok().as_ref() != Some(event) {
            return Err(HostError::Store(StoreError::InvalidRecord));
        }
        if id.sequence <= checkpoint.frontier.get(&id.peer).copied().unwrap_or(0) {
            if checkpoint_events.get(&id) != Some(&event) {
                return Err(HostError::Store(StoreError::InvalidRecord));
            }
        } else {
            isolated_ids.push(id);
            signed.push(wire.to_vec());
        }
    }

    let projection = Projection::new(store.replica());
    let mut events = Vec::new();
    let mut hashes = BTreeSet::<ContentHash>::new();
    for id in isolated_ids {
        let event = store
            .replica()
            .get(id)
            .ok_or(HostError::Store(StoreError::InvalidRecord))?;
        let entry = event.change.entry();
        let content = entry.and_then(|entry| match entry.kind {
            EntryKind::File(hash) => Some(hash),
            EntryKind::Directory => None,
        });
        if let Some(hash) = content {
            hashes.insert(hash);
        }
        let path_hint = projection
            .path(event.object)
            .ok()
            .map(|parts| parts.join("/"))
            .or_else(|| entry.map(|entry| entry.name.clone()))
            .or_else(|| match &event.change {
                Change::Revise { base, .. } => store
                    .replica()
                    .get(*base)
                    .and_then(|base| base.change.entry())
                    .map(|entry| entry.name.clone()),
                _ => None,
            });
        events.push(IsolatedEventSummary {
            peer: hex(&id.peer),
            sequence: id.sequence,
            object: hex(&event.object),
            path_hint,
            change: match &event.change {
                Change::Create(_) => "create",
                Change::Revise { .. } => "revise",
                Change::Resolve { .. } => "resolve",
            },
            operation: if entry.is_none() {
                "delete"
            } else if content.is_some() {
                "file"
            } else {
                "directory"
            },
            content_sha256: content.map(|hash| hex(&hash)),
        });
    }
    let source_blobs = BlobStore::open_existing(old_state.join("files")).map_err(HostError::Io)?;
    let destination_blobs = BlobStore::open(destination.join("files")).map_err(HostError::Io)?;
    let mut copied_blobs = 0;
    let mut missing_blobs = Vec::new();
    for hash in hashes {
        let file = match source_blobs.open_candidate(hash) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing_blobs.push(hex(&hash));
                continue;
            }
            Err(error) => return Err(HostError::Io(error)),
        };
        let metadata = file.metadata().map_err(HostError::Io)?;
        if !metadata.is_file() {
            return Err(HostError::Configuration);
        }
        let blob = Blob {
            hash,
            length: metadata.len(),
        };
        drop(file);
        let verified = source_blobs.open_verified(blob).map_err(HostError::Io)?;
        destination_blobs
            .put(blob, verified)
            .map_err(HostError::Io)?;
        copied_blobs += 1;
    }
    let report = IsolatedArchiveReport {
        share,
        previous_generation: generation,
        next_generation: generation + 1,
        events,
        copied_blobs,
        missing_blobs,
    };
    let event_bytes = postcard::to_allocvec(&signed)
        .map_err(|error| HostError::Store(StoreError::Decode(error)))?;
    write_new(&destination.join("events.bin"), &event_bytes)?;
    let report_bytes = serde_json::to_vec_pretty(&report).map_err(|_| HostError::Configuration)?;
    write_new(&destination.join("report.json"), &report_bytes)?;
    Ok(report)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), HostError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(HostError::Io)?;
    io::Write::write_all(&mut file, bytes).map_err(HostError::Io)?;
    file.sync_all().map_err(HostError::Io)
}

fn hex<const N: usize>(bytes: &[u8; N]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
