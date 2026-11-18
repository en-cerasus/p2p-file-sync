use p2p_sync_core::replica::*;

const OBJECT: ObjectId = [9; 16];

fn id(peer: u8, sequence: u64) -> EventId {
    EventId {
        peer: [peer; 32],
        sequence,
    }
}

fn entry(content: u8) -> Entry {
    Entry {
        parent: None,
        name: "song.tja".into(),
        kind: EntryKind::File([content; 32]),
    }
}

fn event(id: EventId, context: &[EventId], change: Change) -> Event {
    Event {
        id,
        context: context.iter().map(|id| (id.peer, id.sequence)).collect(),
        object: OBJECT,
        change,
    }
}

fn revise(base: EventId, content: Option<u8>) -> Change {
    Change::Revise {
        base,
        entry: content.map(entry),
    }
}

fn resolve(bases: [EventId; 2], content: u8) -> Change {
    Change::Resolve {
        bases: bases.into_iter().collect(),
        entry: Some(entry(content)),
    }
}

fn add(replica: &mut Replica, event: &Event) {
    assert_eq!(replica.apply(event.clone()), Ok(Apply::Added));
}

#[test]
fn missing_dependency_does_not_change_state_and_replays_are_idempotent() {
    let mut replica = Replica::default();
    let root = event(id(1, 1), &[], Change::Create(entry(1)));
    let next = event(id(1, 2), &[root.id], revise(root.id, Some(2)));

    assert_eq!(
        replica.apply(next.clone()),
        Ok(Apply::MissingDependencies(vec![root.id]))
    );
    assert!(replica.clock().is_empty());
    assert_eq!(replica.heads(OBJECT).count(), 0);

    add(&mut replica, &root);
    add(&mut replica, &next);
    assert_eq!(replica.apply(root.clone()), Ok(Apply::Duplicate));
    let mut forged = root.clone();
    forged.change = Change::Create(entry(2));
    assert_eq!(replica.apply(forged), Err(Reject::EventIdReuse));
    assert_eq!(replica.heads(OBJECT).collect::<Vec<_>>(), vec![&next]);
}

#[test]
fn every_delivery_order_preserves_a_delete_conflict_and_parallel_resolutions() {
    let root = event(id(1, 1), &[], Change::Create(entry(1)));
    let edit = event(id(1, 2), &[root.id], revise(root.id, Some(2)));
    let delete = event(id(2, 1), &[root.id], revise(root.id, None));
    let bases = [edit.id, delete.id];
    let first = event(id(2, 2), &bases, resolve(bases, 3));
    let second = event(id(3, 1), &bases, resolve(bases, 4));

    let mut before_resolution = Replica::default();
    for item in [&root, &edit, &delete] {
        add(&mut before_resolution, item);
    }
    assert!(before_resolution.has_conflict(OBJECT));
    assert_eq!(
        before_resolution.heads(OBJECT).collect::<Vec<_>>(),
        vec![&edit, &delete]
    );
    assert_eq!(before_resolution.get(root.id), Some(&root));

    let mut events = [&root, &edit, &delete, &first, &second];
    for_each_order(&mut events, &mut |delivery| {
        let mut replica = Replica::default();
        let mut pending = delivery.to_vec();
        while !pending.is_empty() {
            let previous = pending.len();
            pending.retain(|item| {
                matches!(
                    replica.apply((*item).clone()).unwrap(),
                    Apply::MissingDependencies(_)
                )
            });
            assert!(pending.len() < previous);
        }

        assert_eq!(
            replica.heads(OBJECT).collect::<Vec<_>>(),
            vec![&first, &second]
        );
        assert!(replica.has_conflict(OBJECT));
        assert_eq!(replica.get(root.id), Some(&root));
    });
}

fn for_each_order<T>(items: &mut [T], check: &mut impl FnMut(&[T])) {
    fn visit<T>(items: &mut [T], start: usize, check: &mut impl FnMut(&[T])) {
        if start == items.len() {
            check(items);
            return;
        }
        for index in start..items.len() {
            items.swap(start, index);
            visit(items, start + 1, check);
            items.swap(start, index);
        }
    }

    visit(items, 0, check);
}

#[test]
fn rejects_noncausal_context_and_a_resolution_that_omits_a_visible_head() {
    let mut replica = Replica::default();
    let root = event(id(1, 1), &[], Change::Create(entry(1)));
    let left = event(id(2, 1), &[root.id], revise(root.id, Some(2)));
    let right = event(id(3, 1), &[root.id], revise(root.id, Some(2)));
    for item in [&root, &left, &right] {
        add(&mut replica, item);
    }
    assert!(!replica.has_conflict(OBJECT));
    assert_eq!(replica.heads(OBJECT).count(), 2);

    let mut next = event(id(4, 1), &[left.id], revise(left.id, Some(3)));
    assert_eq!(replica.apply(next.clone()), Err(Reject::InvalidContext));
    next.context = [(root.id.peer, 1), (left.id.peer, 1), (right.id.peer, 1)].into();
    assert_eq!(replica.apply(next), Err(Reject::InvalidBasis));
    assert_eq!(
        replica.heads(OBJECT).collect::<Vec<_>>(),
        vec![&left, &right]
    );
}
