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
        )
    }
}

unsafe fn pull_to_output<T: From<host::PullSummaryV3>>(
    paths: [(*const u8, usize); 3],
    output: *mut T,
    originals: Option<OriginalsCallback>,
    context: *mut std::ffi::c_void,
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
        let (summary, entries) =
            host::pull_with_originals(config, state, destination).map_err(|error| match error {
                host::HostError::Configuration => 2,
                host::HostError::Io(_) | host::HostError::Store(_) => 3,
                host::HostError::Tls(_) => 4,
                host::HostError::Direct(_)
                | host::HostError::Publish(_)
                | host::HostError::Transition(_)
                | host::HostError::CheckpointData(_) => 5,
            })?;
        let json = originals
            .map(|_| serde_json::to_vec(&entries))
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
