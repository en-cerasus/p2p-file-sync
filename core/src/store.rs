use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::SystemTime,
};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::{
    archive::{archives_on, record_archive},
    auth::Roster,
    generation::{CheckpointProtection, GenerationBaseline, GenerationCheckpoint},
    materialize::AppliedFile,
    replica::{
        Apply, Change, ContentHash, EntryKind, Event, EventId, ObjectId, PeerId, Reject, Replica,
        VersionVector,
    },
    retention::{Confirmation, Protection},
};

const SCHEMA_VERSION: u32 = 5;
const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024 + 256;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Membership {
    pub share: [u8; 16],
    pub generation: u64,
    pub members: BTreeSet<PeerId>,
}

#[derive(Debug)]
pub enum StoreError {
    Sql(rusqlite::Error),
    Io(std::io::Error),
    Decode(postcard::Error),
    UnsupportedSchema(u32),
    WrongMembership,
    WrongBaseline,
    NonMember,
    InvalidConfirmation,
    InvalidRecord,
    OversizedRecord,
    Rejected(Reject),
}

impl From<rusqlite::Error> for StoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sql(value)
    }
}

impl From<postcard::Error> for StoreError {
    fn from(value: postcard::Error) -> Self {
        Self::Decode(value)
    }
}

#[derive(Deserialize, Serialize)]
enum Record {
    Event(Event),
    Confirmation(Confirmation),
    SignedEvent { event: Event, wire: Vec<u8> },
}

#[derive(Default)]
struct Index {
    offset: i64,
    replica: Replica,
    event_order: Vec<EventId>,
    signed_events: BTreeMap<EventId, Vec<u8>>,
    protections: BTreeMap<(ObjectId, EventId, EventId), Protection>,
    baseline: Option<(VersionVector, Roster)>,
}

pub struct Store {
    pub(crate) connection: Connection,
    membership: Membership,
    index: Index,
}

impl Store {
    pub fn open(path: impl AsRef<Path>, membership: Membership) -> Result<Self, StoreError> {
        Self::open_internal(path.as_ref(), membership, None)
    }

    /// Bind this database to a verified baseline. Supply the same verified
    /// bundle on restart to reconstruct inherited history before replay.
    pub fn open_with_baseline(
        path: impl AsRef<Path>,
        baseline: GenerationBaseline,
    ) -> Result<Self, StoreError> {
        let (roster, checkpoint, replica, digest) = baseline.into_store_parts();
        if checkpoint.frontier != *replica.clock() {
            return Err(StoreError::WrongBaseline);
        }
        Self::open_internal(
            path.as_ref(),
            roster.membership().clone(),
            Some((checkpoint, replica, roster, digest)),
        )
    }

