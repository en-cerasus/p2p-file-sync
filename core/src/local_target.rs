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
    names::{collision_key, validate_component},
    replica::ObjectId,
};

pub(crate) struct NewFileTarget {
    parent: Dir,
    name: String,
}

/// Apple filesystem operations are only safe to invoke inside a Swift
/// NSFileCoordinator accessor for the affected application-owned URLs.
#[cfg(target_vendor = "apple")]
pub(crate) struct AppleExistingTarget {
    target: NewFileTarget,
    intent: FileIntent,
}

#[cfg(target_vendor = "apple")]
impl AppleExistingTarget {
    pub(crate) fn open(root: &Dir, components: &[&str], intent: &FileIntent) -> io::Result<Self> {
        Ok(Self { target: NewFileTarget::existing(root, components)?, intent: intent.clone() })
    }

    /// A stale operation may have already moved the old file to its reserved
    /// name. Put it back before dropping the intent, without replacing an
    /// intervening local file.
    pub(crate) fn restore_original_if_staged(&self) -> io::Result<bool> {
        let Some(previous) = self.intent.previous else { return Ok(false) };
        if self.target.inspect_name(&self.target.name)? == Some(previous) {
            return Ok(true);
        }
        let backup = update_backup(&self.intent);
        if self.target.inspect_name(&self.target.name)?.is_none()
            && self.target.inspect_name(&backup)? == Some(previous)
        {
            return rename_exclusive(&self.target.parent, &backup,
                                    &self.target.parent, &self.target.name);
        }
        Ok(false)
    }

    fn status(&self) -> io::Result<Option<Blob>> {
        let current = self.target.inspect_name(&self.target.name)?;
        let backup = update_backup(&self.intent);
        let saved = self.target.inspect_name(&backup)?;
        if saved.is_some() && saved != self.intent.previous {
            return Ok(saved);
        }
        if current == self.intent.desired {
            if saved == self.intent.previous && saved.is_some() {
                self.target.parent.remove_file(&backup)?;
                sync_directory(&self.target.parent)?;
            }
            return Ok(current);
        }
        if current.is_none() && saved == self.intent.previous && saved.is_some() {
            return Ok(saved);
        }
        Ok(current)
    }

    fn replace(&self, replacement: Option<File>) -> io::Result<bool> {
        let Some(previous) = self.intent.previous else { return Ok(false) };
        if self.status()? != Some(previous) { return Ok(false); }
        let backup = update_backup(&self.intent);
        match (self.target.inspect_name(&self.target.name)?, self.target.inspect_name(&backup)?) {
            (Some(current), None) if current == previous => {
                if !rename_exclusive(&self.target.parent, &self.target.name,
                                     &self.target.parent, &backup)? {
                    return Ok(false);
                }
            }
            (None, Some(saved)) if saved == previous => {}
            _ => return Ok(false),
        }
        if self.target.inspect_name(&backup)? != Some(previous) { return Ok(false); }
        if let Some(file) = replacement {
            if !self.target.publish_at_vacant_name(file)? { return Ok(false); }
        }
        Ok(self.status()? == self.intent.desired)
    }
}

#[cfg(target_vendor = "apple")]
impl FileTarget for AppleExistingTarget {
    fn inspect(&mut self, _: ObjectId) -> io::Result<Option<Blob>> { self.status() }

    fn replace_if_current(&mut self, intent: &FileIntent, replacement: Option<File>) -> io::Result<bool> {
        if *intent != self.intent { return Ok(false); }
        self.replace(replacement)
    }
}

#[cfg(target_vendor = "apple")]
pub(crate) struct AppleMoveTarget {
    source: NewFileTarget,
    destination: AppleExistingTarget,
}

#[cfg(target_vendor = "apple")]
impl AppleMoveTarget {
    pub(crate) fn open(root: &Dir, before: &[&str], after: &[&str], intent: &FileIntent) -> io::Result<Self> {
        Ok(Self {
            source: NewFileTarget::existing(root, before)?,
            destination: AppleExistingTarget::open(root, after, intent)?,
        })
    }
}

