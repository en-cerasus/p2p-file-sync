pub mod archive;
pub mod auth;
pub mod blob;
pub mod catchup;
pub mod chunks;
pub mod direct;
pub mod generation;
pub mod host;
mod local_target;
pub mod materialize;
pub mod names;
pub mod projection;
pub mod publish;
pub mod replica;
pub mod retention;
pub mod store;
pub mod sync;
pub mod tls;
pub mod wire;

use sha2::{Digest, Sha256};

const ABI_VERSION: u32 = 1;
const SHA256_SIZE: usize = 32;
const OK: i32 = 0;
const INVALID_ARGUMENT: i32 = 1;

pub fn sha256(data: &[u8]) -> [u8; SHA256_SIZE] {
    Sha256::digest(data).into()
}

#[no_mangle]
pub extern "C" fn p2p_sync_abi_version() -> u32 {
    ABI_VERSION
}

/// Computes SHA-256 synchronously and writes exactly 32 bytes.
///
/// # Safety
///
/// For nonzero input_len, input must be readable for input_len bytes.
/// output must be writable for 32 bytes. The caller must keep the memory
/// valid for the duration of the call and prevent concurrent mutation.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_sha256(
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    output_len: usize,
) -> i32 {
    if output.is_null()
        || output_len < SHA256_SIZE
        || input_len > isize::MAX as usize
        || (input.is_null() && input_len != 0)
    {
        return INVALID_ARGUMENT;
    }

    let digest = if input_len == 0 {
        sha256(&[])
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(input, input_len) };
        sha256(bytes)
    };

    unsafe { std::ptr::copy_nonoverlapping(digest.as_ptr(), output, SHA256_SIZE) };
    OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_matches_sha256_reference_vector() {
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        let bytes = b"abc";
        let mut output = [0; SHA256_SIZE];
        let result = unsafe {
            p2p_sync_sha256(
                bytes.as_ptr(),
                bytes.len(),
                output.as_mut_ptr(),
                output.len(),
            )
        };
        assert_eq!(result, OK);
        assert_eq!(output, expected);
    }

    #[test]
    fn ffi_handles_empty_input_and_rejects_invalid_buffers() {
        let mut output = [0; SHA256_SIZE];
        let result =
            unsafe { p2p_sync_sha256(std::ptr::null(), 0, output.as_mut_ptr(), output.len()) };
        assert_eq!(result, OK);
        assert_eq!(output, sha256(&[]));

        let unchanged = output;
        let invalid =
            unsafe { p2p_sync_sha256(std::ptr::null(), 1, output.as_mut_ptr(), output.len()) };
        let too_short =
            unsafe { p2p_sync_sha256(std::ptr::null(), 0, output.as_mut_ptr(), output.len() - 1) };
        assert_eq!(invalid, INVALID_ARGUMENT);
        assert_eq!(too_short, INVALID_ARGUMENT);
        assert_eq!(output, unchanged);
    }
}

/// Pull once using an explicitly selected local configuration.
///
/// # Safety
/// Each pointer/length pair refers to readable UTF-8 bytes for this call.
/// The output is aligned, writable, and does not overlap any input.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_pull(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    destination: *const u8,
    destination_len: usize,
    output: *mut host::PullSummary,
) -> i32 {
    unsafe {
        pull_to_output(
            [
                (config, config_len),
                (state, state_len),
                (destination, destination_len),
            ],
            output,
            None,
            std::ptr::null_mut(),
            None,
            false,
        )
    }
}

/// Pull using the extended result layout.
///
/// # Safety
/// Pointer and buffer requirements are identical to p2p_sync_pull.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_pull_v2(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    destination: *const u8,
    destination_len: usize,
    output: *mut host::PullSummaryV2,
) -> i32 {
    unsafe {
        pull_to_output(
            [
                (config, config_len),
                (state, state_len),
                (destination, destination_len),
            ],
            output,
            None,
            std::ptr::null_mut(),
            None,
            false,
        )
    }
}

