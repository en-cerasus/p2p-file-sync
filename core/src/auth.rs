use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use crate::{
    replica::{Apply, Event, PeerId},
    retention::Confirmation,
    store::{decode, encode, Membership, Store, StoreError},
};

const DOMAIN: &[u8] = b"p2p-file-sync/event/v1\0";
const CONFIRMATION_DOMAIN: &[u8] = b"p2p-file-sync/confirmation/v1\0";
const RECOVERY_DOMAIN: &[u8] = b"p2p-file-sync/recovery-anchor/v1\0";
const RECOVERY_RESPONSE_DOMAIN: &[u8] = b"p2p-file-sync/recovery-response/v1\0";
const MEMBER_RESPONSE_DOMAIN: &[u8] = b"p2p-file-sync/member-response/v1\0";
const HEADER_BYTES: usize = 16 + 8 + 32;
const SIGNATURE_BYTES: usize = 64;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
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
    InvalidConfirmation,
    UnknownPeer,
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

    pub(crate) fn roles(&self) -> &BTreeMap<PeerId, Role> {
        &self.roles
    }

    pub(crate) fn is_reader(&self, peer: &PeerId) -> bool {
        self.roles.get(peer) == Some(&Role::Reader)
    }

    pub fn sign_event(&self, key: &SigningKey, event: &Event) -> Result<Vec<u8>, AuthError> {
        let author = key.verifying_key().to_bytes();
        self.check_writer(&author)?;
        if event.id.peer != author {
            return Err(AuthError::InvalidEvent);
        }
        let payload = encode(event)?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(AuthError::InvalidFrame);
        }
        Ok(self.sign_payload(DOMAIN, key, &author, &payload))
    }

    pub fn verify_event(&self, wire: &[u8]) -> Result<Event, AuthError> {
        let (author, payload) = self.verify_payload(DOMAIN, wire, MAX_PAYLOAD_BYTES)?;
        self.check_writer(author)?;
        let event: Event = decode(payload).map_err(|_| AuthError::InvalidEvent)?;
        if event.id.peer != *author || encode(&event)? != payload {
            return Err(AuthError::InvalidEvent);
        }
        Ok(event)
    }

    /// The caller must first durably reconcile local edits and apply the
    /// successor. Signing alone does not prove filesystem completion.
    pub fn sign_confirmation(
        &self,
        key: &SigningKey,
        confirmation: &Confirmation,
    ) -> Result<Vec<u8>, AuthError> {
        let author = key.verifying_key().to_bytes();
        self.check_member(&author)?;
        if confirmation.peer != author || confirmation.generation != self.membership.generation {
            return Err(AuthError::InvalidConfirmation);
        }
        Ok(self.sign_payload(CONFIRMATION_DOMAIN, key, &author, &encode(confirmation)?))
    }

    pub fn verify_confirmation(&self, wire: &[u8]) -> Result<Confirmation, AuthError> {
        let (author, payload) = self.verify_payload(CONFIRMATION_DOMAIN, wire, 128)?;
        let confirmation: Confirmation =
            decode(payload).map_err(|_| AuthError::InvalidConfirmation)?;
        if confirmation.peer != *author
            || confirmation.generation != self.membership.generation
            || encode(&confirmation)? != payload
        {
            return Err(AuthError::InvalidConfirmation);
        }
        Ok(confirmation)
    }

    pub fn receive_confirmation(&self, store: &mut Store, wire: &[u8]) -> Result<(), AuthError> {
        if store.membership() != &self.membership {
            return Err(AuthError::WrongScope);
        }
        let confirmation = self.verify_confirmation(wire)?;
        Ok(store.confirm_verified_signed(confirmation, wire)?)
    }

    pub fn receive(&self, store: &mut Store, wire: &[u8]) -> Result<Apply, AuthError> {
        if store.membership() != &self.membership {
            return Err(AuthError::WrongScope);
        }
        let event = self.verify_event(wire)?;
        Ok(store.apply_signed(event, wire.to_vec())?)
    }

    pub(crate) fn sign_recovery_payload(
        &self,
        key: &SigningKey,
        payload: &[u8],
    ) -> Result<Vec<u8>, AuthError> {
        let author = key.verifying_key().to_bytes();
        self.check_member(&author)?;
        if payload.is_empty() || payload.len() > MAX_PAYLOAD_BYTES {
            return Err(AuthError::InvalidFrame);
        }
        Ok(self.sign_payload(RECOVERY_DOMAIN, key, &author, payload))
    }

    pub(crate) fn verify_recovery_payload<'a>(
        &self,
        wire: &'a [u8],
    ) -> Result<(&'a PeerId, &'a [u8]), AuthError> {
        self.verify_payload(RECOVERY_DOMAIN, wire, MAX_PAYLOAD_BYTES)
    }

    pub(crate) fn sign_recovery_response(
        &self,
        key: &SigningKey,
        challenge: &[u8; 32],
        anchor: &[u8],
    ) -> Result<Vec<u8>, AuthError> {
        let author = key.verifying_key().to_bytes();
        self.check_member(&author)?;
        if anchor.is_empty() || anchor.len() > MAX_PAYLOAD_BYTES + 256 {
            return Err(AuthError::InvalidFrame);
        }
        let mut payload = Vec::with_capacity(32 + anchor.len());
        payload.extend_from_slice(challenge);
        payload.extend_from_slice(anchor);
        Ok(self.sign_payload(RECOVERY_RESPONSE_DOMAIN, key, &author, &payload))
    }

    pub(crate) fn verify_recovery_response<'a>(
        &self,
        wire: &'a [u8],
        challenge: &[u8; 32],
        expected: &PeerId,
    ) -> Result<&'a [u8], AuthError> {
        let (author, payload) = self.verify_payload(
            RECOVERY_RESPONSE_DOMAIN,
            wire,
            MAX_PAYLOAD_BYTES + 288,
        )?;
        if author != expected || payload.len() <= 32 || payload[..32] != challenge[..] {
            return Err(AuthError::InvalidFrame);
        }
        Ok(&payload[32..])
    }

    pub(crate) fn sign_member_response(
        &self,
        key: &SigningKey,
        challenge: &[u8; 32],
    ) -> Result<Vec<u8>, AuthError> {
        let author = key.verifying_key().to_bytes();
        self.check_member(&author)?;
        Ok(self.sign_payload(MEMBER_RESPONSE_DOMAIN, key, &author, challenge))
    }

    pub(crate) fn verify_member_response(
        &self,
        wire: &[u8],
        challenge: &[u8; 32],
        expected: &PeerId,
    ) -> Result<(), AuthError> {
        let (author, payload) = self.verify_payload(MEMBER_RESPONSE_DOMAIN, wire, 32)?;
        if author != expected || payload != challenge {
            return Err(AuthError::InvalidFrame);
        }
        Ok(())
    }

    pub(crate) fn check_writer(&self, author: &PeerId) -> Result<(), AuthError> {
        match self.roles.get(author) {
            Some(Role::Writer) => Ok(()),
            _ => Err(AuthError::NotWriter),
        }
    }

    fn check_member(&self, author: &PeerId) -> Result<(), AuthError> {
        if self.roles.contains_key(author) {
            Ok(())
        } else {
            Err(AuthError::UnknownPeer)
        }
    }

    fn sign_payload(
        &self,
        domain: &[u8],
        key: &SigningKey,
        author: &PeerId,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut message = domain.to_vec();
        message.extend_from_slice(&self.membership.share);
        message.extend_from_slice(&self.membership.generation.to_be_bytes());
        message.extend_from_slice(author);
        message.extend_from_slice(payload);
        let signature = key.sign(&message);
        message.extend_from_slice(&signature.to_bytes());
        message
    }

    fn verify_payload<'a>(
        &self,
        domain: &[u8],
        wire: &'a [u8],
        max_payload: usize,
    ) -> Result<(&'a PeerId, &'a [u8]), AuthError> {
        let prefix = domain.len() + HEADER_BYTES;
        if wire.len() <= prefix + SIGNATURE_BYTES
            || wire.len() > prefix + SIGNATURE_BYTES + max_payload
            || !wire.starts_with(domain)
        {
            return Err(AuthError::InvalidFrame);
        }
        let signed_bytes = wire.len() - SIGNATURE_BYTES;
        let (message, signature) = wire.split_at(signed_bytes);
        let (scope, author) = message[domain.len()..prefix].split_at(24);
        if scope[..16] != self.membership.share
            || scope[16..] != self.membership.generation.to_be_bytes()
        {
            return Err(AuthError::WrongScope);
        }
        let author: &PeerId = author.try_into().map_err(|_| AuthError::InvalidFrame)?;
        self.check_member(author)?;
        let key = VerifyingKey::from_bytes(author).map_err(|_| AuthError::BadSignature)?;
        let signature = Signature::from_slice(signature).map_err(|_| AuthError::BadSignature)?;
        key.verify_strict(message, &signature)
            .map_err(|_| AuthError::BadSignature)?;
        Ok((author, &message[prefix..]))
    }
}

