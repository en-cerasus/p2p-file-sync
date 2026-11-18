//! An administrative authorization for an explicit share-generation transition.

use std::collections::BTreeMap;
use std::io;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{
    archive::ArchivedFile,
    auth::{Role, Roster},
    blob::{Blob, BlobStore},
    replica::{Apply, ContentHash, EntryKind, Event, EventId, PeerId, Replica, VersionVector},
};

const DOMAIN: &[u8] = b"p2p-file-sync/generation-transition/v1\0";
const MAX_MEMBERS: usize = 1024;
pub(crate) const MAX_TRANSITION_BYTES: usize = 48 * 1024;
const CHECKPOINT_VERSION: u8 = 2;

/// Metadata captured from one consistent view of the old generation.
/// Earlier events provide the names and bases of deleted or moved objects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GenerationCheckpoint {
    pub(crate) version: u8,
    pub share: [u8; 16],
    pub generation: u64,
    pub frontier: VersionVector,
    pub events: Vec<Event>,
    pub(crate) signed_events: Vec<Vec<u8>>,
    pub heads: Vec<EventId>,
    pub protected: Vec<CheckpointProtection>,
    pub archives: Vec<ArchivedFile>,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CheckpointProtection {
    pub original: EventId,
    pub successor: EventId,
    pub hash: ContentHash,
}

impl GenerationCheckpoint {
    pub(crate) const VERSION: u8 = CHECKPOINT_VERSION;

    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Validate all required blobs without modifying the bundle.
    pub fn verify_contents(&self, store: &BlobStore) -> Result<usize, CheckpointDataError> {
        let contents = self.required_contents()?;
        for (&hash, &length) in &contents {
            let error = |source| CheckpointDataError::Content { hash, source };
            let file = store.open_candidate(hash).map_err(error)?;
            let blob = content_metadata(&file, hash, length).map_err(error)?;
            drop(file);
            store.open_verified(blob).map_err(error)?;
        }
        Ok(contents.len())
    }

    fn verify_history(&self, roster: &Roster) -> Result<(), TransitionError> {
        if self.events.len() != self.signed_events.len() {
            return Err(TransitionError::InvalidHistory);
        }
        let mut ordered = Vec::with_capacity(self.events.len());
        for (event, wire) in self.events.iter().zip(&self.signed_events) {
            if roster.verify_event(wire).ok().as_ref() != Some(event)
                || !event
                    .context
                    .keys()
                    .all(|peer| roster.membership().members.contains(peer))
            {
                return Err(TransitionError::InvalidHistory);
            }
            // Each valid event's context covers the event IDs it depends on.
            // Its sum is therefore strictly greater than their context sums.
            let rank: u128 = event
                .context
                .values()
                .map(|&counter| u128::from(counter))
                .sum();
            ordered.push((rank, event));
        }
        ordered.sort_unstable_by_key(|(rank, event)| (*rank, event.id));
        let mut replica = Replica::default();
        for (_, event) in ordered {
            if replica.apply(event.clone()) != Ok(Apply::Added) {
                return Err(TransitionError::InvalidHistory);
            }
        }
        let mut heads: Vec<_> = replica
            .objects()
            .flat_map(|object| replica.heads(object).map(|event| event.id))
            .collect();
        heads.sort_unstable();
        if self.frontier != *replica.clock()
            || self.heads != heads
            || self.required_contents().is_err()
        {
            return Err(TransitionError::InvalidHistory);
        }
        Ok(())
    }

    /// Copy the unique live, protected and archived contents of a verified
    /// checkpoint. Partial results can be reused, but do not activate it.
    pub fn stage_contents(
        &self,
        source: &BlobStore,
        destination: &BlobStore,
    ) -> Result<usize, CheckpointDataError> {
        let contents = self.required_contents()?;
        for (&hash, &length) in &contents {
            let error = |source| CheckpointDataError::Content { hash, source };
            match destination.open_candidate(hash) {
                Ok(file) => {
                    let blob = content_metadata(&file, hash, length).map_err(error)?;
                    drop(file);
                    destination.open_verified(blob).map_err(error)?;
                    continue;
                }
                Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
                Err(other) => return Err(error(other)),
            }
            let file = source.open_candidate(hash).map_err(error)?;
            let blob = content_metadata(&file, hash, length).map_err(error)?;
            destination.put(blob, file).map_err(error)?;
        }
        Ok(contents.len())
    }