#[cfg(target_vendor = "apple")]
impl FileTarget for AppleMoveTarget {
    fn inspect(&mut self, object: ObjectId) -> io::Result<Option<Blob>> {
        self.destination.inspect(object)
    }

    fn replace_if_current(&mut self, intent: &FileIntent, replacement: Option<File>) -> io::Result<bool> {
        self.destination.replace_if_current(intent, replacement)
    }
}

#[cfg(target_vendor = "apple")]
impl crate::materialize::MoveFileTarget for AppleMoveTarget {
    fn already_moved(&mut self, previous: Blob, desired: Blob, same_slot: bool) -> io::Result<bool> {
        if same_slot || !self.source.name_is_vacant()? { return Ok(false); }
        let current = self.destination.status()?;
        Ok(current == Some(previous) || current == Some(desired))
    }

    fn move_if_current(&mut self, previous: Blob, same_slot: bool) -> io::Result<bool> {
        if same_slot || !self.destination.target.name_is_vacant()? { return Ok(false); }
        if self.source.inspect_name(&self.source.name)? != Some(previous) { return Ok(false); }
        if !rename_exclusive(&self.source.parent, &self.source.name,
                             &self.destination.target.parent, &self.destination.target.name)? {
            return Ok(false);
        }
        Ok(self.destination.target.inspect_name(&self.destination.target.name)? == Some(previous))
    }
}