/// Pull and report disk usage for protected contents.
///
/// # Safety
/// Pointer and buffer requirements are identical to p2p_sync_pull.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_pull_v3(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    destination: *const u8,
    destination_len: usize,
    output: *mut host::PullSummaryV3,
) -> i32 {
    unsafe {
        pull_to_output(
            [
                (config, config_len),
                (state, state_len),
                (destination, destination_len),
            ],
            output,
            None,
            std::ptr::null_mut(),
            None,
            false,
        )
    }
}

type OriginalsCallback = extern "C" fn(*mut std::ffi::c_void, *const u8, usize);

/// Pull and optionally deliver the protected-original list as UTF-8 JSON.
///
/// # Safety
/// Paths and output follow p2p_sync_pull. The callback must not unwind or
/// retain the borrowed JSON buffer. Its context is passed through unchanged.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_pull_v4(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    destination: *const u8,
    destination_len: usize,
    output: *mut host::PullSummaryV3,
    originals: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
) -> i32 {
    unsafe {
        pull_to_output(
            [
                (config, config_len),
                (state, state_len),
                (destination, destination_len),
            ],
            output,
            originals,
            context,
            None,
            false,
        )
    }
}

type CancellationCallback = extern "C" fn(*mut std::ffi::c_void) -> u8;

/// Pull with optional cancellation. Return 7 when cancellation is observed.
///
/// # Safety
/// Paths and output follow p2p_sync_pull. The callback must not unwind or
/// retain the borrowed JSON buffer. Both callbacks use the supplied context.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_pull_v5(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    destination: *const u8,
    destination_len: usize,
    output: *mut host::PullSummaryV3,
    originals: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
    cancellation: Option<CancellationCallback>,
) -> i32 {
    unsafe {
        pull_to_output(
            [
                (config, config_len),
                (state, state_len),
                (destination, destination_len),
            ],
            output,
            originals,
            context,
            cancellation,
            false,
        )
    }
}

/// Pull and report JSON containing originals and unresolved conflict versions.
///
/// # Safety
/// Paths and output follow p2p_sync_pull. The callback must not unwind or
/// retain the borrowed JSON buffer. Both callbacks use the supplied context.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_pull_v6(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    destination: *const u8,
    destination_len: usize,
    output: *mut host::PullSummaryV3,
    originals: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
    cancellation: Option<CancellationCallback>,
) -> i32 {
    unsafe {
        pull_to_output(
            [
                (config, config_len),
                (state, state_len),
                (destination, destination_len),
            ],
            output,
            originals,
            context,
            cancellation,
            true,
        )
    }
}