    fn required_contents(&self) -> Result<BTreeMap<ContentHash, Option<u64>>, CheckpointDataError> {
        let events: BTreeMap<_, _> = self.events.iter().map(|event| (event.id, event)).collect();
        if events.len() != self.events.len() {
            return Err(CheckpointDataError::InvalidReference);
        }
        let event = |id| {
            events
                .get(&id)
                .copied()
                .ok_or(CheckpointDataError::InvalidReference)
        };
        let file_hash = |event: &Event| match event.change.entry()?.kind {
            EntryKind::File(hash) => Some(hash),
            EntryKind::Directory => None,
        };
        let mut contents = BTreeMap::new();
        for &head in &self.heads {
            if let Some(hash) = file_hash(event(head)?) {
                contents.insert(hash, None);
            }
        }
        for reference in &self.protected {
            let original = event(reference.original)?;
            if original.object != event(reference.successor)?.object
                || file_hash(original) != Some(reference.hash)
            {
                return Err(CheckpointDataError::InvalidReference);
            }
            contents.insert(reference.hash, None);
        }
        for archive in &self.archives {
            let original = event(archive.event)?;
            let blob = archive.content;
            if original.object != archive.object
                || event(archive.replaced_by)?.object != archive.object
                || file_hash(original) != Some(blob.hash)
                || contents
                    .insert(blob.hash, Some(blob.length))
                    .flatten()
                    .is_some_and(|previous| previous != blob.length)
            {
                return Err(CheckpointDataError::InvalidReference);
            }
        }
        Ok(contents)
    }
}

#[derive(Debug)]
pub enum CheckpointDataError {
    InvalidReference,
    Content {
        hash: ContentHash,
        source: io::Error,
    },
}

