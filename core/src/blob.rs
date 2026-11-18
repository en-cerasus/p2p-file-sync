use std::{
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::replica::ContentHash;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Blob {
    pub hash: ContentHash,
    pub length: u64,
}

/// Verified immutable content in an application-owned directory.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: root.as_ref().canonicalize()?,
        })
    }

    /// Consume exactly the expected number of payload bytes. Framing a
    /// transport stream to this length is the responsibility of the caller.
    pub fn put(&self, expected: Blob, mut source: impl Read) -> io::Result<()> {
        let mut temporary = NamedTempFile::with_prefix_in(".partial-", &self.root)?;
        let hash = hash_stream(&mut source, expected.length, &mut temporary)?;
        if hash != expected.hash {
            return Err(invalid("content hash mismatch"));
        }
        temporary.as_file().sync_all()?;
        let completed = self.path(expected.hash);
        match temporary.persist_noclobber(completed) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                self.open_verified(expected)?;
            }
            Err(error) => return Err(error.error),
        }
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    pub fn open_verified(&self, expected: Blob) -> io::Result<File> {
        let mut file = self.open_candidate(expected.hash)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("blob is not a regular file"));
        }
        let hash = hash_stream(&mut file, expected.length, &mut io::sink())?;
        if hash != expected.hash {
            return Err(invalid("stored content hash mismatch"));
        }
        file.rewind()?;
        Ok(file)
    }

    pub(crate) fn open_candidate(&self, hash: ContentHash) -> io::Result<File> {
        File::open(self.path(hash))
    }

    fn path(&self, hash: ContentHash) -> PathBuf {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = String::with_capacity(hash.len() * 2);
        for byte in hash {
            name.push(HEX[usize::from(byte >> 4)] as char);
            name.push(HEX[usize::from(byte & 15)] as char);
        }
        self.root.join(name)
    }
}

fn hash_stream(
    source: &mut impl Read,
    length: u64,
    target: &mut impl Write,
) -> io::Result<ContentHash> {
    let mut hash = Sha256::new();
    let mut remaining = length;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining != 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let data = &mut buffer[..limit];
        source.read_exact(data)?;
        hash.update(&*data);
        target.write_all(data)?;
        remaining -= limit as u64;
    }
    if source.take(1).read(&mut buffer[..1])? != 0 {
        return Err(invalid("content exceeds declared length"));
    }
    Ok(hash.finalize().into())
}

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256;

    #[test]
    fn only_verified_content_is_published_and_duplicates_reuse_the_same_blob() {
        let directory = tempfile::tempdir().unwrap();
        let store = BlobStore::open(directory.path()).unwrap();
        let data = vec![42; 70_003];
        let blob = Blob {
            hash: sha256(&data),
            length: data.len() as u64,
        };
        let mut corrupt = data.clone();
        corrupt[12] ^= 1;
        assert_eq!(
            store.put(blob, corrupt.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            store.put(blob, &data[..100]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        corrupt.push(1);
        assert_eq!(
            store.put(blob, corrupt.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);

        store.put(blob, data.as_slice()).unwrap();
        store.put(blob, data.as_slice()).unwrap();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        let mut read_back = Vec::new();
        store
            .open_verified(blob)
            .unwrap()
            .read_to_end(&mut read_back)
            .unwrap();
        assert_eq!(read_back, data);
    }

    #[test]
    fn a_corrupted_existing_blob_is_detected_and_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let store = BlobStore::open(directory.path()).unwrap();
        let blob = Blob {
            hash: sha256(b"abc"),
            length: 3,
        };
        let path = store.path(blob.hash);
        fs::write(&path, b"bad").unwrap();
        assert_eq!(
            store.put(blob, &b"abc"[..]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(fs::read(path).unwrap(), b"bad");
    }
}