/// Read a writer's current archive choices as a JSON array. Copy callback
/// bytes before returning; this call must run off the UI thread.
///
/// # Safety
/// Both pointer/length pairs must be readable UTF-8 paths for this call.
/// The callback must not unwind or retain the borrowed buffer.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_history(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    callback: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
) -> i32 {
    let Some(callback) = callback else {
        return INVALID_ARGUMENT;
    };
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) })
    else {
        return INVALID_ARGUMENT;
    };
    let result = std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        serde_json::to_vec(&writer.history().map_err(host_error_code)?).map_err(|_| 6)
    });
    match result {
        Ok(Ok(bytes)) => {
            callback(context, bytes.as_ptr(), bytes.len());
            OK
        }
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

/// Read protected common originals from a writer's serving state.
///
/// # Safety
/// Paths and callback follow p2p_sync_writer_history.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_protected_originals(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    callback: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
) -> i32 {
    let Some(callback) = callback else { return INVALID_ARGUMENT };
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) })
    else {
        return INVALID_ARGUMENT;
    };
    let result = std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        serde_json::to_vec(&writer.protected_originals_report().map_err(host_error_code)?)
            .map_err(|_| 6)
    });
    match result {
        Ok(Ok(bytes)) => {
            callback(context, bytes.as_ptr(), bytes.len());
            OK
        }
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

/// Restore one reviewed archive as a new signed writer revision. The chosen
/// archive and current event IDs use `<64-hex-peer>:<sequence>`.
///
/// # Safety
/// Every pointer/length pair must remain readable for the call.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_restore(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    archive: *const u8,
    archive_len: usize,
    reviewed: *const u8,
    reviewed_len: usize,
) -> i32 {
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) })
    else {
        return INVALID_ARGUMENT;
    };
    let (Some(archive), Some(reviewed)) = (
        unsafe { borrowed_text(archive, archive_len, 128) },
        unsafe { borrowed_text(reviewed, reviewed_len, 128) },
    ) else {
        return INVALID_ARGUMENT;
    };
    let (Some(archive), Some(reviewed)) = (parse_event_id(archive), parse_event_id(reviewed))
    else {
        return INVALID_ARGUMENT;
    };
    match std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        writer
            .restore_archive(archive, reviewed)
            .map_err(host_error_code)
    }) {
        Ok(Ok(_)) => OK,
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

#[derive(serde::Deserialize)]
struct ArchiveDeleteRequest {
    id: i64,
    archive: String,
    replaced_by: String,
    archived_at: i64,
}

/// Delete selected writer-history entries without removing any body still
/// referenced by a protected original, active version or recovery operation.
/// `selection` is a JSON array of the listing's id, archive, replaced_by and
/// archived_at fields. A changed or expired entry rejects the whole request.
///
/// # Safety
/// Every pointer/length pair must remain readable for the call.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_delete_history(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    selection: *const u8,
    selection_len: usize,
) -> i32 {
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) })
    else {
        return INVALID_ARGUMENT;
    };
    let Some(selection) = (unsafe { borrowed_text(selection, selection_len, 65_536) }) else {
        return INVALID_ARGUMENT;
    };
    let Ok(requests) = serde_json::from_str::<Vec<ArchiveDeleteRequest>>(selection) else {
        return INVALID_ARGUMENT;
    };
    if requests.is_empty() || requests.len() > 256 {
        return INVALID_ARGUMENT;
    }
    let Some(selected) = requests
        .into_iter()
        .map(|request| {
            Some(archive::ArchiveSelection {
                id: request.id,
                event: parse_event_id(&request.archive)?,
                replaced_by: parse_event_id(&request.replaced_by)?,
                archived_at: request.archived_at,
            })
        })
        .collect::<Option<Vec<_>>>()
    else {
        return INVALID_ARGUMENT;
    };
    match std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        writer
            .delete_history(&selected)
            .map_err(|error| match error {
                host::HostError::Store(store::StoreError::InvalidRecord) => 5,
                other => host_error_code(other),
            })
    }) {
        Ok(Ok(_)) => OK,
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

/// List the writer's unresolved conflicts as groups of candidate revisions.
/// The callback receives a JSON array with versions (id, name, kind).
///
/// # Safety
/// Paths and callback follow p2p_sync_writer_history.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_conflicts(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    callback: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
) -> i32 {
    let Some(callback) = callback else {
        return INVALID_ARGUMENT;
    };
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) })
    else {
        return INVALID_ARGUMENT;
    };
    let result = std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        let groups: Vec<_> = writer
            .conflicts()
            .map_err(host_error_code)?
            .into_iter()
            .map(|heads| {
                let versions: Vec<_> = heads
                    .into_iter()
                    .map(|event| {
                        let (name, kind) = match event.change.entry() {
                            None => (None, "deleted"),
                            Some(entry) => (
                                Some(entry.name.as_str()),
                                match entry.kind {
                                    replica::EntryKind::File(_) => "file",
                                    replica::EntryKind::Directory => "directory",
                                },
                            ),
                        };
                        serde_json::json!({
                            "id": format_event_id(event.id),
                            "name": name,
                            "kind": kind,
                        })
                    })
                    .collect();
                serde_json::json!({ "versions": versions })
            })
            .collect();
        serde_json::to_vec(&groups).map_err(|_| 6)
    });
    match result {
        Ok(Ok(bytes)) => {
            callback(context, bytes.as_ptr(), bytes.len());
            OK
        }
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

/// Serve authenticated peers until the cancellation callback returns nonzero.
/// The ready callback receives the bound address after the listener starts.
/// A failed peer session does not stop the listener. Returns 7 on a normal
/// requested stop, matching the pull cancellation status.
///
/// # Safety
/// Path pointers must remain readable. Callbacks must be thread-safe for this
/// call, must not unwind or retain the borrowed ready buffer, and the context
/// must outlive the call.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_serve(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    ready: Option<OriginalsCallback>,
    cancellation: Option<CancellationCallback>,
    context: *mut std::ffi::c_void,
) -> i32 {
    let (Some(ready), Some(cancelled)) = (ready, cancellation) else {
        return INVALID_ARGUMENT;
    };
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) })
    else {
        return INVALID_ARGUMENT;
    };
    match std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        let listener = writer.bind().map_err(host_error_code)?;
        listener.set_nonblocking(true).map_err(|_| 3)?;
        let address = listener.local_addr().map_err(|_| 3)?.to_string();
        ready(context, address.as_ptr(), address.len());
        loop {
            if cancelled(context) != 0 {
                return Ok(7);
            }
            match listener.accept() {
                Ok((socket, _)) => {
                    // Invalid or disconnected peers must not stop serving.
                    // Windows may inherit the listener's nonblocking mode
                    // onto accepted sockets; TLS expects a blocking stream.
                    if socket.set_nonblocking(false).is_ok() {
                        let _ = writer.serve(socket);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(_) => return Err(3),
            }
        }
    }) {
        Ok(Ok(status)) => status,
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

/// Scan a selected source folder, publishing new files and changed contents.
/// Missing source files are not interpreted as deletions. The callback gets
/// a JSON object with files, directories and skipped counts.
///
/// # Safety
/// Paths and callback follow p2p_sync_writer_history.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_refresh_tree(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    source: *const u8,
    source_len: usize,
    callback: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
) -> i32 {
    let Some(callback) = callback else {
        return INVALID_ARGUMENT;
    };
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) })
    else {
        return INVALID_ARGUMENT;
    };
    let Some(source) = (unsafe { borrowed_text(source, source_len, 32_768) }) else {
        return INVALID_ARGUMENT;
    };
    let result = std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        let root = writer
            .validate_source_root(std::path::Path::new(source))
            .map_err(host_error_code)?;
        let summary = writer.refresh_tree(&root).map_err(host_error_code)?;
        serde_json::to_vec(&serde_json::json!({
            "files": summary.files,
            "directories": summary.directories,
            "skipped": summary.skipped,
        }))
        .map_err(|_| 6)
    });
    match result {
        Ok(Ok(bytes)) => {
            callback(context, bytes.as_ptr(), bytes.len());
            OK
        }
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

/// List reviewed deletion candidates. The callback receives a JSON array of
/// {path: [components], head: "<peer>:<sequence>"}. Only files whose parent
/// can be opened and whose source name is vacant are included.
///
/// # Safety
/// Paths and callback follow p2p_sync_writer_refresh_tree.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_missing_files(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    source: *const u8,
    source_len: usize,
    callback: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
) -> i32 {
    let Some(callback) = callback else { return INVALID_ARGUMENT };
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) }) else {
        return INVALID_ARGUMENT;
    };
    let Some(source) = (unsafe { borrowed_text(source, source_len, 32_768) }) else {
        return INVALID_ARGUMENT;
    };
    let result = std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        let files: Vec<_> = writer.missing_source_files(std::path::Path::new(source))
            .map_err(host_error_code)?
            .into_iter()
            .map(|(path, head)| serde_json::json!({"path": path, "head": format_event_id(head)}))
            .collect();
        serde_json::to_vec(&files).map_err(|_| 6)
    });
    match result {
        Ok(Ok(bytes)) => {
            callback(context, bytes.as_ptr(), bytes.len());
            OK
        }
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

/// Publish a deletion only if the reviewed head is still current and the
/// source name is still absent. Does not delete any local source file.
///
/// # Safety
/// Every pointer/length pair must remain readable for the call.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_delete_missing_file(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    source: *const u8,
    source_len: usize,
    shared_path: *const u8,
    shared_path_len: usize,
    reviewed: *const u8,
    reviewed_len: usize,
) -> i32 {
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) }) else {
        return INVALID_ARGUMENT;
    };
    let (Some(source), Some(shared_path), Some(reviewed)) = (
        unsafe { borrowed_text(source, source_len, 32_768) },
        unsafe { borrowed_text(shared_path, shared_path_len, 32_768) },
        unsafe { borrowed_text(reviewed, reviewed_len, 128) },
    ) else {
        return INVALID_ARGUMENT;
    };
    let Some(reviewed) = parse_event_id(reviewed) else { return INVALID_ARGUMENT };
    match std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        writer.delete_missing_source_file(std::path::Path::new(source), shared_path, reviewed)
            .map_err(host_error_code)
    }) {
        Ok(Ok(_)) => OK,
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

/// Fetch signed peer events and verified contents into this writer's cache.
///
/// # Safety
/// Path pointers must remain readable for the call. `events_added` must
/// point to writable storage for one u64.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_catch_up(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    peers: *const u8,
    peers_len: usize,
    events_added: *mut u64,
) -> i32 {
    unsafe {
        writer_catch_up(
            config, config_len, state, state_len, peers, peers_len,
            events_added, None, std::ptr::null_mut(),
        )
    }
}

