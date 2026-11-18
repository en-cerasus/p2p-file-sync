pub mod blob;
pub mod materialize;
pub mod names;
pub mod projection;
pub mod replica;
pub mod retention;
pub mod store;

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
