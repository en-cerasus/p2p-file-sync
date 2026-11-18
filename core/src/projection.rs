use std::collections::{BTreeMap, BTreeSet};

use crate::{
    names::collision_key,
    replica::{Entry, EntryKind, ObjectId, Replica},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathIssue {
    Missing(ObjectId),
    Deleted(ObjectId),
    Divergent(ObjectId),
    InvalidName(ObjectId),
    NameCollision(ObjectId),
    ParentIsFile(ObjectId),
    Cycle(ObjectId),
}

/// Filesystem-independent view of the current causal heads. Unresolved
/// branches reserve their names so other objects cannot claim those paths.
pub struct Projection<'a> {
    replica: &'a Replica,
    occupants: BTreeMap<(Option<ObjectId>, String), BTreeSet<ObjectId>>,
}

impl<'a> Projection<'a> {
    pub fn new(replica: &'a Replica) -> Self {
        let mut occupants = BTreeMap::<_, BTreeSet<_>>::new();
        for object in replica.objects() {
            for entry in replica.heads(object).filter_map(|head| head.change.entry()) {
                if let Ok(key) = collision_key(&entry.name) {
                    occupants
                        .entry((entry.parent, key))
                        .or_default()
                        .insert(object);
                }
            }
        }
        Self { replica, occupants }
    }

    pub fn path(&self, object: ObjectId) -> Result<Vec<&'a str>, PathIssue> {
        let mut seen = BTreeSet::new();
        let mut components = Vec::new();
        let mut current = object;
        loop {
            if !seen.insert(current) {
                return Err(PathIssue::Cycle(current));
            }
            let entry = self.entry(current)?;
            if current != object && entry.kind != EntryKind::Directory {
                return Err(PathIssue::ParentIsFile(current));
            }
            let key = collision_key(&entry.name).map_err(|_| PathIssue::InvalidName(current))?;
            if self.occupants[&(entry.parent, key)].len() != 1 {
                return Err(PathIssue::NameCollision(current));
            }
            components.push(entry.name.as_str());
            let Some(parent) = entry.parent else {
                components.reverse();
                return Ok(components);
            };
            current = parent;
        }
    }

    pub(crate) fn vacant(&self, parent: Option<ObjectId>, name: &str) -> bool {
        collision_key(name).is_ok_and(|key| !self.occupants.contains_key(&(parent, key)))
    }

    pub(crate) fn available_to(&self, object: ObjectId, entry: &Entry) -> bool {
        collision_key(&entry.name).is_ok_and(|key| {
            self.occupants
                .get(&(entry.parent, key))
                .is_none_or(|objects| objects.iter().all(|&occupant| occupant == object))
        })
    }

    fn entry(&self, object: ObjectId) -> Result<&'a Entry, PathIssue> {
        let mut heads = self.replica.heads(object);
        let head = heads.next().ok_or(PathIssue::Missing(object))?;
        let entry = head.change.entry();
        if heads.any(|other| other.change.entry() != entry) {
            return Err(PathIssue::Divergent(object));
        }
        entry.ok_or(PathIssue::Deleted(object))
    }
}
