use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::replica::{ContentHash, EventId, ObjectId, PeerId, VersionVector};

/// An authenticated acknowledgement from one member of the current generation.
/// The issuing peer must first durably reconcile its local edits and switch
/// its edit base. Transport receipt alone is not an acknowledgement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Confirmation {
    pub generation: u64,
    pub peer: PeerId,
    pub object: ObjectId,
    pub successor: EventId,
    pub sent_through: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerState {
    AwaitingConfirmation,
    AwaitingEvents { through: u64 },
    Applied,
}

/// One durable reference in the eventual blob store. Multiple replacements
/// can protect the same content hash independently.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Protection {
    generation: u64,
    object: ObjectId,
    original: EventId,
    successor: EventId,
    content: ContentHash,
    members: BTreeMap<PeerId, Option<u64>>,
}

impl Protection {
    pub fn new(
        generation: u64,
        object: ObjectId,
        original: EventId,
        successor: EventId,
        content: ContentHash,
        members: &BTreeSet<PeerId>,
    ) -> Self {
        Self {
            generation,
            object,
            original,
            successor,
            content,
            members: members.iter().map(|&peer| (peer, None)).collect(),
        }
    }

    pub fn original(&self) -> EventId {
        self.original
    }

    pub fn content(&self) -> ContentHash {
        self.content
    }

    /// Only the authenticated acknowledgement for this exact replacement
    /// can advance a member. A repeated or reordered acknowledgement
    /// cannot reduce the required outbound frontier.
    pub fn confirm(&mut self, ack: Confirmation) -> bool {
        if ack.generation != self.generation
            || ack.object != self.object
            || ack.successor != self.successor
            || (ack.peer == self.successor.peer && ack.sent_through < self.successor.sequence)
        {
            return false;
        }

        let Some(through) = self.members.get_mut(&ack.peer) else {
            return false;
        };
        *through = Some(through.map_or(ack.sent_through, |current| current.max(ack.sent_through)));
        true
    }

    /// The supplied clock must cover only contiguous events already applied
    /// to the causal index, with any required conflict references recorded.
    pub fn peer_states<'a>(
        &'a self,
        applied: &'a VersionVector,
    ) -> impl Iterator<Item = (PeerId, PeerState)> + 'a {
        self.members.iter().map(|(&peer, through)| {
            let state = match *through {
                None => PeerState::AwaitingConfirmation,
                Some(through) if applied.get(&peer).copied().unwrap_or(0) < through => {
                    PeerState::AwaitingEvents { through }
                }
                Some(_) => PeerState::Applied,
            };
            (peer, state)
        })
    }

    /// Only this reference can be released. Archive, conflict, live-version
    /// and transfer references are checked independently by the blob store.
    pub fn can_release(&self, applied: &VersionVector) -> bool {
        applied.get(&self.successor.peer).copied().unwrap_or(0) >= self.successor.sequence
            && self
                .peer_states(applied)
                .all(|(_, state)| state == PeerState::Applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITER: PeerId = [1; 32];
    const OFFLINE: PeerId = [2; 32];
    const READER: PeerId = [3; 32];
    const OBJECT: ObjectId = [7; 16];
    const ORIGINAL: EventId = EventId {
        peer: WRITER,
        sequence: 1,
    };
    const SUCCESSOR: EventId = EventId {
        peer: WRITER,
        sequence: 2,
    };

    fn protection() -> Protection {
        Protection::new(
            2,
            OBJECT,
            ORIGINAL,
            SUCCESSOR,
            [9; 32],
            &[WRITER, OFFLINE, READER].into(),
        )
    }

    fn acknowledgement(peer: PeerId, sent_through: u64) -> Confirmation {
        Confirmation {
            generation: 2,
            peer,
            object: OBJECT,
            successor: SUCCESSOR,
            sent_through,
        }
    }

    #[test]
    fn early_confirmation_keeps_original_until_missing_event_is_applied() {
        let mut protection = protection();
        let mut clock = [(WRITER, 2)].into();

        assert!(protection.confirm(acknowledgement(WRITER, 2)));
        assert!(protection.confirm(acknowledgement(READER, 0)));
        assert!(!protection.can_release(&clock));

        assert!(protection.confirm(acknowledgement(OFFLINE, 1)));
        assert_eq!(
            protection.peer_states(&clock).collect::<Vec<_>>(),
            [
                (WRITER, PeerState::Applied),
                (OFFLINE, PeerState::AwaitingEvents { through: 1 }),
                (READER, PeerState::Applied),
            ],
        );
        assert!(protection.confirm(acknowledgement(OFFLINE, 0)));
        assert!(!protection.can_release(&clock));

        clock.insert(OFFLINE, 1);
        assert!(protection.can_release(&clock));
    }

    #[test]
    fn unrelated_or_old_confirmations_do_not_unprotect_original() {
        let mut protection = protection();
        let mut invalid = acknowledgement(OFFLINE, 0);
        invalid.generation = 1;
        assert!(!protection.confirm(invalid));
        invalid.generation = 2;
        invalid.successor = ORIGINAL;
        assert!(!protection.confirm(invalid));
        assert!(!protection.confirm(acknowledgement([4; 32], 0)));
        assert!(!protection.confirm(acknowledgement(WRITER, 1)));

        assert!(protection
            .peer_states(&VersionVector::new())
            .all(|(_, state)| state == PeerState::AwaitingConfirmation));
    }
}
