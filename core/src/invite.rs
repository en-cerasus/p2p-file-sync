//! Signed, bearer-style invitations and device-bound admission certificates.
//! Transport and one-time redemption are separate from these wire formats.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::auth::Role;

const INVITE_DOMAIN: &[u8] = b"p2p-file-sync/invitation/v1\0";
const ADMISSION_DOMAIN: &[u8] = b"p2p-file-sync/admission/v1\0";
const MAX_WIRE: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Invitation {
    pub share: [u8; 16],
    pub generation: u64,
    pub authority: [u8; 32],
    pub role: Role,
    pub invite_id: [u8; 32],
    pub expires_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Admission {
    pub invitation: Invitation,
    pub device: [u8; 32],
    pub expires_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Signed<T> {
    payload: T,
    signature: Vec<u8>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum InviteError {
    InvalidFormat,
    WrongScope,
    Expired,
    BadSignature,
}

fn signature_bytes<T: Serialize>(domain: &[u8], payload: &T) -> Result<Vec<u8>, InviteError> {
    let mut bytes = domain.to_vec();
    bytes.extend(postcard::to_allocvec(payload).map_err(|_| InviteError::InvalidFormat)?);
    Ok(bytes)
}

fn seal<T: Serialize + Clone>(domain: &[u8], payload: &T, key: &SigningKey) -> Result<Vec<u8>, InviteError> {
    let signature = key.sign(&signature_bytes(domain, payload)?).to_bytes().to_vec();
    postcard::to_allocvec(&Signed { payload: payload.clone(), signature })
        .map_err(|_| InviteError::InvalidFormat)
}

fn open<T: for<'a> Deserialize<'a> + Serialize>(domain: &[u8], wire: &[u8], authority: [u8; 32]) -> Result<T, InviteError> {
    if wire.len() > MAX_WIRE { return Err(InviteError::InvalidFormat); }
    let signed: Signed<T> = postcard::from_bytes(wire).map_err(|_| InviteError::InvalidFormat)?;
    let canonical = postcard::to_allocvec(&signed).map_err(|_| InviteError::InvalidFormat)?;
    if canonical != wire { return Err(InviteError::InvalidFormat); }
    let key = VerifyingKey::from_bytes(&authority).map_err(|_| InviteError::BadSignature)?;
    let signature: [u8; 64] = signed.signature.try_into().map_err(|_| InviteError::InvalidFormat)?;
    key.verify(&signature_bytes(domain, &signed.payload)?, &Signature::from_bytes(&signature))
        .map_err(|_| InviteError::BadSignature)?;
    Ok(signed.payload)
}

impl Invitation {
    pub fn sign(&self, authority: &SigningKey) -> Result<Vec<u8>, InviteError> {
        if self.authority != authority.verifying_key().to_bytes() { return Err(InviteError::WrongScope); }
        seal(INVITE_DOMAIN, self, authority)
    }

    /// The expected authority must come from local trust, not the untrusted key text.
    pub fn verify(wire: &[u8], expected_authority: [u8; 32], share: [u8; 16], generation: u64, now: u64) -> Result<Self, InviteError> {
        let invite: Self = open(INVITE_DOMAIN, wire, expected_authority)?;
        if invite.authority != expected_authority || invite.share != share || invite.generation != generation { return Err(InviteError::WrongScope); }
        if invite.expires_at <= now { return Err(InviteError::Expired); }
        Ok(invite)
    }
}

impl Admission {
    pub fn sign(&self, authority: &SigningKey) -> Result<Vec<u8>, InviteError> {
        if self.invitation.authority != authority.verifying_key().to_bytes() || self.expires_at > self.invitation.expires_at { return Err(InviteError::WrongScope); }
        seal(ADMISSION_DOMAIN, self, authority)
    }

    pub fn verify(wire: &[u8], expected_authority: [u8; 32], share: [u8; 16], generation: u64, device: [u8; 32], now: u64) -> Result<Self, InviteError> {
        let admission: Self = open(ADMISSION_DOMAIN, wire, expected_authority)?;
        let invite = &admission.invitation;
        if invite.authority != expected_authority || invite.share != share || invite.generation != generation || admission.device != device || admission.expires_at > invite.expires_at { return Err(InviteError::WrongScope); }
        if admission.expires_at <= now { return Err(InviteError::Expired); }
        Ok(admission)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitation_and_admission_are_scoped_and_device_bound() {
        let admin = SigningKey::from_bytes(&[7; 32]);
        let authority = admin.verifying_key().to_bytes();
        let invite = Invitation { share: [2; 16], generation: 3, authority, role: Role::Reader, invite_id: [9; 32], expires_at: 100 };
        let wire = invite.sign(&admin).unwrap();
        assert_eq!(Invitation::verify(&wire, authority, [2; 16], 3, 99), Ok(invite.clone()));
        assert_eq!(Invitation::verify(&wire, authority, [2; 16], 4, 99), Err(InviteError::WrongScope));
        assert_eq!(Invitation::verify(&wire, authority, [2; 16], 3, 100), Err(InviteError::Expired));
        let mut tampered = wire.clone();
        tampered[10] ^= 1;
        assert!(Invitation::verify(&tampered, authority, [2; 16], 3, 99).is_err());

        let admission = Admission { invitation: invite, device: [4; 32], expires_at: 90 };
        let proof = admission.sign(&admin).unwrap();
        assert_eq!(Admission::verify(&proof, authority, [2; 16], 3, [4; 32], 89), Ok(admission));
        assert_eq!(Admission::verify(&proof, authority, [2; 16], 3, [5; 32], 89), Err(InviteError::WrongScope));
    }
}