impl NewFileTarget {
    pub(crate) fn name_is_vacant(&self) -> io::Result<bool> {
        let key = collision_key(&self.name).map_err(|_| invalid_name())?;
        for entry in self.parent.entries()? {
            if entry?
                .file_name()
                .to_str()
                .and_then(|name| collision_key(name).ok())
                .is_some_and(|candidate| candidate == key)
            {
                return Ok(false);
            }
        }
        match self.parent.symlink_metadata(&self.name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Ok(_) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn existing(root: &Dir, components: &[&str]) -> io::Result<Self> {
        let (name, ancestors) = components.split_last().ok_or_else(invalid_name)?;
        let mut parent = root.try_clone()?;
        for component in ancestors {
            validate_component(component).map_err(|_| invalid_name())?;
            parent = parent.open_dir_nofollow(component)?;
        }
        validate_component(name).map_err(|_| invalid_name())?;
        Ok(Self {
            parent,
            name: (*name).into(),
        })
    }

    #[cfg(windows)]
    fn lock_verified(&self, name: &str, expected: Blob) -> io::Result<Option<cap_std::fs::File>> {
        use cap_std::fs::{MetadataExt, OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_GENERIC_READ,
        };

        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        options
            .access_mode(FILE_GENERIC_READ | DELETE)
            .share_mode(0);
        let mut file = match self.parent.open_with(name, &options) {
            Ok(file) => file,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(32) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let meta = file.metadata()?;
        if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(None);
        }
        if measure(&mut file)? != expected {
            return Ok(None);
        }
        Ok(Some(file))
    }

    #[cfg(windows)]
    pub(crate) fn move_verified(
        &self,
        destination: &Self,
        expected: Blob,
        same_slot: bool,
    ) -> io::Result<bool> {
        let safe = if same_slot {
            self.is_only_spelling()?
        } else {
            destination.name_is_vacant()?
        };
        if !safe {
            return Ok(false);
        }
        let Some(file) = self.lock_verified(&self.name, expected)? else {
            return Ok(false);
        };
        match rename_into(&file, Some(&destination.parent), &destination.name) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Check the stored spelling and reject a second spelling on a
    /// case-sensitive directory. Opening by name alone cannot do this.
    #[cfg(windows)]
    pub(crate) fn is_only_spelling(&self) -> io::Result<bool> {
        let key = collision_key(&self.name).map_err(|_| invalid_name())?;
        let mut found = false;
        for entry in self.parent.entries()? {
            let name = entry?.file_name();
            if let Some(name) = name
                .to_str()
                .filter(|name| collision_key(name).is_ok_and(|candidate| candidate == key))
            {
                if name != self.name {
                    return Ok(false);
                }
                found = true;
            }
        }
        Ok(found)
    }

    #[cfg(windows)]
    fn delete_named_verified(&self, name: &str, expected: Blob) -> io::Result<bool> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
        };

        let Some(file) = self.lock_verified(name, expected)? else {
            return Ok(false);
        };
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // The file stays locked between the content check and the disposition.
        let success = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        drop(file);
        Ok(true)
    }

    #[cfg(windows)]
    pub(crate) fn update_status(&self, intent: &FileIntent) -> io::Result<Option<Blob>> {
        let current = self.inspect_name(&self.name)?;
        if current == intent.desired {
            self.finish_update(intent)?;
        }
        if current.is_none() {
            // A verified backup proves the pending transition removed the
            // live name. Absence without this evidence is a local edit.
            let backup = update_backup(intent);
            if self.inspect_name(&backup)? == intent.previous {
                return Ok(intent.previous);
            }
        }
        Ok(current)
    }

    #[cfg(windows)]
    pub(crate) fn restore_update_backup(
        &self,
        intent: &FileIntent,
        previous: Blob,
    ) -> io::Result<bool> {
        let Some(file) = self.lock_verified(&update_backup(intent), previous)? else {
            return Ok(false);
        };
        // A handle-based, no-replacement rename ensures a concurrent
        // creator keeps their file and the backup stays available.
        match rename_by_handle(&file, &self.name) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    #[cfg(windows)]
    pub(crate) fn finish_update(&self, intent: &FileIntent) -> io::Result<()> {
        let backup = update_backup(intent);
        if let Some(previous) = intent.previous {
            // If the reserved path contains unrelated data, leave it alone.
            self.delete_named_verified(&backup, previous)?;
        }
        Ok(())
    }

    #[cfg(windows)]
    fn update_verified(
        &self,
        intent: &FileIntent,
        replacement: File,
        previous: Blob,
    ) -> io::Result<bool> {
        let backup = update_backup(intent);
        let current = self.inspect_name(&self.name)?;
        let backup_contents = self.inspect_name(&backup)?;
        match (current, backup_contents) {
            (Some(current), None) if current == previous => {
                let Some(file) = self.lock_verified(&self.name, previous)? else {
                    return Ok(false);
                };
                rename_by_handle(&file, &backup)?;
            }
            (None, Some(saved)) if saved == previous => {}
            _ => return Ok(false),
        }
        let published = self.publish_at_vacant_name(replacement)?;
        if published {
            self.finish_update(intent)?;
        }
        Ok(published)
    }

    pub(crate) fn open(root: &Dir, components: &[&str]) -> io::Result<Self> {
        let (name, ancestors) = components.split_last().ok_or_else(invalid_name)?;
        let parent = open_or_create_directories(root, ancestors)?;
        validate_component(name).map_err(|_| invalid_name())?;
        Ok(Self {
            parent,
            name: (*name).into(),
        })
    }
}

pub(crate) fn open_or_create_directories(root: &Dir, components: &[&str]) -> io::Result<Dir> {
    let mut parent = root.try_clone()?;
    for component in components {
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
    Ok(parent)
}

impl NewFileTarget {
    fn inspect_name(&self, name: &str) -> io::Result<Option<Blob>> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        match self.parent.symlink_metadata(name) {
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
        let mut file = self.parent.open_with(name, &options)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "target is not a regular file",
            ));
        }
        Ok(Some(measure(&mut file)?))
    }

    fn publish_at_vacant_name(&self, mut replacement: File) -> io::Result<bool> {
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

impl FileTarget for NewFileTarget {
    fn inspect(&mut self, _: ObjectId) -> io::Result<Option<Blob>> {
        self.inspect_name(&self.name)
    }

    fn replace_if_current(
        &mut self,
        intent: &FileIntent,
        replacement: Option<File>,
    ) -> io::Result<bool> {
        #[cfg(windows)]
        if let Some(previous) = intent.previous {
            return match replacement {
                Some(file) => self.update_verified(intent, file, previous),
                None => self.delete_named_verified(&self.name, previous),
            };
        }
        match replacement {
            Some(file) if intent.previous.is_none() => self.publish_at_vacant_name(file),
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported file transition",
            )),
        }
    }
}