pub use ed25519_dalek::SigningKey as PeerSigningKey;

#[cfg(test)]
mod recovery_response_tests {
    use super::*;

    #[test]
    fn signed_response_is_bound_to_this_request_challenge() {
        let key = SigningKey::from_bytes(&[31; 32]);
        let peer = key.verifying_key().to_bytes();
        let roster = Roster::new([4; 16], 1, [(peer, Role::Writer)].into());
        let response = roster.sign_recovery_response(&key, &[1; 32], b"signed anchor").unwrap();
        assert_eq!(roster.verify_recovery_response(&response, &[1; 32], &peer).unwrap(), b"signed anchor");
        assert!(matches!(roster.verify_recovery_response(&response, &[2; 32], &peer), Err(AuthError::InvalidFrame)));
        let mut tampered = response;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(matches!(roster.verify_recovery_response(&tampered, &[1; 32], &peer), Err(AuthError::BadSignature)));
    }
}

#[cfg(test)]
mod member_response_tests {
    use super::*;

    #[test]
    fn member_proof_rejects_old_identity_wrong_identity_and_replay() {
        let allowed = SigningKey::from_bytes(&[41; 32]);
        let other = SigningKey::from_bytes(&[42; 32]);
        let revoked = SigningKey::from_bytes(&[43; 32]);
        let revoked_writer = SigningKey::from_bytes(&[44; 32]);
        let roster = Roster::new(
            [9; 16],
            2,
            [
                (allowed.verifying_key().to_bytes(), Role::Reader),
                (other.verifying_key().to_bytes(), Role::Writer),
            ]
            .into(),
        );
        let challenge = [7; 32];
        let proof = roster.sign_member_response(&allowed, &challenge).unwrap();
        assert!(roster.verify_member_response(
            &proof, &challenge, &allowed.verifying_key().to_bytes()
        ).is_ok());
        assert!(matches!(
            roster.verify_member_response(&proof, &challenge, &other.verifying_key().to_bytes()),
            Err(AuthError::InvalidFrame)
        ));
        assert!(matches!(
            roster.verify_member_response(&proof, &[8; 32], &allowed.verifying_key().to_bytes()),
            Err(AuthError::InvalidFrame)
        ));
        assert!(matches!(
            roster.sign_member_response(&revoked, &challenge),
            Err(AuthError::UnknownPeer)
        ));
        let stale_roster = Roster::new(
            [9; 16], 2,
            [(revoked.verifying_key().to_bytes(), Role::Reader)].into(),
        );
        let stale_proof = stale_roster.sign_member_response(&revoked, &challenge).unwrap();
        assert!(matches!(
            roster.verify_member_response(&stale_proof, &challenge, &revoked.verifying_key().to_bytes()),
            Err(AuthError::UnknownPeer)
        ));
        let stale_writer_roster = Roster::new(
            [9; 16], 2,
            [(revoked_writer.verifying_key().to_bytes(), Role::Writer)].into(),
        );
        let stale_writer_proof = stale_writer_roster
            .sign_member_response(&revoked_writer, &challenge).unwrap();
        assert!(matches!(
            roster.verify_member_response(
                &stale_writer_proof, &challenge, &revoked_writer.verifying_key().to_bytes()
            ),
            Err(AuthError::UnknownPeer)
        ));
        let old = Roster::new(
            [9; 16], 1,
            [(allowed.verifying_key().to_bytes(), Role::Reader)].into(),
        );
        let old_proof = old.sign_member_response(&allowed, &challenge).unwrap();
        assert!(matches!(
            roster.verify_member_response(&old_proof, &challenge, &allowed.verifying_key().to_bytes()),
            Err(AuthError::WrongScope)
        ));
    }
}
