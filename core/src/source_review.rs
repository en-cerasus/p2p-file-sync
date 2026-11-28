use std::{collections::{BTreeMap, BTreeSet}, fs, io::{self, Read, Seek, SeekFrom, Write}, path::Path};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};

use crate::{
    chunks::Manifest,
    local_target::open_root,
    names::{collision_key, recovery_name},
    projection::Projection,
    replica::{EntryKind, Replica},
};

#[derive(Debug, Deserialize, Serialize)]
pub struct SourceArchiveReport {
    pub candidates: Vec<Vec<String>>,
    pub files: Vec<ArchivedSourceFile>,
    pub directories: Vec<Vec<String>>,
    pub missing: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ArchivedSourceFile {
    pub path: Vec<String>,
    pub bytes: u64,
    pub sha256: String,
}

/// Save physical review candidates without modifying the source or emitting
/// an event. A missing manifest means this new archive is incomplete.
pub(crate) fn archive(root: &Path, replica: &Replica, output: &Path)
    -> io::Result<SourceArchiveReport>
{
    let paths = candidates(root, replica)?;
    fs::create_dir(output)?;
    let source = open_root(root)?;
    let saved = output.join("source");
    fs::create_dir(&saved)?;
    let mut report = SourceArchiveReport {
        candidates: paths.clone(), files: Vec::new(), directories: Vec::new(), missing: Vec::new(),
    };
    let mut covered = Vec::<Vec<String>>::new();
    for path in paths {
        if covered.iter().any(|parent| path.starts_with(parent)) {
            continue;
        }
        if copy_candidate(&source, &saved, &path, &mut report)? {
            covered.push(path);
        }
    }
    let manifest = serde_json::to_vec_pretty(&report).map_err(io::Error::other)?;
    let mut file = fs::OpenOptions::new().write(true).create_new(true)
        .open(output.join("manifest.json"))?;
    file.write_all(&manifest)?;
    file.sync_all()?;
    Ok(report)
}

/// Recheck every copied file against the manifest without trusting a path
/// from the manifest to escape the archive's `source` directory.
pub fn verify_archive(output: &Path) -> io::Result<SourceArchiveReport> {
    let archive = open_root(output)?;
    if !archive.symlink_metadata("manifest.json")?.is_file()
        || !archive.symlink_metadata("source")?.is_dir()
    {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid archive layout"));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = archive.open_with("manifest.json", &options)?;
    if file.metadata()?.len() > 8 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "archive manifest too large"));
    }
    let mut bytes = Vec::new();
    file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "archive manifest too large"));
    }
    let report: SourceArchiveReport = serde_json::from_slice(&bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid archive manifest"))?;
    let root = archive.open_dir_nofollow("source")?;
    let mut seen = BTreeSet::new();
    let mut expected_directories = BTreeSet::new();
    for entry in &report.files {
        validate_archive_path(&entry.path)?;
        if !seen.insert(entry.path.clone()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "duplicate archive file"));
        }
        for length in 1..entry.path.len() {
            expected_directories.insert(entry.path[..length].to_vec());
        }
        let (name, parents) = entry.path.split_last().expect("validated nonempty path");
        let mut directory = root.try_clone()?;
        for parent in parents {
            directory = directory.open_dir_nofollow(parent)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = directory.open_with(name, &options)?;
        let length = file.metadata()?.len();
        if length != entry.bytes {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "archive file size changed"));
        }
        let hash = Manifest::build(&mut file, length)?.file().hash;
        let expected: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
        if expected != entry.sha256 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "archive file hash changed"));
        }
    }
    for path in &report.directories {
        validate_archive_path(path)?;
        expected_directories.insert(path.clone());
        for length in 1..path.len() {
            expected_directories.insert(path[..length].to_vec());
        }
        let mut directory = root.try_clone()?;
        for name in path {
            directory = directory.open_dir_nofollow(name)?;
        }
    }
    for path in report.candidates.iter().chain(&report.missing) {
        validate_archive_path(path)?;
    }
    let mut actual_files = BTreeSet::new();
    let mut actual_directories = BTreeSet::new();
    scan_archive_tree(&root, &mut Vec::new(), &mut actual_files,
                      &mut actual_directories, 0)?;
    if seen != actual_files || expected_directories != actual_directories {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            "archive contents do not match manifest"));
    }
    Ok(report)
}

fn scan_archive_tree(directory: &Dir, prefix: &mut Vec<String>,
                     files: &mut BTreeSet<Vec<String>>,
                     directories: &mut BTreeSet<Vec<String>>,
                     depth: usize) -> io::Result<()> {
    if depth >= 128 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "archive tree too deep"));
    }
    let mut children = directory.entries()?.collect::<io::Result<Vec<_>>>()?;
    children.sort_by_cached_key(|entry| entry.file_name());
    let mut names = BTreeSet::new();
    for child in children {
        let name = child.file_name().into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid archive name"))?;
        let key = collision_key(&name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid archive name"))?;
        if !names.insert(key) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "archive name collision"));
        }
        let kind = directory.symlink_metadata(&name)?.file_type();
        prefix.push(name.clone());
        if kind.is_dir() {
            directories.insert(prefix.clone());
            scan_archive_tree(&directory.open_dir_nofollow(&name)?, prefix,
                              files, directories, depth + 1)?;
        } else if kind.is_file() {
            files.insert(prefix.clone());
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidData,
                "unsupported archive item"));
        }
        prefix.pop();
    }
    Ok(())
}

fn validate_archive_path(path: &[String]) -> io::Result<()> {
    if path.is_empty() || path.len() > 128 || path.iter().any(|name| collision_key(name).is_err()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid archive path"));
    }
    Ok(())
}