fn measure(file: &mut cap_std::fs::File) -> io::Result<Blob> {
    let mut hash = Sha256::new();
    let length = io::copy(file, &mut hash)?;
    Ok(Blob {
        hash: hash.finalize().into(),
        length,
    })
}

#[cfg(any(windows, target_vendor = "apple"))]
fn update_backup(intent: &FileIntent) -> String {
    let mut context = Sha256::new();
    context.update(intent.object);
    context.update(intent.event.peer);
    context.update(intent.event.sequence.to_be_bytes());
    format!(".p2p-update-{:x}", context.finalize())
}

#[cfg(target_vendor = "apple")]
fn sync_directory(directory: &Dir) -> io::Result<()> {
    directory.try_clone()?.into_std_file().sync_all()
}

/// Darwin's exclusive rename leaves a concurrently created target intact.
/// Callers first verify the source under file coordination and recheck the
/// reserved name after this call. Unsupported volumes fail closed.
#[cfg(target_vendor = "apple")]
fn rename_exclusive(source: &Dir, old: &str, destination: &Dir, new: &str) -> io::Result<bool> {
    use std::{ffi::CString, os::{fd::AsRawFd, raw::{c_char, c_int, c_uint}}};
    unsafe extern "C" {
        fn renameatx_np(old_fd: c_int, old_name: *const c_char,
                        new_fd: c_int, new_name: *const c_char, flags: c_uint) -> c_int;
    }
    const RENAME_EXCL: c_uint = 0x00000004;
    let old = CString::new(old).map_err(|_| invalid_name())?;
    let new = CString::new(new).map_err(|_| invalid_name())?;
    let result = unsafe {
        renameatx_np(source.as_raw_fd(), old.as_ptr(), destination.as_raw_fd(), new.as_ptr(), RENAME_EXCL)
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::AlreadyExists { Ok(false) } else { Err(error) };
    }
    sync_directory(source)?;
    sync_directory(destination)?;
    Ok(true)
}

#[cfg(windows)]
fn rename_by_handle(file: &cap_std::fs::File, name: &str) -> io::Result<()> {
    rename_into(file, None, name)
}