/// Cancellable writer catch-up. Returns 7 on a requested stop.
///
/// # Safety
/// Path pointers must remain readable, `events_added` must be writable, and
/// the callback must not unwind. `context` must outlive this call.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_catch_up_v2(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    peers: *const u8,
    peers_len: usize,
    events_added: *mut u64,
    cancellation: Option<CancellationCallback>,
    context: *mut std::ffi::c_void,
) -> i32 {
    if cancellation.is_none() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        writer_catch_up(
            config, config_len, state, state_len, peers, peers_len,
            events_added, cancellation, context,
        )
    }
}

unsafe fn writer_catch_up(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    peers: *const u8,
    peers_len: usize,
    events_added: *mut u64,
    cancellation: Option<CancellationCallback>,
    context: *mut std::ffi::c_void,
) -> i32 {
    if events_added.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe { *events_added = 0 };
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) })
    else {
        return INVALID_ARGUMENT;
    };
    let Some(peers) = (unsafe { borrowed_text(peers, peers_len, 32_768) }) else {
        return INVALID_ARGUMENT;
    };
    match std::panic::catch_unwind(|| {
        let cancelled = || cancellation.is_some_and(|check| check(context) != 0);
        if cancelled() {
            return Err(7);
        }
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        writer
            .catch_up_cancellable(std::path::Path::new(peers), &cancelled)
            .map_err(host_error_code)
    }) {
        Ok(Ok(count)) => {
            unsafe { *events_added = count as u64 };
            OK
        }
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

/// Resolve a conflict by choosing one reviewed head. `reviewed` is a JSON
/// array of all candidate IDs shown to the user; a changed set is rejected.
///
/// # Safety
/// Every pointer/length pair must remain readable for the call.
#[no_mangle]
pub unsafe extern "C" fn p2p_sync_writer_resolve(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
    choice: *const u8,
    choice_len: usize,
    reviewed: *const u8,
    reviewed_len: usize,
) -> i32 {
    let Some((config, state)) = (unsafe { writer_paths(config, config_len, state, state_len) })
    else {
        return INVALID_ARGUMENT;
    };
    let (Some(choice), Some(reviewed)) =
        (unsafe { borrowed_text(choice, choice_len, 128) }, unsafe {
            borrowed_text(reviewed, reviewed_len, 65_536)
        })
    else {
        return INVALID_ARGUMENT;
    };
    let Some(choice) = parse_event_id(choice) else {
        return INVALID_ARGUMENT;
    };
    let Ok(reviewed) = serde_json::from_str::<Vec<String>>(reviewed) else {
        return INVALID_ARGUMENT;
    };
    if reviewed.len() < 2 || reviewed.len() > 256 {
        return INVALID_ARGUMENT;
    }
    let Some(ids) = reviewed
        .iter()
        .map(|id| parse_event_id(id))
        .collect::<Option<std::collections::BTreeSet<_>>>()
    else {
        return INVALID_ARGUMENT;
    };
    if ids.len() != reviewed.len() || !ids.contains(&choice) {
        return INVALID_ARGUMENT;
    }
    match std::panic::catch_unwind(|| {
        let mut writer = host::Sender::open(config, state).map_err(host_error_code)?;
        writer
            .resolve_conflict(choice, ids)
            .map_err(host_error_code)
    }) {
        Ok(Ok(_)) => OK,
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}

fn format_event_id(id: replica::EventId) -> String {
    let peer: String = id.peer.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{peer}:{}", id.sequence)
}

unsafe fn borrowed_text<'a>(pointer: *const u8, length: usize, maximum: usize) -> Option<&'a str> {
    if pointer.is_null() || length == 0 || length > maximum {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length) };
    std::str::from_utf8(bytes)
        .ok()
        .filter(|text| !text.contains('\0'))
}

unsafe fn writer_paths<'a>(
    config: *const u8,
    config_len: usize,
    state: *const u8,
    state_len: usize,
) -> Option<(&'a std::path::Path, &'a std::path::Path)> {
    Some((
        std::path::Path::new(unsafe { borrowed_text(config, config_len, 32_768) }?),
        std::path::Path::new(unsafe { borrowed_text(state, state_len, 32_768) }?),
    ))
}

