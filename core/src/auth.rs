use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use crate::{
    replica::{Apply, Event, PeerId},
    store::{decode, encode, Membership, Store, StoreError},
};

const DOMAIN: &[u8] = b"p2p-file-sync/event/v1\0";
const HEADER_BYTES: usize = 16 + 8 + 32;
const SIGNATURE_BYTES: usize = 64;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Reader,
    Writer,
}

/// Roster established by the local trust layer, for one share generation.
/// A peer ID is the byte encoding of its Ed25519 public key.
pub struct Roster {
    membership: Membership,
    roles: BTreeMap<PeerId, Role>,
}

#[derive(Debug)]
pub enum AuthError {
    InvalidFrame,
    WrongScope,
    NotWriter,
    BadSignature,
    InvalidEvent,
    Store(StoreError),
}

impl From<StoreError> for AuthError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl Roster {
    pub fn new(share: [u8; 16], generation: u64, roles: BTreeMap<PeerId, Role>) -> Self {
        let members = roles.keys().copied().collect();
        Self {
            membership: Membership {
                share,
                generation,
                members,
            },
            roles,
        }
    }

    pub fn membership(&self) -> &Membership {
        &self.membership
    }

    pub fn sign_event(&self, key: &SigningKey, event: &Event) -> Result<Vec<u8>, AuthError> {
        let author = key.verifying_key().to_bytes();
        self.require_writer(&author)?;
        if event.id.peer != author {
            return Err(AuthError::InvalidEvent);
        }
        let payload = encode(event)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(AuthError::InvalidFrame);
        }
        let mut wire = self.prefix(&author);
        wire.extend_from_slice(&payload);
        let signature = key.sign(&wire);
        wire.extend_from_slice(&signature.to_bytes());
        Ok(wire)
    }

    pub fn verify_event(&self, wire: &[u8]) -> Result<Event, AuthError> {
        let prefix_bytes = DOMAIN.len() + HEADER_BYTES;
        if wire.len() <= prefix_bytes + SIGNATURE_BYTES
            || wire.len() > prefix_bytes + SIGNATURE_BYTES + MAX_PAYLOAD_BYTES
            || !wire.starts_with(DOMAIN)
        {
            return Err(AuthError::InvalidFrame);
        }

        let signed_bytes = wire.len() - SIGNATURE_BYTES;
        let (message, signature) = wire.split_at(signed_bytes);
        let (scope, author) = message[DOMAIN.len()..prefix_bytes].split_at(24);
        if scope[..16] != self.membership.share
            || scope[16..] != self.membership.generation.to_be_bytes()
        {
            return Err(AuthError::WrongScope);
        }
        let author: &PeerId = author.try_into().map_err(|_| AuthError::InvalidFrame)?;
        self.require_writer(author)?;
        let key = VerifyingKey::from_bytes(author).map_err(|_| AuthError::BadSignature)?;
        let signature = Signature::from_slice(signature).map_err(|_| AuthError::BadSignature)?;
        key.verify_strict(message, &signature)
            .map_err(|_| AuthError::BadSignature)?;

        let payload = &message[prefix_bytes..];
        let event: Event = decode(payload).map_err(|_| AuthError::InvalidEvent)?;
        if event.id.peer != *author || encode(&event)? != payload {
            return Err(AuthError::InvalidEvent);
        }
        Ok(event)
    }

    pub fn receive(&self, store: &mut Store, wire: &[u8]) -> Result<Apply, AuthError> {
        if store.membership() != &self.membership {
            return Err(AuthError::WrongScope);
        }
        let event = self.verify_event(wire)?;
        Ok(store.apply_signed(event, wire.to_vec())?)
    }

    fn require_writer(&self, author: &PeerId) -> Result<(), AuthError> {
        match self.roles.get(author) {
            Some(Role::Writer) => Ok(()),
            _ => Err(AuthError::NotWriter),
        }
    }

    fn prefix(&self, author: &PeerId) -> Vec<u8> {
        let mut message = DOMAIN.to_vec();
        message.extend_from_slice(&self.membership.share);
        message.extend_from_slice(&self.membership.generation.to_be_bytes());
        message.extend_from_slice(author);
        message
    }
}

pub use ed25519_dalek::SigningKey as PeerSigningKey;