    fn open_internal(
        path: &Path,
        membership: Membership,
        baseline: Option<(GenerationCheckpoint, Replica, Roster, ContentHash)>,
    ) -> Result<Self, StoreError> {
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                tx.execute_batch(
                    "CREATE TABLE membership (id INTEGER PRIMARY KEY CHECK (id = 1), value BLOB NOT NULL);
                     CREATE TABLE journal (position INTEGER PRIMARY KEY, record BLOB NOT NULL);",
                )?;
                tx.execute(
                    "INSERT INTO membership (id, value) VALUES (1, ?1)",
                    [encode(&membership)?],
                )?;
            }
            1..=SCHEMA_VERSION => {
                let stored =
                    tx.query_row("SELECT value FROM membership WHERE id = 1", [], |row| {
                        decode_column(row, 0)
                    })?;
                if decode::<Membership>(&stored)? != membership {
                    return Err(StoreError::WrongMembership);
                }
            }
            _ => return Err(StoreError::UnsupportedSchema(version)),
        }

        if version < 2 {
            tx.execute_batch(
                "CREATE TABLE file_intent (
                    object BLOB PRIMARY KEY,
                    payload BLOB NOT NULL
                );",
            )?;
        }
        if version < 3 {
            tx.execute_batch(
                "ALTER TABLE file_intent ADD COLUMN path BLOB;
                 CREATE TABLE applied_file (
                    object BLOB PRIMARY KEY,
                    payload BLOB NOT NULL
                 );",
            )?;
        }
        if version < 4 {
            tx.execute_batch(
                "CREATE TABLE file_archive (
                    id INTEGER PRIMARY KEY,
                    object BLOB NOT NULL,
                    recorded_at INTEGER NOT NULL,
                    payload BLOB NOT NULL
                );
                CREATE INDEX file_archive_expiry ON file_archive(recorded_at);",
            )?;
        }
        if version < 5 {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS generation_baseline (
                    id INTEGER PRIMARY KEY CHECK (id = 1), digest BLOB NOT NULL
                );",
            )?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }

        let stored: Option<Vec<u8>> = tx
            .query_row(
                "SELECT digest FROM generation_baseline WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let mut index = Index::default();
        match (stored, baseline) {
            (None, None) => {}
            (stored, Some((checkpoint, replica, roster, digest))) => {
                if let Some(stored) = stored {
                    if stored != digest {
                        return Err(StoreError::WrongBaseline);
                    }
                } else {
                    let has_events: bool = tx.query_row(
                        "SELECT EXISTS (SELECT 1 FROM journal)
                             OR EXISTS (SELECT 1 FROM file_intent)
                             OR EXISTS (SELECT 1 FROM applied_file)
                             OR EXISTS (SELECT 1 FROM file_archive)",
                        [],
                        |row| row.get(0),
                    )?;
                    if has_events {
                        return Err(StoreError::WrongBaseline);
                    }
                    tx.execute(
                        "INSERT INTO generation_baseline VALUES (1, ?1)",
                        [digest.as_slice()],
                    )?;
                    for archive in &checkpoint.archives {
                        tx.execute(
                            "INSERT INTO file_archive (object, recorded_at, payload) VALUES (?1, ?2, ?3)",
                            params![archive.object.as_slice(), archive.archived_at, encode(archive)?],
                        )?;
                    }
                }
                index = Index::from_baseline(checkpoint, replica, roster)?;
            }
            (Some(_), None) => return Err(StoreError::WrongBaseline),
        }

        index.refresh(&tx, &membership)?;
        tx.commit()?;
        Ok(Self {
            connection,
            membership,
            index,
        })
    }

    pub(crate) fn membership(&self) -> &Membership {
        &self.membership
    }

    pub(crate) fn event_order(&self) -> &[EventId] {
        &self.index.event_order
    }

    pub(crate) fn has_signed_content(&self, hash: ContentHash) -> bool {
        self.index.signed_events.keys().any(|&id| {
            self.index
                .replica
                .get(id)
                .and_then(|event| event.change.entry())
                .is_some_and(|entry| entry.kind == EntryKind::File(hash))
        })
    }

    /// Original signed event for forwarding to another peer.
    pub fn signed_event(&self, id: EventId) -> Option<&[u8]> {
        self.index.signed_events.get(&id).map(Vec::as_slice)
    }

    pub fn replica(&self) -> &Replica {
        &self.index.replica
    }

    pub fn protections(&self) -> impl Iterator<Item = &Protection> {
        self.index.protections.values()
    }

    /// Read commits made by another handle before querying this handle's index.
    pub fn refresh(&mut self) -> Result<(), StoreError> {
        self.index.refresh(&self.connection, &self.membership)
    }

    pub fn generation_checkpoint(
        &mut self,
        now: SystemTime,
    ) -> Result<GenerationCheckpoint, StoreError> {
        // Exporting a second transition will require a chain of checkpoints.
        if self.index.baseline.is_some() {
            return Err(StoreError::WrongBaseline);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.index.refresh(&tx, &self.membership)?;
        let replica = &self.index.replica;
        let mut events: Vec<_> = self
            .index
            .event_order
            .iter()
            .map(|&id| replica.get(id).cloned().ok_or(StoreError::InvalidRecord))
            .collect::<Result<_, _>>()?;
        events.sort_unstable_by_key(|event| event.id);
        let signed_events = events
            .iter()
            .map(|event| {
                self.index
                    .signed_events
                    .get(&event.id)
                    .cloned()
                    .ok_or(StoreError::InvalidRecord)
            })
            .collect::<Result<_, _>>()?;
        let mut heads: Vec<_> = replica
            .objects()
            .flat_map(|object| replica.heads(object).map(|event| event.id))
            .collect();
        heads.sort_unstable();
        let mut protected: Vec<_> = self
            .index
            .protections
            .values()
            .filter(|protection| protection.is_active(replica))
            .map(|protection| CheckpointProtection {
                original: protection.original(),
                successor: protection.successor(),
                hash: protection.content(),
            })
            .collect();
        protected.sort_unstable();
        Ok(GenerationCheckpoint {
            version: GenerationCheckpoint::VERSION,
            share: self.membership.share,
            generation: self.membership.generation,
            frontier: replica.clock().clone(),
            events,
            signed_events,
            heads,
            protected,
            archives: archives_on(&tx, now)?,
        })
    }

    pub(crate) fn begin_indexed_write(
        &mut self,
    ) -> Result<(rusqlite::Transaction<'_>, &Replica), StoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.index.refresh(&tx, &self.membership)?;
        Ok((tx, &self.index.replica))
    }

    /// Accepts values only after the caller verifies their authentication.
    /// Missing dependencies leave the event uncommitted for the caller to retry.
    pub fn apply_verified(&mut self, event: Event) -> Result<Apply, StoreError> {
        self.apply_record(event, None, None)
    }

    pub(crate) fn apply_signed(
        &mut self,
        event: Event,
        wire: Vec<u8>,
    ) -> Result<Apply, StoreError> {
        self.apply_record(event, Some(wire), None)
    }

    /// Commit a locally authored revision of the published snapshot together
    /// with its history and the publishing peer's own confirmation.
    pub(crate) fn apply_publication(
        &mut self,
        event: Event,
        wire: Vec<u8>,
        previous: AppliedFile,
    ) -> Result<Apply, StoreError> {
        self.apply_record(event, Some(wire), Some(previous))
    }

    fn apply_record(
        &mut self,
        event: Event,
        wire: Option<Vec<u8>>,
        previous: Option<AppliedFile>,
    ) -> Result<Apply, StoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.index.refresh(&tx, &self.membership)?;
        self.index
            .validate_scope(&event, &self.membership, wire.as_deref())?;
        let outcome = self
            .index
            .replica
            .validate(&event)
            .map_err(StoreError::Rejected)?;
        if outcome == Apply::Added {
            let confirmation = if let Some(previous) = previous {
                let desired = match &event.change {
                    Change::Revise { base, entry } if *base == previous.event => {
                        match entry.as_ref().map(|entry| &entry.kind) {
                            Some(EntryKind::File(hash)) => Some(*hash),
                            None => None,
                            Some(EntryKind::Directory) => return Err(StoreError::InvalidRecord),
                        }
                    }
                    _ => return Err(StoreError::InvalidRecord),
                };
                record_archive(&tx, event.object, previous, event.id, desired)?;
                Some(Record::Confirmation(Confirmation {
                    generation: self.membership.generation,
                    peer: event.id.peer,
                    object: event.object,
                    successor: event.id,
                    sent_through: event.id.sequence,
                }))
            } else {
                None
            };
            let record = match wire {
                Some(wire) => Record::SignedEvent { event, wire },
                None => Record::Event(event),
            };
            let mut position = append(&tx, &record)?;
            if let Some(confirmation) = &confirmation {
                position = append(&tx, confirmation)?;
            }
            tx.commit()?;
            self.index.record(record, &self.membership)?;
            if let Some(confirmation) = confirmation {
                self.index.record(confirmation, &self.membership)?;
            }
            self.index.offset = position;
        }
        Ok(outcome)
    }

    /// A single verified acknowledgement may advance several original
    /// versions superseded by the same resolution.
    pub fn confirm_verified(&mut self, confirmation: Confirmation) -> Result<(), StoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.index.refresh(&tx, &self.membership)?;
        self.index
            .validate_confirmation(confirmation, &self.membership)?;
        if self
            .index
            .protections
            .range(
                (confirmation.object, confirmation.successor, FIRST_EVENT)
                    ..=(confirmation.object, confirmation.successor, LAST_EVENT),
            )
            .all(|(_, protection)| {
                protection
                    .confirmed_through(confirmation.peer)
                    .is_some_and(|through| through >= confirmation.sent_through)
            })
        {
            return Ok(());
        }

        let record = Record::Confirmation(confirmation);
        let position = append(&tx, &record)?;
        tx.commit()?;
        self.index.record(record, &self.membership)?;
        self.index.offset = position;
        Ok(())
    }
}