fn parse_event_id(value: &str) -> Option<replica::EventId> {
    let (peer, sequence) = value.split_once(':')?;
    if peer.len() != 64 || !peer.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut decoded = [0; 32];
    for (index, byte) in decoded.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&peer[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(replica::EventId {
        peer: decoded,
        sequence: sequence.parse().ok().filter(|&n| n != 0)?,
    })
}

fn host_error_code(error: host::HostError) -> i32 {
    match error {
        host::HostError::Configuration | host::HostError::InactiveGeneration => 2,
        host::HostError::Io(_) | host::HostError::Store(_) => 3,
        host::HostError::Tls(_) => 4,
        host::HostError::Direct(direct::DirectError::Cancelled) => 7,
        _ => 5,
    }
}

unsafe fn pull_to_output<T: From<host::PullSummaryV3>>(
    paths: [(*const u8, usize); 3],
    output: *mut T,
    originals: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
    cancellation: Option<CancellationCallback>,
    detailed: bool,
) -> i32 {
    if output.is_null() {
        return INVALID_ARGUMENT;
    }

    if paths
        .iter()
        .any(|(ptr, len)| ptr.is_null() || *len == 0 || *len > 32_768)
    {
        return INVALID_ARGUMENT;
    }
    let run = std::panic::catch_unwind(|| {
        let [config, state, destination] = paths.map(|(ptr, len)| {
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
            std::str::from_utf8(bytes)
                .ok()
                .filter(|path| !path.contains('\0'))
                .map(std::path::Path::new)
        });
        let (Some(config), Some(state), Some(destination)) = (config, state, destination) else {
            return Err(INVALID_ARGUMENT);
        };
        let (summary, entries) = host::pull_with_originals(config, state, destination, &|| {
            cancellation.is_some_and(|check| check(context) != 0)
        })
        .map_err(|error| match error {
            host::HostError::Configuration | host::HostError::InactiveGeneration => 2,
            host::HostError::Io(_) | host::HostError::Store(_) => 3,
            host::HostError::Tls(_) => 4,
            host::HostError::Direct(direct::DirectError::Cancelled) => 7,
            host::HostError::Direct(_)
            | host::HostError::Authentication(_)
            | host::HostError::Publish(_)
            | host::HostError::Transition(_)
            | host::HostError::CheckpointData(_)
            | host::HostError::Baseline(_)
            | host::HostError::CheckpointTooLarge => 5,
        })?;
        let json = originals
            .map(|_| {
                if detailed {
                    serde_json::to_vec(&entries)
                } else {
                    serde_json::to_vec(&entries.originals)
                }
            })
            .transpose()
            .map_err(|_| 6)?;
        Ok((summary, json))
    });
    match run {
        Ok(Ok((summary, json))) => {
            if let (Some(callback), Some(json)) = (originals, json) {
                callback(context, json.as_ptr(), json.len());
            }
            unsafe { output.write(summary.into()) };
            OK
        }
        Ok(Err(code)) => code,
        Err(_) => 6,
    }
}