/// Returns true when a directory covers all nested candidates.
fn copy_candidate(root: &Dir, output: &Path, path: &[String],
                  report: &mut SourceArchiveReport) -> io::Result<bool> {
    let (name, parents) = path.split_last().ok_or_else(|| io::Error::other("empty candidate"))?;
    let mut directory = root.try_clone()?;
    for parent in parents {
        match directory.open_dir_nofollow(parent) {
            Ok(next) => directory = next,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                report.missing.push(path.to_vec());
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
    }
    let kind = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata.file_type(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            report.missing.push(path.to_vec());
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let target = path.iter().fold(output.to_path_buf(), |base, name| base.join(name));
    if kind.is_dir() {
        let child = directory.open_dir_nofollow(name)?;
        copy_directory(&child, &target, path, report, 0)?;
        Ok(true)
    } else if kind.is_file() {
        copy_file(&directory, name, &target, path, report)?;
        Ok(false)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported source item"))
    }
}

fn copy_directory(source: &Dir, target: &Path, path: &[String],
                  report: &mut SourceArchiveReport, depth: usize) -> io::Result<()> {
    if depth >= 128 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "source tree too deep"));
    }
    fs::create_dir_all(target)?;
    report.directories.push(path.to_vec());
    let mut children = source.entries()?.collect::<io::Result<Vec<_>>>()?;
    children.sort_by_cached_key(|entry| entry.file_name());
    for entry in children {
        let name = entry.file_name().into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid source name"))?;
        if recovery_name(&name) { continue; }
        collision_key(&name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid source name"))?;
        let mut child_path = path.to_vec();
        child_path.push(name.clone());
        let child_target = target.join(&name);
        let kind = source.symlink_metadata(&name)?.file_type();
        if kind.is_dir() {
            copy_directory(&source.open_dir_nofollow(&name)?, &child_target,
                           &child_path, report, depth + 1)?;
        } else if kind.is_file() {
            copy_file(source, &name, &child_target, &child_path, report)?;
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported source item"));
        }
    }
    Ok(())
}

fn copy_file(source: &Dir, name: &str, target: &Path, path: &[String],
             report: &mut SourceArchiveReport) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut input = source.open_with(name, &options)?;
    fs::create_dir_all(target.parent().ok_or_else(|| io::Error::other("missing parent"))?)?;
    let mut output = fs::OpenOptions::new().read(true).write(true)
        .create_new(true).open(target)?;
    let length = io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    input.seek(SeekFrom::Start(0))?;
    let original = Manifest::build(&mut input, length)?.file();
    output.seek(SeekFrom::Start(0))?;
    let copied = Manifest::build(&mut output, length)?.file();
    if original.hash != copied.hash {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "source changed during archive"));
    }
    report.files.push(ArchivedSourceFile {
        path: path.to_vec(), bytes: length,
        sha256: original.hash.iter().map(|byte| format!("{byte:02x}")).collect(),
    });
    Ok(())
}

/// Compare a physical source folder with the recovered shared heads without
/// storing blobs or publishing events. Each returned path needs user review.
pub(crate) fn candidates(root: &Path, replica: &Replica) -> io::Result<Vec<Vec<String>>> {
    let projection = Projection::new(replica);
    let mut expected = BTreeMap::new();
    for object in replica.objects() {
        let mut heads = replica.heads(object);
        let Some(head) = heads.next() else { continue };
        if heads.any(|other| other.change.entry() != head.change.entry()) {
            continue;
        }
        let (Ok(path), Some(entry)) = (projection.path(object), head.change.entry()) else {
            continue;
        };
        expected.insert(
            path.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            entry.kind.clone(),
        );
    }
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    visit(&open_root(root)?, &mut Vec::new(), &expected, &mut seen, &mut found, 0)?;
    // A local deletion is also a difference that needs review. Report the
    // missing parent once instead of listing every child below it.
    for path in expected.keys() {
        if !seen.contains(path) && !found.iter().any(|candidate| path.starts_with(candidate)) {
            found.push(path.clone());
        }
    }
    found.sort();
    Ok(found)
}

fn visit(
    directory: &Dir,
    prefix: &mut Vec<String>,
    expected: &BTreeMap<Vec<String>, EntryKind>,
    seen: &mut BTreeSet<Vec<String>>,
    found: &mut Vec<Vec<String>>,
    depth: usize,
) -> io::Result<()> {
    if depth >= 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source tree too deep",
        ));
    }
    let mut children = directory.entries()?.collect::<io::Result<Vec<_>>>()?;
    children.sort_by_cached_key(|entry| entry.file_name());
    let mut names = std::collections::BTreeSet::new();
    for child in children {
        let kind = child.file_type()?;
        if !kind.is_dir() && !kind.is_file() {
            continue;
        }
        let name = child
            .file_name()
            .into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid source name"))?;
        if recovery_name(&name) {
            continue;
        }
        let key = collision_key(&name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid source name"))?;
        if !names.insert(key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "source name collision",
            ));
        }
        prefix.push(name.clone());
        seen.insert(prefix.clone());
        if kind.is_dir() {
            if expected.get(prefix) != Some(&EntryKind::Directory) {
                found.push(prefix.clone());
            }
            let child = directory.open_dir_nofollow(&name)?;
            visit(&child, prefix, expected, seen, found, depth + 1)?;
        } else {
            if !directory.symlink_metadata(&name)?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "source changed while reviewing",
                ));
            }
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let mut file = directory.open_with(&name, &options)?;
            let length = file.metadata()?.len();
            let actual = Manifest::build(&mut file, length)?.file().hash;
            if expected.get(prefix) != Some(&EntryKind::File(actual)) {
                found.push(prefix.clone());
            }
        }
        prefix.pop();
    }
    Ok(())
}
