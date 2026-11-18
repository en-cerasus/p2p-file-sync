use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::{
    replica::{Apply, Change, EntryKind, Event, EventId, ObjectId, PeerId, Reject, Replica},
    retention::{Confirmation, Protection},
};

const SCHEMA_VERSION: u32 = 4;
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
    Decode(postcard::Error),
    UnsupportedSchema(u32),
    WrongMembership,
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
}

pub struct Store {
    pub(crate) connection: Connection,
    membership: Membership,
    index: Index,
}

impl Store {
    pub fn open(path: impl AsRef<Path>, membership: Membership) -> Result<Self, StoreError> {
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
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }

        let mut index = Index::default();
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
        self.apply_record(event, None)
    }

    pub(crate) fn apply_signed(
        &mut self,
        event: Event,
        wire: Vec<u8>,
    ) -> Result<Apply, StoreError> {
        self.apply_record(event, Some(wire))
    }

    fn apply_record(&mut self, event: Event, wire: Option<Vec<u8>>) -> Result<Apply, StoreError> {
        if !self.membership.members.contains(&event.id.peer)
            || !event
                .context
                .keys()
                .all(|peer| self.membership.members.contains(peer))
        {
            return Err(StoreError::NonMember);
        }

        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.index.refresh(&tx, &self.membership)?;
        let outcome = self
            .index
            .replica
            .validate(&event)
            .map_err(StoreError::Rejected)?;
        if outcome == Apply::Added {
            let record = match wire {
                Some(wire) => Record::SignedEvent { event, wire },
                None => Record::Event(event),
            };
            let position = append(&tx, &record)?;
            tx.commit()?;
            self.index.record(record, &self.membership)?;
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
                if !membership.members.contains(&event.id.peer)
                    || !event
                        .context
                        .keys()
                        .all(|peer| membership.members.contains(peer))
                    || self.replica.validate(&event) != Ok(Apply::Added)
                {
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
