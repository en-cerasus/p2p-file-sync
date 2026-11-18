//! An administrative authorization for an explicit share-generation transition.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{
    archive::ArchivedFile,
    auth::Role,
    replica::{ContentHash, Event, EventId, PeerId, VersionVector},
};

const DOMAIN: &[u8] = b"p2p-file-sync/generation-transition/v1\0";
const MAX_MEMBERS: usize = 1024;
const MAX_MESSAGE: usize = 48 * 1024;
const CHECKPOINT_VERSION: u8 = 1;

/// Metadata captured from one consistent view of the old generation.
/// Earlier events provide the names and bases of deleted or moved objects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GenerationCheckpoint {
    pub(crate) version: u8,
    pub share: [u8; 16],
    pub generation: u64,
    pub frontier: VersionVector,
    pub events: Vec<Event>,
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
}

impl GenerationTransition {
    /// Match the exact bytes to the digest before decoding, then verify scope.
    pub fn verify_checkpoint(&self, bytes: &[u8]) -> Result<GenerationCheckpoint, TransitionError> {
        if crate::sha256(bytes) != self.checkpoint {
            return Err(TransitionError::WrongCheckpoint);
        }
        let (checkpoint, trailing) = postcard::take_from_bytes::<GenerationCheckpoint>(bytes)
            .map_err(|_| TransitionError::Invalid)?;
        if !trailing.is_empty() || checkpoint.version != CHECKPOINT_VERSION {
            return Err(TransitionError::Invalid);
        }
        if checkpoint.share != self.share || checkpoint.generation != self.previous {
            return Err(TransitionError::WrongScope);
        }
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
        if wire.len() > MAX_MESSAGE {
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
