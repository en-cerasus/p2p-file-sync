use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};

use crate::{
    blob::Blob,
    materialize::{FileIntent, FileTarget},
    names::validate_component,
    replica::ObjectId,
};

pub(crate) struct NewFileTarget {
    parent: Dir,
    name: String,
}

impl NewFileTarget {
    pub(crate) fn open(root: &Dir, components: &[&str]) -> io::Result<Self> {
        let (name, ancestors) = components.split_last().ok_or_else(invalid_name)?;
        let mut parent = root.try_clone()?;
        for component in ancestors {
            validate_component(component).map_err(|_| invalid_name())?;
            parent = match parent.open_dir_nofollow(component) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match parent.create_dir(component) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    parent.open_dir_nofollow(component)?
                }
                result => result?,
            };
        }
        validate_component(name).map_err(|_| invalid_name())?;
        Ok(Self {
            parent,
            name: (*name).into(),
        })
    }
}

impl FileTarget for NewFileTarget {
    fn inspect(&mut self, _: ObjectId) -> io::Result<Option<Blob>> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        match self.parent.symlink_metadata(&self.name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Ok(metadata) if metadata.is_file() => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "target is not a regular file",
                ))
            }
        }
        let mut file = self.parent.open_with(&self.name, &options)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "target is not a regular file",
            ));
        }
        let mut hash = Sha256::new();
        let length = io::copy(&mut file, &mut hash)?;
        Ok(Some(Blob {
            hash: hash.finalize().into(),
            length,
        }))
    }

    fn replace_if_current(
        &mut self,
        intent: &FileIntent,
        replacement: Option<File>,
    ) -> io::Result<bool> {
        let Some(mut replacement) = replacement.filter(|_| intent.previous.is_none()) else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "only new files can be published",
            ));
        };
        let name = scratch_name()?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut scratch = self.parent.open_with(&name, &options)?;
        let result = (|| {
            io::copy(&mut replacement, &mut scratch)?;
            scratch.flush()?;
            scratch.sync_all()?;
            // Linking a completed inode publishes it without replacing a
            // file created by another writer while we copied the data.
            match self.parent.hard_link(&name, &self.parent, &self.name) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
                Err(error) => Err(error),
            }
        })();
        drop(scratch);
        let cleanup = self.parent.remove_file(&name);
        let linked = result?;
        cleanup?;
        #[cfg(unix)]
        self.parent.try_clone()?.into_std_file().sync_all()?;
        Ok(linked)
    }
}

fn scratch_name() -> io::Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(format!(".p2p-part-{:032x}", u128::from_be_bytes(bytes)))
}

fn invalid_name() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid output path")
}

pub(crate) fn open_root(path: &Path) -> io::Result<Dir> {
    Dir::open_ambient_dir(path, cap_std::ambient_authority())
}
