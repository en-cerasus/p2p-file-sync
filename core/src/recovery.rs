//! A peer-held, signed high-water mark for detecting rollback of both the
//! database and its local witness directory. This is a verification primitive;
//! the host must obtain the anchor from an independent trusted peer before
//! allowing a recovered database to publish local edits.

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    auth::{AuthError, Roster},
    replica::{EventId, PeerId},
    store::{decode, encode, Store, StoreError},
};

const HASH_DOMAIN: &[u8] = b"p2p-file-sync/recovery-prefix/v1\0";

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
struct Prefix {
    peer: PeerId,
    sequence: u64,
    digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
struct Anchor {
    prefixes: Vec<Prefix>,
}

#[derive(Debug)]
pub enum RecoveryError {
    Auth(AuthError),
    Store(StoreError),
    WrongScope,
    UntrustedSigner,
    InvalidAnchor,
    MissingSignedEvent(EventId),
    Behind(EventId),
    RemoteBehind(EventId),
    StaleAnchor(EventId),
    Diverged(EventId),
}

impl From<AuthError> for RecoveryError {
    fn from(value: AuthError) -> Self {
        Self::Auth(value)
    }
}

impl From<StoreError> for RecoveryError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

/// Sign the current durable event prefixes. The resulting bytes must be held
/// by a different trusted peer or outside the restorable local state.
pub fn sign_anchor(
    roster: &Roster,
    store: &mut Store,
    key: &SigningKey,
) -> Result<Vec<u8>, RecoveryError> {
    if store.membership() != roster.membership() {
        return Err(RecoveryError::WrongScope);
    }
    store.refresh()?;
    let prefixes = store
        .replica()
        .clock()
        .iter()
        .map(|(&peer, &sequence)| {
            Ok(Prefix {
                peer,
                sequence,
                digest: prefix_digest(store, roster, peer, sequence)?,
            })
        })
        .collect::<Result<Vec<_>, RecoveryError>>()?;
    Ok(roster.sign_recovery_payload(key, &encode(&Anchor { prefixes })?)?)
}

/// Compare local history with a signed anchor supplied by the named trusted
/// peer. A matching prefix may have additional later events locally.
pub fn verify_anchor(
    roster: &Roster,
    store: &mut Store,
    wire: &[u8],
    trusted_peer: PeerId,
) -> Result<(), RecoveryError> {
    if store.membership() != roster.membership() {
        return Err(RecoveryError::WrongScope);
    }
    let anchor = parse_signed_anchor(roster, wire, trusted_peer)?;
    store.refresh()?;
    for prefix in anchor.prefixes {
        let id = EventId {
            peer: prefix.peer,
            sequence: prefix.sequence,
        };
        if store
            .replica()
            .clock()
            .get(&prefix.peer)
            .copied()
            .unwrap_or(0)
            < prefix.sequence
        {
            return Err(RecoveryError::Behind(id));
        }
        if prefix_digest(store, roster, prefix.peer, prefix.sequence)? != prefix.digest {
            return Err(RecoveryError::Diverged(id));
        }
    }
    Ok(())
}

/// Authenticate an anchor received over a peer connection before storing or
/// forwarding it. This does not claim the local history has caught up.
pub fn authenticate_anchor(
    roster: &Roster,
    wire: &[u8],
    trusted_peer: PeerId,
) -> Result<(), RecoveryError> {
    parse_signed_anchor(roster, wire, trusted_peer).map(|_| ())
}

/// A peer must retain every event it authored. Compare its signed current
/// frontier with the history already held locally before accepting a pull.
/// Other authors may legitimately be ahead or behind on either device.
pub fn verify_remote_author(
    roster: &Roster,
    store: &mut Store,
    wire: &[u8],
    trusted_peer: PeerId,
) -> Result<(), RecoveryError> {
    if store.membership() != roster.membership() {
        return Err(RecoveryError::WrongScope);
    }
    let anchor = parse_signed_anchor(roster, wire, trusted_peer)?;
    store.refresh()?;
    let local = store.replica().clock().get(&trusted_peer).copied().unwrap_or(0);
    let remote = anchor.prefixes.iter().find(|prefix| prefix.peer == trusted_peer);
    let remote_sequence = remote.map_or(0, |prefix| prefix.sequence);
    let id = EventId { peer: trusted_peer, sequence: local };
    if remote_sequence < local {
        return Err(RecoveryError::RemoteBehind(id));
    }
    if local != 0 && remote_sequence == local
        && prefix_digest(store, roster, trusted_peer, local)? != remote.unwrap().digest
    {
        return Err(RecoveryError::Diverged(id));
    }
    Ok(())
}

/// Both anchors must match the same durable local history before a peer-held
/// high-water mark can advance. This proves continuity even though each anchor
/// contains only one digest per author's final sequence.
pub fn verify_anchor_advance(
    roster: &Roster,
    store: &mut Store,
    previous: &[u8],
    candidate: &[u8],
    trusted_peer: PeerId,
) -> Result<(), RecoveryError> {
    verify_anchor(roster, store, previous, trusted_peer)?;
    verify_anchor(roster, store, candidate, trusted_peer)?;
    let old = parse_signed_anchor(roster, previous, trusted_peer)?;
    let new = parse_signed_anchor(roster, candidate, trusted_peer)?;
    for prefix in old.prefixes {
        let next = new.prefixes.iter().find(|next| next.peer == prefix.peer);
        if next.is_none_or(|next| next.sequence < prefix.sequence) {
            return Err(RecoveryError::StaleAnchor(EventId {
                peer: prefix.peer,
                sequence: prefix.sequence,
            }));
        }
    }
    Ok(())
}

fn parse_signed_anchor(
    roster: &Roster,
    wire: &[u8],
    trusted_peer: PeerId,
) -> Result<Anchor, RecoveryError> {
    let (author, payload) = roster.verify_recovery_payload(wire)?;
    if *author != trusted_peer {
        return Err(RecoveryError::UntrustedSigner);
    }
    let anchor: Anchor = decode(payload).map_err(|_| RecoveryError::InvalidAnchor)?;
    if encode(&anchor).map_err(|_| RecoveryError::InvalidAnchor)? != payload {
        return Err(RecoveryError::InvalidAnchor);
    }
    let mut previous = None;
    for prefix in &anchor.prefixes {
        if prefix.sequence == 0
            || !roster.membership().members.contains(&prefix.peer)
            || previous.is_some_and(|peer| peer >= prefix.peer)
        {
            return Err(RecoveryError::InvalidAnchor);
        }
        previous = Some(prefix.peer);
    }
    Ok(anchor)
}

fn prefix_digest(
    store: &Store,
    roster: &Roster,
    peer: PeerId,
    sequence: u64,
) -> Result<[u8; 32], RecoveryError> {
    let mut hash = Sha256::new();
    hash.update(HASH_DOMAIN);
    hash.update(roster.membership().share);
    hash.update(roster.membership().generation.to_be_bytes());
    hash.update(peer);
    for number in 1..=sequence {
        let id = EventId {
            peer,
            sequence: number,
        };
        let wire = store
            .signed_event(id)
            .ok_or(RecoveryError::MissingSignedEvent(id))?;
        hash.update((wire.len() as u64).to_be_bytes());
        hash.update(wire);
    }
    Ok(hash.finalize().into())
}