impl Index {
    fn from_baseline(
        checkpoint: GenerationCheckpoint,
        replica: Replica,
        roster: Roster,
    ) -> Result<Self, StoreError> {
        let membership = roster.membership();
        let mut protections = BTreeMap::new();
        for reference in checkpoint.protected {
            let object = replica
                .get(reference.original)
                .ok_or(StoreError::InvalidRecord)?
                .object;
            protections.insert(
                (object, reference.successor, reference.original),
                Protection::new(
                    membership.generation,
                    object,
                    reference.original,
                    reference.successor,
                    reference.hash,
                    &membership.members,
                ),
            );
        }
        Ok(Self {
            replica,
            signed_events: checkpoint
                .events
                .into_iter()
                .map(|event| event.id)
                .zip(checkpoint.signed_events)
                .collect(),
            protections,
            baseline: Some((checkpoint.frontier, roster)),
            ..Self::default()
        })
    }

    fn validate_scope(
        &self,
        event: &Event,
        membership: &Membership,
        signed: Option<&[u8]>,
    ) -> Result<(), StoreError> {
        if !membership.members.contains(&event.id.peer) {
            return Err(StoreError::NonMember);
        }
        if let Some((frontier, roster)) = &self.baseline {
            if signed
                .and_then(|wire| roster.verify_event(wire).ok())
                .as_ref()
                != Some(event)
                || frontier
                    .iter()
                    .any(|(peer, sequence)| event.context.get(peer).unwrap_or(&0) < sequence)
            {
                return Err(StoreError::WrongBaseline);
            }
        }
        if !event.context.iter().all(|(peer, sequence)| {
            membership.members.contains(peer)
                || self
                    .baseline
                    .as_ref()
                    .and_then(|(frontier, _)| frontier.get(peer))
                    == Some(sequence)
        }) {
            return Err(StoreError::NonMember);
        }
        Ok(())
    }
    fn refresh(
        &mut self,
        connection: &Connection,
        membership: &Membership,
    ) -> Result<(), StoreError> {
        let mut statement = connection.prepare(
            "SELECT position, record FROM journal WHERE position > ?1 ORDER BY position",
        )?;
        let mut rows = statement.query([self.offset])?;
        while let Some(row) = rows.next()? {
            let position: i64 = row.get(0)?;
            if self.offset.checked_add(1) != Some(position) {
                return Err(StoreError::InvalidRecord);
            }
            let record = decode::<Record>(&decode_column(row, 1)?)?;
            self.record(record, membership)?;
            self.offset = position;
        }
        Ok(())
    }

