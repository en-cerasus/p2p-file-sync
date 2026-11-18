use std::collections::BTreeSet;

use crate::{
    auth::{AuthError, Roster},
    replica::{Apply, EventId, VersionVector},
    store::{Store, StoreError},
};

pub const MAX_PAGE_EVENTS: usize = 64;
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub enum CatchupError {
    Store(StoreError),
    Authentication(AuthError),
    InvalidFrontier,
    OversizedPage,
    UnsignedEvent(EventId),
}

impl From<StoreError> for CatchupError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<AuthError> for CatchupError {
    fn from(value: AuthError) -> Self {
        Self::Authentication(value)
    }
}

pub struct EventPage<'a> {
    pub events: Vec<&'a [u8]>,
    pub has_more: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ReceiveReport {
    pub added: usize,
    pub duplicates: usize,
    pub deferred: usize,
    pub missing_dependencies: BTreeSet<EventId>,
}

/// Select a bounded page in local causal application order. The remote
/// frontier is used for filtering only and never advances local state.
pub fn outgoing_page<'a>(
    store: &'a mut Store,
    remote: &VersionVector,
) -> Result<EventPage<'a>, CatchupError> {
    store.refresh()?;
    if remote
        .iter()
        .any(|(peer, &sequence)| sequence == 0 || !store.membership().members.contains(peer))
    {
        return Err(CatchupError::InvalidFrontier);
    }

    let mut events = Vec::new();
    let mut bytes = 0;
    for &id in store.event_order() {
        if remote.get(&id.peer).copied().unwrap_or(0) >= id.sequence {
            continue;
        }

        let wire = store
            .signed_event(id)
            .ok_or(CatchupError::UnsignedEvent(id))?;
        if events.len() == MAX_PAGE_EVENTS || wire.len() > MAX_PAGE_BYTES - bytes {
            return Ok(EventPage {
                events,
                has_more: true,
            });
        }
        bytes += wire.len();
        events.push(wire);
    }

    Ok(EventPage {
        events,
        has_more: false,
    })
}

/// Authenticate a whole page before committing any event. Events blocked
/// by earlier frames in the same page are retried. The caller can resend
/// unresolved frames, using the durable local clock after reconnection.
pub fn receive_page(
    store: &mut Store,
    roster: &Roster,
    frames: &[&[u8]],
) -> Result<ReceiveReport, CatchupError> {
    if store.membership() != roster.membership() {
        return Err(AuthError::WrongScope.into());
    }
    if frames.len() > MAX_PAGE_EVENTS
        || frames
            .iter()
            .try_fold(0_usize, |size, frame| size.checked_add(frame.len()))
            .is_none_or(|size| size > MAX_PAGE_BYTES)
    {
        return Err(CatchupError::OversizedPage);
    }

    let mut remaining = frames
        .iter()
        .map(|&wire| roster.verify_event(wire).map(|event| (event, wire)))
        .collect::<Result<Vec<_>, _>>()?;

    let mut added = 0;
    let mut duplicates = 0;
    loop {
        let previous = remaining.len();
        let mut waiting = Vec::new();
        let mut missing_dependencies = BTreeSet::new();
        for (event, wire) in remaining {
            match store.apply_signed(event.clone(), wire.to_vec())? {
                Apply::Added => added += 1,
                Apply::Duplicate => duplicates += 1,
                Apply::MissingDependencies(missing) => {
                    missing_dependencies.extend(missing);
                    waiting.push((event, wire));
                }
            }
        }
        if waiting.is_empty() || waiting.len() == previous {
            return Ok(ReceiveReport {
                added,
                duplicates,
                deferred: waiting.len(),
                missing_dependencies,
            });
        }
        remaining = waiting;
    }
}
