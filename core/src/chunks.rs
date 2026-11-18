use std::{
    fs::File,
    io::{self, Read},
};

use sha2::{Digest, Sha256};

use crate::{
    blob::{Blob, BlobStore},
    replica::ContentHash,
    sha256,
};

pub const CHUNK_BYTES: u64 = 256 * 1024;
pub const MAX_CHUNKS: usize = 262_144;

/// A manifest for one immutable file version. The supplied file hash must
/// be bound to the authenticated event by the calling transport.
pub struct Manifest {
    file: Blob,
    hashes: Vec<ContentHash>,
}

impl Manifest {
    pub fn new(file: Blob, hashes: Vec<ContentHash>) -> io::Result<Self> {
        if hashes.len() != chunk_count(file.length)? {
            return Err(invalid("chunk count does not match file length"));
        }
        Ok(Self { file, hashes })
    }

    /// Build a manifest from an immutable, length-framed source.
    pub fn build(mut source: impl Read, length: u64) -> io::Result<Self> {
        let count = chunk_count(length)?;
        let mut hashes = Vec::with_capacity(count);
        let mut hash = Sha256::new();
        let mut buffer = vec![0; CHUNK_BYTES as usize];
        for index in 0..count {
            let bytes = &mut buffer[..chunk_length(length, index) as usize];
            source.read_exact(bytes)?;
            hash.update(&*bytes);
            hashes.push(sha256(bytes));
        }
        if source.read(&mut buffer[..1])? != 0 {
            return Err(invalid("source exceeds declared file length"));
        }
        Self::new(
            Blob {
                hash: hash.finalize().into(),
                length,
            },
            hashes,
        )
    }

    pub fn file(&self) -> Blob {
        self.file
    }

    pub fn hashes(&self) -> &[ContentHash] {
        &self.hashes
    }

    pub fn chunk(&self, index: usize) -> Option<Blob> {
        let hash = *self.hashes.get(index)?;
        Some(Blob {
            hash,
            length: chunk_length(self.file.length, index),
        })
    }

    /// Files in the chunk cache survive restarts and are shared by
    /// manifests. Only a missing file is requested again automatically.
    pub fn missing_chunks(&self, cache: &BlobStore) -> io::Result<Vec<usize>> {
        let mut missing = Vec::new();
        for index in 0..self.hashes.len() {
            let chunk = self.chunk(index).expect("index comes from the manifest");
            match cache.open_verified(chunk) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => missing.push(index),
                Err(error) => return Err(error),
            }
        }
        Ok(missing)
    }

    pub fn receive_chunk(
        &self,
        cache: &BlobStore,
        index: usize,
        source: impl Read,
    ) -> io::Result<()> {
        let chunk = self
            .chunk(index)
            .ok_or_else(|| invalid("unknown chunk index"))?;
        cache.put(chunk, source)
    }

    /// Assemble through a bounded reader and independently verify the
    /// complete file hash before publishing it to the completed-file store.
    pub fn finish(&self, cache: &BlobStore, completed: &BlobStore) -> io::Result<()> {
        match completed.open_verified(self.file) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        completed.put(
            self.file,
            ChunkReader {
                manifest: self,
                cache,
                next: 0,
                current: None,
            },
        )
    }
}

struct ChunkReader<'a> {
    manifest: &'a Manifest,
    cache: &'a BlobStore,
    next: usize,
    current: Option<File>,
}

impl Read for ChunkReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(file) = &mut self.current {
                let count = file.read(buffer)?;
                if count != 0 {
                    return Ok(count);
                }
            }
            let Some(chunk) = self.manifest.chunk(self.next) else {
                return Ok(0);
            };
            self.current = Some(self.cache.open_verified(chunk)?);
            self.next += 1;
        }
    }
}

fn chunk_count(length: u64) -> io::Result<usize> {
    let count = length.div_ceil(CHUNK_BYTES);
    if count > MAX_CHUNKS as u64 {
        return Err(invalid("file exceeds supported chunk count"));
    }
    Ok(count as usize)
}

fn chunk_length(length: u64, index: usize) -> u64 {
    (length - index as u64 * CHUNK_BYTES).min(CHUNK_BYTES)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