    fn record(&mut self, record: Record, membership: &Membership) -> Result<(), StoreError> {
        let (record, signature) = match record {
            Record::SignedEvent { event, wire } => {
                let id = event.id;
                (Record::Event(event), Some((id, wire)))
            }
            record => (record, None),
        };
        match record {
            Record::SignedEvent { .. } => unreachable!(),
            Record::Event(event) => {
                self.validate_scope(
                    &event,
                    membership,
                    signature.as_ref().map(|(_, wire)| wire.as_slice()),
                )?;
                if self.replica.validate(&event) != Ok(Apply::Added) {
                    return Err(StoreError::InvalidRecord);
                }

                match &event.change {
                    Change::Create(_) => {}
                    Change::Revise { base, .. } => self.protect(*base, &event, membership),
                    Change::Resolve { bases, .. } => {
                        for &base in bases {
                            self.protect(base, &event, membership);
                        }
                    }
                }
                self.event_order.push(event.id);
                self.replica.record(event);
                if let Some((id, wire)) = signature {
                    self.signed_events.insert(id, wire);
                }
            }
            Record::Confirmation(ack) => {
                self.validate_confirmation(ack, membership)?;
                for ((_, successor, _), protection) in self.protections.range_mut(
                    (ack.object, ack.successor, FIRST_EVENT)
                        ..=(ack.object, ack.successor, LAST_EVENT),
                ) {
                    debug_assert_eq!(*successor, ack.successor);
                    protection.confirm(ack);
                }
            }
        }
        Ok(())
    }

