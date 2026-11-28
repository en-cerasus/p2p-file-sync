use std::{collections::{BTreeMap, BTreeSet}, io, path::Path};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};

use crate::{
    chunks::Manifest,
    local_target::open_root,
    names::{collision_key, recovery_name},
    projection::Projection,
    replica::{EntryKind, Replica},
};

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