#[cfg(windows)]
fn rename_into(file: &cap_std::fs::File, parent: Option<&Dir>, name: &str) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_RENAME_INFO;

    use windows_sys::{
        Wdk::Storage::FileSystem::{FileRenameInformation, NtSetInformationFile},
        Win32::{Foundation::RtlNtStatusToDosError, System::IO::IO_STATUS_BLOCK},
    };

    // FILE_RENAME_INFO ends with a flexible UTF-16 array. The fixed backing
    // struct retains native alignment and provides room for our fixed ASCII name.
    #[repr(C)]
    struct RenameBuffer {
        prefix: FILE_RENAME_INFO,
        tail: [u16; 256],
    }
    let mut buffer = RenameBuffer {
        prefix: FILE_RENAME_INFO::default(),
        tail: [0; 256],
    };
    let name: Vec<u16> = name.encode_utf16().collect();
    if name.len() >= 256 {
        return Err(invalid_name());
    }
    let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    buffer.prefix.RootDirectory = parent.map_or(std::ptr::null_mut(), AsRawHandle::as_raw_handle);
    buffer.prefix.FileNameLength = (name.len() * 2) as u32;
    let address = (&mut buffer as *mut RenameBuffer).cast::<u8>();
    let mut status_block = IO_STATUS_BLOCK::default();
    let status = unsafe {
        // The offset is naturally aligned for UTF-16 and the tail covers the
        // bytes passed to Windows, including termination.
        std::ptr::copy_nonoverlapping(name.as_ptr(), address.add(offset).cast::<u16>(), name.len());
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status_block,
            address.cast(),
            std::mem::size_of::<RenameBuffer>() as u32,
            FileRenameInformation,
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(())
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

#[cfg(all(test, target_vendor = "apple"))]
mod apple_tests {
    use super::*;
    use crate::materialize::MoveFileTarget;
    use crate::replica::EventId;

    fn intent(desired: Option<Blob>) -> FileIntent {
        FileIntent {
            object: [4; 16],
            event: EventId { peer: [5; 32], sequence: 7 },
            previous: Some(Blob { hash: crate::sha256(b"old"), length: 3 }),
            desired,
        }
    }

    #[test]
    fn apple_update_resumes_from_backup_and_preserves_a_new_occupant() {
        let folder = tempfile::tempdir().unwrap();
        let root = open_root(folder.path()).unwrap();
        let path = folder.path().join("song.tja");
        let replacement = folder.path().join("replacement");
        std::fs::write(&path, b"old").unwrap();
        std::fs::write(&replacement, b"new").unwrap();
        let desired = Blob { hash: crate::sha256(b"new"), length: 3 };
        let intent = intent(Some(desired));
        let backup = update_backup(&intent);
        assert!(rename_exclusive(&root, "song.tja", &root, &backup).unwrap());
        std::fs::write(&path, b"local edit").unwrap();
        let mut target = AppleExistingTarget::open(&root, &["song.tja"], &intent).unwrap();
        assert!(!target.replace_if_current(&intent, Some(File::open(&replacement).unwrap())).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"local edit");
        assert_eq!(std::fs::read(folder.path().join(&backup)).unwrap(), b"old");

        std::fs::remove_file(&path).unwrap();
        assert!(target.replace_if_current(&intent, Some(File::open(&replacement).unwrap())).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!folder.path().join(&backup).exists());
        std::fs::write(folder.path().join(&backup), b"old").unwrap();
        assert_eq!(target.inspect(intent.object).unwrap(), Some(desired));
        assert!(!folder.path().join(&backup).exists());
    }

    #[test]
    fn apple_deletion_removes_only_the_verified_version() {
        let folder = tempfile::tempdir().unwrap();
        let root = open_root(folder.path()).unwrap();
        let path = folder.path().join("song.tja");
        std::fs::write(&path, b"local edit").unwrap();
        let intent = intent(None);
        let mut target = AppleExistingTarget::open(&root, &["song.tja"], &intent).unwrap();
        assert!(!target.replace_if_current(&intent, None).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"local edit");
        std::fs::write(&path, b"old").unwrap();
        assert!(target.replace_if_current(&intent, None).unwrap());
        assert!(!path.exists());
        assert!(!folder.path().join(update_backup(&intent)).exists());
    }

    #[test]
    fn apple_move_rejects_an_occupied_name_and_can_then_update() {
        let folder = tempfile::tempdir().unwrap();
        let root = open_root(folder.path()).unwrap();
        let source = folder.path().join("song.tja");
        let destination = folder.path().join("moved.tja");
        let replacement = folder.path().join("replacement");
        std::fs::write(&source, b"old").unwrap();
        std::fs::write(&destination, b"local edit").unwrap();
        std::fs::write(&replacement, b"new").unwrap();
        let previous = Blob { hash: crate::sha256(b"old"), length: 3 };
        let desired = Blob { hash: crate::sha256(b"new"), length: 3 };
        let intent = intent(Some(desired));
        let mut target = AppleMoveTarget::open(&root, &["song.tja"], &["moved.tja"], &intent).unwrap();
        assert!(!target.move_if_current(previous, false).unwrap());
        assert_eq!(std::fs::read(&source).unwrap(), b"old");
        assert_eq!(std::fs::read(&destination).unwrap(), b"local edit");

        std::fs::remove_file(&destination).unwrap();
        assert!(target.move_if_current(previous, false).unwrap());
        assert!(target.already_moved(previous, desired, false).unwrap());
        assert!(!source.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");
        assert!(target.replace_if_current(&intent, Some(File::open(&replacement).unwrap())).unwrap());
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
    }
}