    fn validate_confirmation(
        &self,
        ack: Confirmation,
        membership: &Membership,
    ) -> Result<(), StoreError> {
        if ack.generation != membership.generation
            || !membership.members.contains(&ack.peer)
            || (ack.peer == ack.successor.peer && ack.sent_through < ack.successor.sequence)
            || self
                .protections
                .range(
                    (ack.object, ack.successor, FIRST_EVENT)
                        ..=(ack.object, ack.successor, LAST_EVENT),
                )
                .next()
                .is_none()
        {
            return Err(StoreError::InvalidConfirmation);
        }
        Ok(())
    }

    fn protect(&mut self, base: EventId, successor: &Event, membership: &Membership) {
        if let Some(EntryKind::File(hash)) = self
            .replica
            .get(base)
            .and_then(|event| event.change.entry())
            .map(|entry| &entry.kind)
        {
            if successor
                .change
                .entry()
                .is_some_and(|entry| entry.kind == EntryKind::File(*hash))
            {
                return;
            }
            self.protections.insert(
                (successor.object, successor.id, base),
                Protection::new(
                    membership.generation,
                    successor.object,
                    base,
                    successor.id,
                    *hash,
                    &membership.members,
                ),
            );
        }
    }
}

const FIRST_EVENT: EventId = EventId {
    peer: [0; 32],
    sequence: 0,
};
const LAST_EVENT: EventId = EventId {
    peer: [255; 32],
    sequence: u64::MAX,
};

fn append(connection: &Connection, record: &Record) -> Result<i64, StoreError> {
    connection.execute(
        "INSERT INTO journal (record) VALUES (?1)",
        params![encode(record)?],
    )?;
    Ok(connection.last_insert_rowid())
}

pub(crate) fn encode(value: &impl Serialize) -> Result<Vec<u8>, StoreError> {
    let bytes = postcard::to_allocvec(value)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(StoreError::OversizedRecord);
    }
    Ok(bytes)
}

pub(crate) fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, StoreError> {
    let (record, rest) = postcard::take_from_bytes(bytes)?;
    if !rest.is_empty() {
        return Err(StoreError::InvalidRecord);
    }
    Ok(record)
}

pub(crate) fn decode_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Vec<u8>> {
    let column = row.get_ref(index)?;
    let value = column.as_blob().map_err(|_| {
        rusqlite::Error::InvalidColumnType(index, "record".into(), column.data_type())
    })?;
    if value.len() > MAX_RECORD_BYTES {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(value.to_vec())
}
