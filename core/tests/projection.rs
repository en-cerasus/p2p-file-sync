use p2p_sync_core::{
    projection::{PathIssue, Projection},
    replica::*,
};

const PEER: PeerId = [5; 32];
const ROOT: ObjectId = [1; 16];
const CHILD: ObjectId = [2; 16];
const OTHER: ObjectId = [3; 16];

fn add(replica: &mut Replica, object: ObjectId, change: Change) -> EventId {
    let context = replica.clock().clone();
    let id = EventId {
        peer: PEER,
        sequence: context.get(&PEER).copied().unwrap_or(0) + 1,
    };
    assert_eq!(
        replica.apply(Event {
            id,
            context,
            object,
            change
        }),
        Ok(Apply::Added)
    );
    id
}

fn entry(name: &str, parent: Option<ObjectId>, kind: EntryKind) -> Entry {
    Entry {
        parent,
        name: name.into(),
        kind,
    }
}

#[test]
fn resolves_by_parent_identity_and_blocks_canonical_sibling_collisions() {
    let mut replica = Replica::default();
    let dir = add(
        &mut replica,
        ROOT,
        Change::Create(entry("譜面", None, EntryKind::Directory)),
    );
    add(
        &mut replica,
        CHILD,
        Change::Create(entry("É.tja", Some(ROOT), EntryKind::File([1; 32]))),
    );
    assert_eq!(
        Projection::new(&replica).path(CHILD),
        Ok(vec!["譜面", "É.tja"])
    );
    add(
        &mut replica,
        ROOT,
        Change::Revise {
            base: dir,
            entry: Some(entry("移動済", None, EntryKind::Directory)),
        },
    );
    assert_eq!(
        Projection::new(&replica).path(CHILD),
        Ok(vec!["移動済", "É.tja"])
    );

    add(
        &mut replica,
        OTHER,
        Change::Create(entry("e\u{301}.TJA", Some(ROOT), EntryKind::File([2; 32]))),
    );
    let projection = Projection::new(&replica);
    assert_eq!(projection.path(CHILD), Err(PathIssue::NameCollision(CHILD)));
    assert_eq!(projection.path(OTHER), Err(PathIssue::NameCollision(OTHER)));
    assert_eq!(projection.path(ROOT), Ok(vec!["移動済"]));
}

#[test]
fn rejects_cycles_and_file_ancestors() {
    let mut replica = Replica::default();
    let root = add(
        &mut replica,
        ROOT,
        Change::Create(entry("root", None, EntryKind::Directory)),
    );
    let child = add(
        &mut replica,
        CHILD,
        Change::Create(entry("child", Some(ROOT), EntryKind::Directory)),
    );
    add(
        &mut replica,
        OTHER,
        Change::Create(entry("song", Some(CHILD), EntryKind::File([1; 32]))),
    );
    add(
        &mut replica,
        ROOT,
        Change::Revise {
            base: root,
            entry: Some(entry("root", Some(CHILD), EntryKind::Directory)),
        },
    );
    assert_eq!(
        Projection::new(&replica).path(OTHER),
        Err(PathIssue::Cycle(CHILD))
    );

    add(
        &mut replica,
        CHILD,
        Change::Revise {
            base: child,
            entry: Some(entry("child", Some(ROOT), EntryKind::File([3; 32]))),
        },
    );
    assert_eq!(
        Projection::new(&replica).path(OTHER),
        Err(PathIssue::ParentIsFile(CHILD))
    );
}
