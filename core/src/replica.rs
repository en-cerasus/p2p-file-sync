use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub type PeerId = [u8; 32];
pub type ObjectId = [u8; 16];
pub type ContentHash = [u8; 32];
pub type VersionVector = BTreeMap<PeerId, u64>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EventId {
    pub peer: PeerId,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum EntryKind {
    File(ContentHash),
    Directory,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Entry {
    pub parent: Option<ObjectId>,
    pub name: String,
    pub kind: EntryKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Change {
    Create(Entry),
    Revise {
        base: EventId,
        entry: Option<Entry>,
    },
    Resolve {
        bases: BTreeSet<EventId>,
        entry: Option<Entry>,
    },
}

impl Change {
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Create(entry) => Some(entry),
            Self::Revise { entry, .. } | Self::Resolve { entry, .. } => entry.as_ref(),
        }
    }
}

/// One event in a single share and membership generation. The caller must
/// authenticate author, share, generation, and payload before handing it over.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Event {
    pub id: EventId,
    pub context: VersionVector,
    pub object: ObjectId,
    pub change: Change,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Apply {
    Added,
    Duplicate,
    MissingDependencies(Vec<EventId>),
}

#[derive(Debug, Eq, PartialEq)]
pub enum Reject {
    InvalidSequence,
    InvalidContext,
    InvalidBasis,
    InvalidEntry,
    EventIdReuse,
}

/// In-memory causal index. The event log retains superseded versions for
/// reconstruction and conflict recovery.
#[derive(Default)]
pub struct Replica {
    clock: VersionVector,
    events: BTreeMap<EventId, Event>,
    history: BTreeMap<ObjectId, Vec<EventId>>,
    heads: BTreeMap<ObjectId, BTreeSet<EventId>>,
}

impl Replica {
    pub fn clock(&self) -> &VersionVector {
        &self.clock
    }

    pub fn get(&self, id: EventId) -> Option<&Event> {
        self.events.get(&id)
    }

    pub fn heads(&self, object: ObjectId) -> impl Iterator<Item = &Event> {
        self.heads
            .get(&object)
            .into_iter()
            .flatten()
            .map(|id| &self.events[id])
    }

    /// Multiple identical heads retain their causal identities but do not
    /// require user intervention.
    pub fn has_conflict(&self, object: ObjectId) -> bool {
        let mut heads = self.heads(object);
        let Some(first) = heads.next() else {
            return false;
        };
        heads.any(|head| head.change.entry() != first.change.entry())
    }

    /// Missing events can be retried without any change to local state.
    /// Each reported ID is the highest required sequence for that peer.
    pub fn apply(&mut self, event: Event) -> Result<Apply, Reject> {
        let result = self.validate(&event)?;
        if result == Apply::Added {
            self.record(event);
        }
        Ok(result)
    }

    pub(crate) fn validate(&self, event: &Event) -> Result<Apply, Reject> {
        if let Some(existing) = self.events.get(&event.id) {
            return if existing == event {
                Ok(Apply::Duplicate)
            } else {
                Err(Reject::EventIdReuse)
            };
        }

        if let Some(entry) = event.change.entry() {
            if entry.parent == Some(event.object)
                || crate::names::validate_component(&entry.name).is_err()
            {
                return Err(Reject::InvalidEntry);
            }
        }

        let predecessor = clock_at(&event.context, event.id.peer);
        if predecessor.checked_add(1) != Some(event.id.sequence) {
            return Err(Reject::InvalidSequence);
        }
        if event.context.values().any(|&sequence| sequence == 0) {
            return Err(Reject::InvalidContext);
        }

        let frontiers = event
            .context
            .iter()
            .map(|(&peer, &sequence)| EventId { peer, sequence });
        let missing: Vec<_> = frontiers
            .clone()
            .filter(|id| !covers(&self.clock, *id))
            .collect();
        if !missing.is_empty() {
            return Ok(Apply::MissingDependencies(missing));
        }

        for id in frontiers {
            if self.events[&id]
                .context
                .iter()
                .any(|(&peer, &sequence)| clock_at(&event.context, peer) < sequence)
            {
                return Err(Reject::InvalidContext);
            }
        }

        let visible_heads = self.heads_at(event.object, &event.context);
        let valid = match &event.change {
            Change::Create(_) => visible_heads.is_empty(),
            Change::Revise { base, .. } => visible_heads.len() == 1 && visible_heads.contains(base),
            Change::Resolve { bases, .. } => visible_heads.len() >= 2 && &visible_heads == bases,
        };
        if !valid {
            return Err(Reject::InvalidBasis);
        }

        Ok(Apply::Added)
    }

    pub(crate) fn record(&mut self, event: Event) {
        let heads = self.heads.entry(event.object).or_default();
        heads.retain(|id| !covers(&event.context, *id));
        heads.insert(event.id);
        self.history.entry(event.object).or_default().push(event.id);
        self.clock.insert(event.id.peer, event.id.sequence);
        self.events.insert(event.id, event);
    }

    fn heads_at(&self, object: ObjectId, context: &VersionVector) -> BTreeSet<EventId> {
        let mut heads = BTreeSet::new();
        if let Some(history) = self.history.get(&object) {
            // History is applied in causal order, including when different
            // replicas choose a different order for concurrent operations.
            for &id in history.iter().filter(|id| covers(context, **id)) {
                heads.retain(|head| !covers(&self.events[&id].context, *head));
                heads.insert(id);
            }
        }
        heads
    }
}

fn clock_at(clock: &VersionVector, peer: PeerId) -> u64 {
    clock.get(&peer).copied().unwrap_or_default()
}

fn covers(clock: &VersionVector, id: EventId) -> bool {
    clock_at(clock, id.peer) >= id.sequence
}