fn content_metadata(
    file: &std::fs::File,
    hash: ContentHash,
    length: Option<u64>,
) -> io::Result<Blob> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || length.is_some_and(|length| metadata.len() != length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid checkpoint blob",
        ));
    }
    Ok(Blob {
        hash,
        length: metadata.len(),
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GenerationTransition {
    pub share: [u8; 16],
    pub previous: u64,
    pub next: u64,
    /// A digest of the migration checkpoint to be verified before activation.
    pub checkpoint: ContentHash,
    pub members: BTreeMap<PeerId, Role>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum TransitionError {
    Invalid,
    WrongScope,
    BadSignature,
    WrongCheckpoint,
    InvalidHistory,
}

impl GenerationTransition {
    /// Match the exact bytes to the digest before decoding, then verify scope.
    pub fn verify_checkpoint(
        &self,
        bytes: &[u8],
        old_roster: &Roster,
    ) -> Result<GenerationCheckpoint, TransitionError> {
        if crate::sha256(bytes) != self.checkpoint {
            return Err(TransitionError::WrongCheckpoint);
        }
        let (checkpoint, trailing) = postcard::take_from_bytes::<GenerationCheckpoint>(bytes)
            .map_err(|_| TransitionError::Invalid)?;
        if !trailing.is_empty() || checkpoint.version != CHECKPOINT_VERSION {
            return Err(TransitionError::Invalid);
        }
        if checkpoint.share != self.share
            || checkpoint.generation != self.previous
            || old_roster.membership().share != self.share
            || old_roster.membership().generation != self.previous
        {
            return Err(TransitionError::WrongScope);
        }
        checkpoint.verify_history(old_roster)?;
        Ok(checkpoint)
    }

    /// Use a separately trusted administrative key, rather than a sync writer key.
    pub fn sign(&self, administrator: &SigningKey) -> Result<Vec<u8>, TransitionError> {
        self.validate()?;
        let payload = postcard::to_allocvec(self).map_err(|_| TransitionError::Invalid)?;
        let mut wire = administrator
            .sign(&signed_payload(&payload))
            .to_bytes()
            .to_vec();
        wire.extend(payload);
        Ok(wire)
    }

    /// Validate the transition against the locally trusted current generation.
    /// This verifies authorization. Activation must verify the referenced checkpoint.
    pub fn verify(
        wire: &[u8],
        administrator: &VerifyingKey,
        share: [u8; 16],
        current: u64,
    ) -> Result<Self, TransitionError> {
        if wire.len() > MAX_TRANSITION_BYTES {
            return Err(TransitionError::Invalid);
        }
        let (signature, payload) = wire
            .split_first_chunk::<64>()
            .ok_or(TransitionError::Invalid)?;
        administrator
            .verify_strict(&signed_payload(payload), &Signature::from_bytes(signature))
            .map_err(|_| TransitionError::BadSignature)?;
        let (transition, rest) =
            postcard::take_from_bytes::<Self>(payload).map_err(|_| TransitionError::Invalid)?;
        let canonical = postcard::to_allocvec(&transition).map_err(|_| TransitionError::Invalid)?;
        if !rest.is_empty() || canonical != payload {
            return Err(TransitionError::Invalid);
        }
        transition.validate()?;
        if transition.share != share || transition.previous != current {
            return Err(TransitionError::WrongScope);
        }
        Ok(transition)
    }

    fn validate(&self) -> Result<(), TransitionError> {
        if self.previous == 0
            || self.previous.checked_add(1) != Some(self.next)
            || self.members.len() > MAX_MEMBERS
            || !self.members.values().any(|role| *role == Role::Writer)
            || self
                .members
                .keys()
                .any(|peer| VerifyingKey::from_bytes(peer).map_or(true, |key| key.is_weak()))
        {
            return Err(TransitionError::Invalid);
        }
        Ok(())
    }
}

fn signed_payload(payload: &[u8]) -> Vec<u8> {
    [DOMAIN, payload].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_authenticates_scope_checkpoint_and_membership() {
        let admin = SigningKey::from_bytes(&[1; 32]);
        let writer = SigningKey::from_bytes(&[2; 32]);
        let reader = SigningKey::from_bytes(&[3; 32]);
        let transition = GenerationTransition {
            share: [12; 16],
            previous: 9,
            next: 10,
            checkpoint: crate::sha256(b"checkpoint"),
            members: [
                (writer.verifying_key().to_bytes(), Role::Writer),
                (reader.verifying_key().to_bytes(), Role::Reader),
            ]
            .into(),
        };
        let mut wire = transition.sign(&admin).unwrap();
        let verify = |wire: &[u8], share, generation| {
            GenerationTransition::verify(wire, &admin.verifying_key(), share, generation)
        };
        assert_eq!(verify(&wire, transition.share, 9), Ok(transition.clone()));
        assert_eq!(
            verify(&wire, transition.share, 10),
            Err(TransitionError::WrongScope)
        );
        assert_eq!(verify(&wire, [13; 16], 9), Err(TransitionError::WrongScope));
        assert_eq!(
            GenerationTransition::verify(&wire, &writer.verifying_key(), transition.share, 9),
            Err(TransitionError::BadSignature)
        );
        *wire.last_mut().unwrap() ^= 1;
        assert_eq!(
            verify(&wire, transition.share, 9),
            Err(TransitionError::BadSignature)
        );
        let mut invalid = transition;
        invalid.next = 11;
        assert_eq!(invalid.sign(&admin), Err(TransitionError::Invalid));
        invalid.next = 10;
        invalid.members.retain(|_, role| *role == Role::Reader);
        assert_eq!(invalid.sign(&admin), Err(TransitionError::Invalid));
    }
}
