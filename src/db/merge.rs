//! Merging two versions of the same database.
//!
//! See [`Database::merge`][crate::Database::merge] for the entry point. The
//! types in this module describe what a merge changed, via the returned
//! [`MergeLog`].

use std::{collections::HashSet, ops::Deref};

use chrono::NaiveDateTime;
use thiserror::Error;

use crate::{
    db::{
        CustomIconId, Entry, EntryId, EntryMut, Group, GroupId, GroupMut, GroupRef, History, MoveGroupError,
        Times,
    },
    Database,
};

/// The kind of change a merge applied to an object.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum MergeEventType {
    /// The object existed only in the source and was created in the destination.
    Created,
    /// The object was deleted as a result of the merge.
    Deleted,
    /// The object was moved to a different location (parent group).
    LocationUpdated,
    /// The object's contents were updated from the source.
    Updated,
}

/// The object a [`MergeEvent`] applies to.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum MergeEventTarget {
    /// An entry, identified by its UUID.
    Entry(EntryId),
    /// A group, identified by its UUID.
    Group(GroupId),
    /// A custom icon, identified by its UUID.
    Icon(CustomIconId),
}

/// A single change applied to the destination database during a merge.
#[derive(Debug, Clone)]
pub struct MergeEvent {
    /// The object that was changed.
    pub target: MergeEventTarget,
    /// The kind of change that was applied.
    pub event_type: MergeEventType,
}

/// Errors while merge two databases
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum MergeError {
    /// Two entries with the same UUID have the same modification time but different contents.
    #[error("Entries with UUID {0} have the same modification time but have diverged.")]
    EntryModificationTimeNotUpdated(EntryId),

    /// Two groups with the same UUID have the same modification time but different contents.
    #[error("Groups with UUID {0} have the same modification time but have diverged.")]
    GroupModificationTimeNotUpdated(GroupId),

    /// An entry has two history items sharing the same timestamp, so their order is ambiguous.
    #[error("Found history entries with the same timestamp ({0}) for entry {1}.")]
    DuplicateHistoryEntries(NaiveDateTime, EntryId),

    /// A group could not be moved to its merged location.
    #[error(transparent)]
    MoveGroupError(#[from] MoveGroupError),
}

/// Record of everything a merge changed, returned by [`Database::merge`].
#[derive(Debug, Default, Clone)]
pub struct MergeLog {
    /// Non-fatal issues encountered during the merge.
    pub warnings: Vec<String>,
    /// The changes that were applied to the destination database.
    pub events: Vec<MergeEvent>,
}

impl Database {
    /// Merge a database with another version of the same database and a common ancestor,
    /// applying the changes to self.
    ///
    /// This is a three-way merge. Providing the last-known-common ancestor lets the merge
    /// distinguish a one-sided change from a true conflict:
    ///
    /// - Changed in `self` only (since `ancestor`) → keep `self`'s value, silently.
    /// - Changed in `other` only → take `other`'s value, silently.
    /// - Changed identically in both → trivially resolved.
    /// - Changed differently in both → real conflict: resolved by newest-wins and surfaced in
    ///   [`MergeLog::warnings`].
    ///
    /// UUIDs are used to detect which entries and groups are the same across the three databases.
    /// Use [`Database::merge`] when no ancestor is available; it degrades to two-way (newest-wins)
    /// semantics by treating an empty database as the ancestor, so every object looks changed on
    /// both sides.
    pub fn merge_with_ancestor(
        &mut self,
        other: &Database,
        ancestor: &Database,
    ) -> Result<MergeLog, MergeError> {
        let mut log = MergeLog::default();
        merge_icons(self, other, ancestor, &mut log)?;
        merge_groups(self, other, ancestor, &mut log)?;

        Ok(log)
    }

    /// Merge a database with another version of the same database, applying the changes to self.
    ///
    /// This is a two-way merge: conflicts are resolved by `last_modification` timestamp alone
    /// (newest-wins). When a last-known-common ancestor is available,
    /// [`Database::merge_with_ancestor`] resolves one-sided changes automatically and surfaces
    /// true conflicts more precisely.
    ///
    /// This function will use the UUIDs to detect what entries and groups are the same.
    pub fn merge(&mut self, other: &Database) -> Result<MergeLog, MergeError> {
        self.merge_with_ancestor(other, &Database::new())
    }
}

/// Get the last update time (modification or location change) of a group, considering its entries and subgroups.
fn get_last_update(group: GroupRef<'_>) -> Option<NaiveDateTime> {
    let last_update = group.times.last_modification.or(group.times.location_changed);

    group
        .entries()
        .filter_map(|e| e.times.last_modification.or(e.times.location_changed))
        .chain(
            group
                .groups()
                .filter_map(|g| g.times.last_modification.or(g.times.location_changed)),
        )
        .chain(last_update)
        .max()
}

/// Merge groups from `source` into `dest`, appending to a log of the merge process.
///
/// NOTE: this function will also call `merge_entries` to handle entries within the groups.
fn merge_groups(
    dest_db: &mut Database,
    source_db: &Database,
    base_db: &Database,
    log: &mut MergeLog,
) -> Result<(), MergeError> {
    let dest_groups = dest_db.groups.keys().cloned().collect::<HashSet<_>>();
    let source_groups = source_db.groups.keys().cloned().collect::<HashSet<_>>();

    // Handle groups that exist only in source and might need to be added.
    let mut groups_to_add = HashSet::new();
    for &id in source_groups.difference(&dest_groups) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let source = source_db.group(id).unwrap();

        // was the group deleted in dest?
        if let Some(deletion_time) = dest_db.deleted_objects.get(&id.uuid()) {
            // Delete-vs-edit conflict: deleted in the destination, still present in the source.
            // If it existed in the ancestor and the source edited it since, warn (the timestamp
            // logic below still decides the outcome).
            if let Some(base_group) = base_db.group(id) {
                if have_groups_diverged(&source, &base_group) {
                    log.warnings.push(format!(
                        "Group {id} was deleted in the destination but modified in the source \
                         since the common ancestor.",
                    ));
                }
            }

            // get the last modification time of the group in source.
            let source_last_update = get_last_update(source);

            // compare deletion time and last update time to decide whether to re-add the group
            match (deletion_time, source_last_update) {
                (Some(deletion_time), Some(source_last_update)) => {
                    // if the group was deleted after its last modification time in source,
                    // do not re-add it, otherwise we can re-add the group
                    if *deletion_time >= source_last_update {
                        continue;
                    }
                }
                (Some(_), None) => {
                    // blank last update time in source - do not re-add the group
                    continue;
                }
                (None, Some(_)) => {
                    // blank deletion time is probably older than concrete update time - re-add the
                    // group
                }
                (None, None) => {
                    // both times are blank - do not re-add the group
                    continue;
                }
            }
        }

        groups_to_add.insert(id);
    }

    // actually add groups from groups_to_add. Use a stack to ensure that parent groups are added as needed
    let mut add_stack = Vec::new();
    loop {
        // refill the stack if it's empty
        if add_stack.is_empty() {
            if let Some(&next) = groups_to_add.iter().next() {
                // refill the stack with an arbitrary group to re-add
                add_stack.push(next);
                groups_to_add.remove(&next);
            } else {
                // no more groups to re-add
                break;
            }
        }

        // get the current group from the stack
        #[allow(clippy::expect_used)] // stack is guaranteed to be non-empty
        let &id = add_stack.last().expect("non-empty queue");

        // get the desired parent of the group to be re-added
        #[allow(clippy::expect_used)] // id is guaranteed to exist in source
        let source = source_db.group(id).expect("source group exists");

        #[allow(clippy::expect_used)] // this would be a severe issue with the algorithm
        let parent_id = source.parent().expect("cannot re-add root").id();

        // does the parent exist in dest?
        if let Some(mut parent) = dest_db.group_mut(parent_id) {
            // yes - re-add the group
            #[allow(clippy::expect_used)] // id was selected from source_groups.difference(dest_groups)
            let mut dest_group = parent
                .add_group_with_id(id)
                .expect("group to be re-added should not exist yet");
            dest_group.times = source.times.clone();
            dest_group.name = source.name.clone();
            dest_group.notes = source.notes.clone();
            dest_group.icon = source.icon.clone();
            dest_group.custom_data = source.custom_data.clone();
            dest_group.is_expanded = source.is_expanded;
            dest_group.default_autotype_sequence = source.default_autotype_sequence.clone();
            dest_group.enable_autotype = source.enable_autotype;
            dest_group.enable_searching = source.enable_searching;
            dest_group.last_top_visible_entry = source.last_top_visible_entry;

            log.events.push(MergeEvent {
                target: MergeEventTarget::Group(id),
                event_type: MergeEventType::Created,
            });

            // success - remove the current item from the stack (it was already removed from the set)
            add_stack.pop();
        } else {
            // the parent does not exist yet - add it to the stack to be re-added first
            add_stack.push(parent_id);

            // since we will deal with the parent now, it doesn't need to be handled later
            groups_to_add.remove(&parent_id);
        }
    }

    // Handle groups that exist only in destination. These groups might need to be deleted.
    let mut to_delete = Vec::new();
    for &id in dest_groups.difference(&source_groups) {
        // Delete-vs-edit conflict: present in the destination, tombstoned in the source. If it
        // existed in the ancestor and the destination edited it since, warn. Computed before the
        // mutable borrow below so the immutable borrow ends first.
        if source_db.deleted_objects.contains_key(&id.uuid()) {
            if let Some(base_group) = base_db.group(id) {
                #[allow(clippy::unwrap_used)] // id is guaranteed to exist in dest
                let dest_group_ref = dest_db.group(id).unwrap();
                if have_groups_diverged(&dest_group_ref, &base_group) {
                    log.warnings.push(format!(
                        "Group {id} was modified in the destination but deleted in the source \
                         since the common ancestor.",
                    ));
                }
            }
        }

        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let dest = dest_db.group_mut(id).unwrap();

        // was the group deleted in source?
        if let Some(deletion_time) = source_db.deleted_objects.get(&id.uuid()) {
            let dest_last_updated = get_last_update(dest.as_ref());
            if let (Some(deletion_time), Some(dest_last_updated)) = (deletion_time, dest_last_updated) {
                // if the group was deleted and then later modified in dest, do not delete it
                if *deletion_time < dest_last_updated {
                    continue;
                }
            }

            // queue the deletion so that all subgroups will also emit a deletion event
            to_delete.push(id);
            dest_db.deleted_objects.insert(id.uuid(), *deletion_time);

            log.events.push(MergeEvent {
                target: MergeEventTarget::Group(id),
                event_type: MergeEventType::Deleted,
            });
        }
    }

    // perform the entry merges now that all groups that need adding are added but the groups that
    // need deleting still haven't been deleted, so that the entries can still be accessed and
    // generate events
    merge_entries(dest_db, source_db, base_db, log)?;

    // perform all group deletions
    while let Some(id) = to_delete.pop() {
        if let Some(group) = dest_db.group_mut(id) {
            group.remove();
        }
    }

    // re-compute the group set after additions and deletions
    let dest_groups = dest_db.groups.keys().cloned().collect::<HashSet<_>>();

    // Handle groups that exist in both source and destination.
    let mut moves = Vec::new();
    let root_id = dest_db.root().id();
    for &id in dest_groups.intersection(&source_groups) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let mut dest = dest_db.group_mut(id).unwrap();

        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let source = source_db.group(id).unwrap();

        let dest_parent_id = dest.as_ref().parent().map(|p| p.id());
        let source_parent_id = source.parent().map(|p| p.id());

        // was the group moved?
        if dest_parent_id != source_parent_id {
            let dest_location_changed = dest.times.location_changed;
            let source_location_changed = source.times.location_changed;

            if let (Some(dlc), Some(slc)) = (dest_location_changed, source_location_changed) {
                if slc > dlc {
                    // the source group has been moved more recently than the destination group.
                    // try to move the destination group to the new location.

                    let Some(parent_id) = source.parent().map(|p| p.id()) else {
                        log.warnings.push(format!("Cannot move root group {}", id,));
                        continue;
                    };

                    if !dest_groups.contains(&parent_id) {
                        log.warnings.push(format!(
                            "Cannot move group {} to group {} because the group does not exist in the destination database.",
                            id,
                            parent_id,
                        ));
                        continue;
                    };

                    // to avoid creating cycles in situations where two groups swap their parent-child
                    // relationship, move all groups to root first and then to their final destination
                    moves.push((id, parent_id));
                    dest.move_to(root_id)?;
                    dest.times.location_changed = Some(slc);

                    log.events.push(MergeEvent {
                        target: MergeEventTarget::Group(id),
                        event_type: MergeEventType::LocationUpdated,
                    });
                }
            } else {
                log.warnings.push(format!(
                    "Cannot determine which group {} move is more recent because one of the groups does not have a location changed timestamp.",
                    id,
                ));
            }
        }

        let dest_last_modification = dest.times.last_modification.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Destination group {} did not have a last modification timestamp",
                id
            ));
            Times::now()
        });

        let source_last_modification = source.times.last_modification.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Source group {} did not have a last modification timestamp",
                id
            ));
            Times::epoch()
        });

        if dest_last_modification == source_last_modification {
            if !have_groups_diverged(&dest, &source) {
                continue;
            }
            // Divergent content at the same timestamp — resolve as a three-way conflict when an
            // ancestor is present; the two-way `merge` shim keeps its historical hard error.
            let Some(base_group) = base_db.group(id) else {
                return Err(MergeError::GroupModificationTimeNotUpdated(id));
            };
            let dest_changed = have_groups_diverged(&dest, &base_group);
            let source_changed = have_groups_diverged(&source, &base_group);

            if dest_changed && source_changed {
                log.warnings.push(format!(
                    "Group {id} was modified in both databases at the same timestamp since the \
                     common ancestor. Resolving by keeping the destination copy.",
                ));
                continue;
            }
            if !source_changed {
                continue;
            }

            // Only the source changed — adopt it despite the equal timestamp.
            overwrite_group_fields(&mut dest, &source);
            log.events.push(MergeEvent {
                target: MergeEventTarget::Group(id),
                event_type: MergeEventType::Updated,
            });
            continue;
        }

        // Three-way check (see `merge_entries` for the rationale and the two-way no-op property).
        let base_lm = base_db.group(id).and_then(|g| g.times.last_modification);
        let dest_changed = base_lm.is_none_or(|b| dest_last_modification > b);
        let source_changed = base_lm.is_none_or(|b| source_last_modification > b);

        if !source_changed {
            // The source contributed nothing since the ancestor; keep the destination as-is.
            continue;
        }

        if dest_changed && base_lm.is_some() && have_groups_diverged(&dest, &source) {
            log.warnings.push(format!(
                "Group {id} was modified in both databases since the common ancestor. \
                 Resolving by keeping the most recently modified version.",
            ));
        }

        if dest_last_modification > source_last_modification {
            // The destination group is more recent than the source group. Nothing to do.
            continue;
        }

        // The source group is more recent than the destination group. Update dest with source.
        overwrite_group_fields(&mut dest, &source);

        log.events.push(MergeEvent {
            target: MergeEventTarget::Group(id),
            event_type: MergeEventType::Updated,
        });
    }

    // perform all the moves that were queued up
    for (group_id, parent_id) in moves {
        #[allow(clippy::unwrap_used)] // group_id and parent_id are guaranteed to exist
        let mut group = dest_db.group_mut(group_id).unwrap();
        group.move_to(parent_id)?;
    }

    Ok(())
}

/// Merge entries from `source` into `dest`, appending to a log of the merge process.
fn merge_entries(
    dest_db: &mut Database,
    source_db: &Database,
    base_db: &Database,
    log: &mut MergeLog,
) -> Result<(), MergeError> {
    let dest_entries = dest_db.entries.keys().cloned().collect::<HashSet<_>>();
    let source_entries = source_db.entries.keys().cloned().collect::<HashSet<_>>();

    // Handle entries that exist only in source and might need to be added.
    for &id in source_entries.difference(&dest_entries) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let source_entry = source_db.entry(id).unwrap();

        // was the entry deleted in dest?
        if let Some(deletion_time) = dest_db.deleted_objects.get(&id.uuid()) {
            // Delete-vs-edit conflict: deleted in the destination, still present in the source.
            // If it existed in the ancestor and the source edited it since, that is a two-sided
            // divergent change — surface it (the timestamp logic below still decides the outcome).
            if let Some(base_entry) = base_db.entry(id) {
                if have_entries_diverged(&source_entry, source_db, &base_entry, base_db) {
                    log.warnings.push(format!(
                        "Entry {id} was deleted in the destination but modified in the source \
                         since the common ancestor.",
                    ));
                }
            }

            // get the last modification or location change time in source.
            let source_update_time = source_entry
                .times
                .last_modification
                .or(source_entry.times.location_changed);

            match (deletion_time, source_update_time) {
                (Some(deletion_time), Some(source_update_time)) => {
                    // if the entry was deleted after its last modification time in source,
                    // do not re-add it
                    if *deletion_time >= source_update_time {
                        continue;
                    }
                }
                (Some(_), None) => {
                    // blank last update time in source - do not re-add the entry
                    continue;
                }
                (None, Some(_)) => {
                    // blank deletion time is probably older than concrete update time - re-add the
                    // entry
                }
                (None, None) => {
                    // both times are blank - do not re-add the entry
                    continue;
                }
            }

            // otherwise, we can re-add the entry
        }

        let parent_id = source_entry.parent().id();

        // Clone the source entry and rewrite its attachment references —
        // they point into the *source* pool and would otherwise dangle or
        // collide with unrelated destination blobs.
        let mut cloned = source_entry.deref().clone();
        cloned.remap_attachments(source_db, dest_db);

        let Some(mut parent) = dest_db.group_mut(parent_id) else {
            log.warnings.push(format!(
                "Cannot add entry {} because its parent group {} does not exist in the destination database.",
                id, parent_id,
            ));
            continue;
        };

        #[allow(clippy::expect_used)] // id was selected from source_entries.difference(dest_entries)
        let mut entry = parent
            .add_entry_with_id(id)
            .expect("entry to be (re-)added should not exist yet");

        *entry = cloned;

        log.events.push(MergeEvent {
            target: MergeEventTarget::Entry(id),
            event_type: MergeEventType::Created,
        });
    }

    // Handle entries that exist only in destination. These entries might need to be deleted.
    for &id in dest_entries.difference(&source_entries) {
        // Delete-vs-edit conflict: present in the destination, tombstoned in the source. If it
        // existed in the ancestor and the destination edited it since, that is a two-sided
        // divergent change — surface it before the deletion logic decides the outcome. Computed
        // here so the immutable borrow ends before the mutable `entry_mut` below.
        if source_db.deleted_objects.contains_key(&id.uuid()) {
            if let Some(base_entry) = base_db.entry(id) {
                #[allow(clippy::unwrap_used)] // id is guaranteed to exist in dest
                let dest_entry_ref = dest_db.entry(id).unwrap();
                if have_entries_diverged(&dest_entry_ref, dest_db, &base_entry, base_db) {
                    log.warnings.push(format!(
                        "Entry {id} was modified in the destination but deleted in the source \
                         since the common ancestor.",
                    ));
                }
            }
        }

        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let dest_entry = dest_db.entry_mut(id).unwrap();

        // was the entry deleted in source?
        if let Some(deletion_time) = source_db.deleted_objects.get(&id.uuid()) {
            let dest_update_time = dest_entry
                .times
                .last_modification
                .or(dest_entry.times.location_changed);

            if let (Some(deletion_time), Some(dest_update_time)) = (deletion_time, dest_update_time) {
                // if the entry was deleted and then later modified in dest, do not delete it
                if *deletion_time < dest_update_time {
                    continue;
                }
            }

            dest_entry.remove();
            dest_db.deleted_objects.insert(id.uuid(), *deletion_time);

            log.events.push(MergeEvent {
                target: MergeEventTarget::Entry(id),
                event_type: MergeEventType::Deleted,
            });
        }
    }

    // Handle entries that exist in both source and destination.
    for &id in dest_entries.intersection(&source_entries) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist in both dest and source
        let source_entry = source_db.entry(id).unwrap();
        let source_parent_id = source_entry.parent().id();

        // has the entry moved? (scoped so the mutable borrow of dest_db ends
        // before the pool-touching phases below)
        {
            #[allow(clippy::unwrap_used)] // id is guaranteed to exist in both dest and source
            let mut dest_entry = dest_db.entry_mut(id).unwrap();
            let dest_parent_id = dest_entry.as_ref().parent().id();

            if dest_parent_id != source_parent_id {
                // which move is more recent?
                let source_location_changed = source_entry.times.location_changed;
                let dest_location_changed = dest_entry.times.location_changed;
                if let (Some(slc), Some(dlc)) = (source_location_changed, dest_location_changed) {
                    if slc > dlc {
                        // the source entry has been moved more recently than the destination entry.
                        // try to move the destination entry to the new location.

                        if dest_entry.move_to(source_parent_id).is_ok() {
                            log.events.push(MergeEvent {
                                target: MergeEventTarget::Entry(id),
                                event_type: MergeEventType::LocationUpdated,
                            });
                            dest_entry.times.location_changed = Some(slc);
                        } else {
                            log.warnings.push(format!(
                                "Cannot move entry {} to group {} because the group does not exist in the destination database.",
                                id,
                                source_parent_id,
                            ));
                        }
                    }
                } else {
                    log.warnings.push(format!(
                        "Cannot determine which entry {} move is more recent because one of the entries does not have a location changed timestamp.",
                        id,
                    ));
                }
            }
        }

        // Snapshot the destination entry so the comparisons and the history
        // archive below can run while dest_db's attachment pool is mutated.
        let dest_entry_snapshot = {
            #[allow(clippy::unwrap_used)] // id is guaranteed to exist in both dest and source
            let dest_entry = dest_db.entry(id).unwrap();
            dest_entry.deref().clone()
        };

        let source_last_modification = source_entry.times.last_modification.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Source entry {} did not have a last modification timestamp",
                id
            ));
            Times::epoch()
        });

        let dest_last_modification = dest_entry_snapshot.times.last_modification.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Destination entry {} did not have a last modification timestamp",
                id
            ));
            Times::now()
        });

        // Decide whether the source's fields should replace the destination's. Both the
        // equal-timestamp and unequal-timestamp cases funnel into the shared history-merge tail
        // below, so an equal-timestamp merge preserves the same history guarantee as an ordinary
        // one (the source's intermediate history versions are never dropped).
        let take_source_fields;
        if dest_last_modification == source_last_modification {
            if !have_entries_diverged(&dest_entry_snapshot, dest_db, &source_entry, source_db) {
                // Identical content at the same timestamp — nothing to do.
                continue;
            }

            // Divergent content at the same timestamp (KDBX timestamps have one-second
            // precision, so two branches can edit within the same second). Without an ancestor
            // we cannot tell which side is authoritative, so the two-way `merge` shim keeps its
            // historical hard error. With an ancestor, resolve it as a three-way conflict.
            let Some(base_entry) = base_db.entry(id) else {
                return Err(MergeError::EntryModificationTimeNotUpdated(id));
            };
            let dest_changed = have_entries_diverged(&dest_entry_snapshot, dest_db, &base_entry, base_db);
            let source_changed = have_entries_diverged(&source_entry, source_db, &base_entry, base_db);

            if dest_changed && source_changed {
                // Both sides changed the entry differently within the same timestamp — a true
                // conflict with no newer side. Deterministic tie-break: keep the destination
                // fields (its history still merges with the source's below).
                log.warnings.push(format!(
                    "Entry {id} was modified in both databases at the same timestamp since the \
                     common ancestor. Resolving by keeping the destination copy.",
                ));
                take_source_fields = false;
            } else {
                // Adopt the source only if it is the side that changed.
                take_source_fields = source_changed;
            }
        } else {
            // Unequal timestamps: compare both sides against the common ancestor to distinguish a
            // one-sided change (auto-resolved) from a true conflict (warned, then newest-wins).
            // With the empty ancestor used by the two-way `merge` shim, `base_lm` is `None`, so
            // both sides count as changed and the warning is suppressed — a no-op overlay.
            let base_lm = base_db.entry(id).and_then(|e| e.times.last_modification);
            let dest_changed = base_lm.is_none_or(|b| dest_last_modification > b);
            let source_changed = base_lm.is_none_or(|b| source_last_modification > b);

            if !source_changed {
                // The source contributed nothing since the ancestor; keep the destination as-is.
                continue;
            }

            if dest_changed
                && base_lm.is_some()
                && have_entries_diverged(&dest_entry_snapshot, dest_db, &source_entry, source_db)
            {
                log.warnings.push(format!(
                    "Entry {id} was modified in both databases since the common ancestor. \
                     Resolving by keeping the most recently modified version.",
                ));
            }

            take_source_fields = source_last_modification > dest_last_modification;
        }

        let source_history = source_entry.history.clone().unwrap_or_else(|| {
            log.warnings.push(format!("Source entry {} had no history.", id));
            History::default()
        });

        let dest_history = dest_entry_snapshot.history.clone().unwrap_or_else(|| {
            log.warnings
                .push(format!("Destination entry {} had no history.", id));
            History::default()
        });

        let mut merged_history = merge_history(&dest_history, &source_history, dest_db, source_db, log)?;
        let merged_location_timestamp = dest_entry_snapshot
            .times
            .location_changed
            .or(source_entry.times.location_changed);

        if take_source_fields {
            // add the previous dest entry to history if it has diverged
            if let Some(last_history_entry) = merged_history.entries.first() {
                if have_entries_diverged(&dest_entry_snapshot, dest_db, last_history_entry, dest_db) {
                    let mut dest_entry_for_history = dest_entry_snapshot.clone();
                    dest_entry_for_history.history = None;
                    merged_history.add_entry(dest_entry_for_history);
                }
            }

            // The source entry wins. Replace dest with source, importing the
            // source's attachment bytes into the destination pool.
            let mut incoming = source_entry.deref().clone();
            incoming.history = None;
            incoming.remap_attachments(source_db, dest_db);

            #[allow(clippy::unwrap_used)] // id is guaranteed to exist in both dest and source
            let mut dest_entry = dest_db.entry_mut(id).unwrap();
            overwrite_entry_fields(&mut dest_entry, incoming);

            log.events.push(MergeEvent {
                target: MergeEventTarget::Entry(id),
                event_type: MergeEventType::Updated,
            });
        }

        #[allow(clippy::unwrap_used)] // id is guaranteed to exist in both dest and source
        let mut dest_entry = dest_db.entry_mut(id).unwrap();
        dest_entry.history = Some(merged_history);
        dest_entry.times.location_changed = merged_location_timestamp;
    }

    Ok(())
}

/// Merge two histories together, returning the merged history.
///
/// Source-side history versions are cloned out of `source_db` and therefore
/// carry attachment references into the *source* pool; they are remapped into
/// `dest_db`'s pool before merging.
fn merge_history(
    dest: &History,
    source: &History,
    dest_db: &mut Database,
    source_db: &Database,
    log: &mut MergeLog,
) -> Result<History, MergeError> {
    let mut entries: Vec<Entry> = Vec::new();

    let mut entries_dest: Vec<Entry> = dest.entries.to_vec();
    let mut entries_source: Vec<Entry> = source.entries.to_vec();

    for e in entries_dest.iter_mut() {
        if e.times.last_modification.is_none() {
            log.warnings.push(format!(
                "Destination history entry {} did not have a last modification timestamp",
                e.id()
            ));
            e.times.last_modification = Some(Times::epoch());
        }
    }

    for e in entries_source.iter_mut() {
        e.remap_attachments(source_db, dest_db);
        if e.times.last_modification.is_none() {
            log.warnings.push(format!(
                "Source history entry {} did not have a last modification timestamp",
                e.id()
            ));
            e.times.last_modification = Some(Times::epoch());
        }
    }

    // After the remap above, every version on both sides references dest_db's
    // pool, so the equal-time comparisons below resolve against it alone.
    let dest_db = &*dest_db;

    entries_dest.sort_by_key(|e| e.times.last_modification);
    entries_source.sort_by_key(|e| e.times.last_modification);

    // perform a merge of both histories, which are sorted by last modification time.
    //
    // this code has a lot of unwraps but they are all checked - entry lists are checked for
    // emptiness, and times are made not-none before sorting, so the unwraps should never panic.
    #[allow(clippy::unwrap_used)]
    loop {
        match (entries_dest.is_empty(), entries_source.is_empty()) {
            (false, false) => {
                // Both histories have entries left to process.
                let dest_entry = entries_dest.last().unwrap();
                let source_entry = entries_source.last().unwrap();

                let dest_time = dest_entry.times.last_modification.unwrap();

                let source_time = source_entry.times.last_modification.unwrap();

                if dest_time > source_time {
                    entries.push(entries_dest.pop().unwrap());
                } else if source_time > dest_time {
                    entries.push(entries_source.pop().unwrap());
                } else if have_entries_diverged(dest_entry, dest_db, source_entry, dest_db) {
                    log.warnings.push(format!(
                        "History entries for {} have the same modification timestamp {} but have diverged.",
                        dest_entry.id(),
                        source_time,
                    ));

                    // Both entries have the same timestamp but are different.
                    entries.push(entries_dest.pop().unwrap());
                    entries.push(entries_source.pop().unwrap());
                } else {
                    // The entries are the same, so we can just take one of them.
                    entries.push(entries_dest.pop().unwrap());
                    entries_source.pop();
                }
            }

            (true, false) => {
                // Only the source history has entries left to process - just take them all.
                entries.push(entries_source.pop().unwrap());
            }
            (false, true) => {
                // Only the destination history has entries left to process - just take them all.
                entries.push(entries_dest.pop().unwrap());
            }
            (true, true) => break,
        }
    }

    Ok(History { entries })
}

/// Merge custom icons, returning the merged history
fn merge_icons(
    dest_db: &mut Database,
    source_db: &Database,
    base_db: &Database,
    log: &mut MergeLog,
) -> Result<(), MergeError> {
    let dest_icons = dest_db.custom_icons.keys().cloned().collect::<HashSet<_>>();
    let source_icons = source_db.custom_icons.keys().cloned().collect::<HashSet<_>>();

    // Handle icons that exist only in source and might need to be added.
    for &id in source_icons.difference(&dest_icons) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let source_icon = source_db.custom_icons.get(&id).unwrap();

        if let Some(deletion_time) = dest_db.deleted_objects.get(&id.uuid()) {
            // Delete-vs-edit conflict: deleted in the destination, present in the source. If it
            // existed in the ancestor and the source changed it since, warn.
            if let Some(base_icon) = base_db.custom_icons.get(&id) {
                if source_icon != base_icon {
                    log.warnings.push(format!(
                        "Custom icon {id} was deleted in the destination but modified in the \
                         source since the common ancestor.",
                    ));
                }
            }

            let source_last_modification = source_icon.last_modification_time;

            if let (Some(deletion_time), Some(source_last_modification)) =
                (deletion_time, source_last_modification)
            {
                // if the icon was deleted after its last modification time in source,
                // do not re-add it
                if *deletion_time >= source_last_modification {
                    continue;
                }
            } else if deletion_time.is_some() && source_last_modification.is_none() {
                // blank last modification time in source - do not re-add the icon
                continue;
            } else if deletion_time.is_none() && source_last_modification.is_some() {
                // blank deletion time is probably older than concrete update time - re-add the icon
            } else {
                // both times are blank - do not re-add the icon
                continue;
            }
        }

        dest_db.custom_icons.insert(id, source_icon.clone());

        log.events.push(MergeEvent {
            target: MergeEventTarget::Icon(id),
            event_type: MergeEventType::Created,
        });
    }

    // Handle icons that exist only in destination. These icons might need to be deleted.
    for &id in dest_icons.difference(&source_icons) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist
        let dest_icon = dest_db.custom_icons.get(&id).unwrap();

        if let Some(deletion_time) = source_db.deleted_objects.get(&id.uuid()) {
            // Delete-vs-edit conflict: present in the destination, tombstoned in the source. If it
            // existed in the ancestor and the destination changed it since, warn.
            if let Some(base_icon) = base_db.custom_icons.get(&id) {
                if dest_icon != base_icon {
                    log.warnings.push(format!(
                        "Custom icon {id} was modified in the destination but deleted in the \
                         source since the common ancestor.",
                    ));
                }
            }

            let dest_last_modification = dest_icon.last_modification_time;

            if let (Some(deletion_time), Some(dest_last_modification)) = (deletion_time, dest_last_modification)
            {
                // if the icon was deleted and then later modified in dest, do not delete it
                if *deletion_time < dest_last_modification {
                    continue;
                }
            }

            dest_db.custom_icons.remove(&id);
            dest_db.deleted_objects.insert(id.uuid(), *deletion_time);

            log.events.push(MergeEvent {
                target: MergeEventTarget::Icon(id),
                event_type: MergeEventType::Deleted,
            });
        }
    }

    for &id in dest_icons.intersection(&source_icons) {
        #[allow(clippy::unwrap_used)] // id is guaranteed to exist in both dest and source
        let dest_icon = dest_db.custom_icons.get(&id).unwrap();

        #[allow(clippy::unwrap_used)] // id is guaranteed to exist in both dest and source
        let source_icon = source_db.custom_icons.get(&id).unwrap();

        let dest_last_modification = dest_icon.last_modification_time.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Destination custom icon {} did not have a last modification timestamp",
                id
            ));
            Times::epoch()
        });

        let source_last_modification = source_icon.last_modification_time.unwrap_or_else(|| {
            log.warnings.push(format!(
                "Source custom icon {} did not have a last modification timestamp",
                id
            ));
            Times::epoch()
        });

        if dest_last_modification == source_last_modification {
            if dest_icon != source_icon {
                log.warnings.push(format!(
                    "Custom icons with UUID {} have the same modification time but have diverged.",
                    id,
                ));
            }
            continue;
        }

        // Three-way check (see `merge_entries`). The base timestamp is kept as an `Option` — not
        // collapsed to `epoch` — so the two-way shim's empty ancestor (`None`) treats both sides
        // as changed and never early-continues, preserving two-way behavior for pre-1970 icon
        // timestamps.
        let base_lm = base_db
            .custom_icons
            .get(&id)
            .and_then(|i| i.last_modification_time);
        let dest_changed = base_lm.is_none_or(|b| dest_last_modification > b);
        let source_changed = base_lm.is_none_or(|b| source_last_modification > b);

        if !source_changed {
            // The source contributed nothing since the ancestor; keep the destination as-is.
            continue;
        }

        if dest_changed && base_lm.is_some() && dest_icon != source_icon {
            log.warnings.push(format!(
                "Custom icon {id} was modified in both databases since the common ancestor. \
                 Resolving by keeping the most recently modified version.",
            ));
        }

        if dest_last_modification > source_last_modification {
            // The destination icon is more recent than the source icon. Nothing to do.
            continue;
        }

        // The source icon is more recent than the destination icon. Update dest with source.
        dest_db.custom_icons.insert(id, source_icon.clone());

        log.events.push(MergeEvent {
            target: MergeEventTarget::Icon(id),
            event_type: MergeEventType::Updated,
        });
    }

    Ok(())
}

/// Overwrite `dest`'s content fields from `source`. Location and child membership are not
/// touched. Kept in sync with the field set compared by [`have_groups_diverged`].
fn overwrite_group_fields(dest: &mut GroupMut<'_>, source: &Group) {
    dest.name = source.name.clone();
    dest.notes = source.notes.clone();
    dest.icon = source.icon.clone();
    dest.custom_data = source.custom_data.clone();
    dest.times.last_modification = source.times.last_modification.or(dest.times.last_modification);
    dest.is_expanded = source.is_expanded;
    dest.default_autotype_sequence = source.default_autotype_sequence.clone();
    dest.enable_autotype = source.enable_autotype;
    dest.enable_searching = source.enable_searching;
    dest.last_top_visible_entry = source.last_top_visible_entry;
}

fn have_groups_diverged(a: &Group, b: &Group) -> bool {
    let new_times = Times::default();

    let mut a = a.clone();
    a.times = new_times.clone();
    a.entries.clear();
    a.groups.clear();
    a.parent = None;
    // previous_parent_group tracks location history, not content; exclude it so a moved group is
    // not mistaken for a true three-way content conflict.
    a.previous_parent_group = None;

    let mut b = b.clone();
    b.times = new_times.clone();
    b.entries.clear();
    b.groups.clear();
    b.parent = None;
    b.previous_parent_group = None;

    !a.eq(&b)
}

/// Check if two entries are dissimilar, ignoring their timestamps, history,
/// and attachment pool ids (attachments compare by name and bytes, resolved
/// against each entry's own database).
fn have_entries_diverged(a: &Entry, a_db: &Database, b: &Entry, b_db: &Database) -> bool {
    !a.content_equivalent(a_db, b, b_db)
}

/// Overwrite `dest_entry`'s content fields — everything except history and location — from
/// `incoming`, taking ownership of `incoming`'s data. `incoming`'s attachment references must
/// already be remapped into `dest_entry`'s database pool (see [`Entry::remap_attachments`]).
fn overwrite_entry_fields(dest_entry: &mut EntryMut<'_>, incoming: Entry) {
    dest_entry.times.last_modification = incoming.times.last_modification;
    dest_entry.fields = incoming.fields;
    dest_entry.autotype = incoming.autotype;
    dest_entry.tags = incoming.tags;
    dest_entry.custom_data = incoming.custom_data;
    dest_entry.icon = incoming.icon;
    dest_entry.foreground_color = incoming.foreground_color;
    dest_entry.background_color = incoming.background_color;
    dest_entry.override_url = incoming.override_url;
    dest_entry.quality_check = incoming.quality_check;
    dest_entry.attachments = incoming.attachments;
}

#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod merge_tests {
    use uuid::uuid;

    use super::MergeError;
    use crate::db::{fields, AttachmentId, EntryId, GroupId, History, Times};
    use crate::Database;

    const ROOT_GROUP_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000001"));
    const GROUP1_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000002"));
    const GROUP2_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000003"));
    const SUBGROUP1_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000004"));
    const SUBGROUP2_ID: GroupId = GroupId::from_uuid(uuid!("00000000-0000-0000-0000-000000000005"));
    const ENTRY1_ID: EntryId = EntryId::from_uuid(uuid!("00000000-0000-0000-0000-000000000006"));
    const ENTRY2_ID: EntryId = EntryId::from_uuid(uuid!("00000000-0000-0000-0000-000000000007"));

    /// Build up an example database for testing
    ///
    /// The database structure is as follows:
    ///
    /// root (ROOT_GROUP_ID)
    /// ├── entry1 (ENTRY1_ID)
    /// ├── group1 (GROUP1_ID)
    /// │   └── subgroup1 (SUBGROUP1_ID)
    /// │       └── entry2 (ENTRY2_ID)
    /// └── group2 (GROUP2_ID)
    ///    └── subgroup2 (SUBGROUP2_ID)
    ///
    fn create_test_database() -> Database {
        let mut db = Database::new_with_root_id(ROOT_GROUP_ID);

        // build up root -> group1 -> subgroup1 -> entry2
        db.root_mut()
            .add_group_with_id(GROUP1_ID)
            .unwrap()
            .edit(|g| g.name = "group1".to_string())
            .add_group_with_id(SUBGROUP1_ID)
            .unwrap()
            .edit(|sg| sg.name = "subgroup1".to_string())
            .add_entry_with_id(ENTRY2_ID)
            .unwrap()
            .edit(|e| e.set_unprotected("Title", "entry2"));

        // build up root -> group2 -> subgroup2
        db.root_mut()
            .add_group_with_id(GROUP2_ID)
            .unwrap()
            .edit(|g| g.name = "group2".to_string())
            .add_group_with_id(SUBGROUP2_ID)
            .unwrap()
            .edit(|sg| sg.name = "subgroup2".to_string());

        // Placing the first entry in the root group
        db.root_mut()
            .add_entry_with_id(ENTRY1_ID)
            .unwrap()
            .edit(|e| e.set_unprotected("Title", "entry1"));

        db
    }

    /// sleep for 1 second to ensure different timestamps
    fn sleep() {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    fn assert_history_ordered(history: &History) {
        let mut last_modification_time: Option<&chrono::NaiveDateTime> = None;
        for entry in &history.entries {
            if last_modification_time.is_none() {
                last_modification_time = entry.times.last_modification.as_ref();
            }

            if let Some(entry_modification_time) = entry.times.last_modification.as_ref() {
                if last_modification_time.unwrap() < entry_modification_time {
                    panic!(
                        "History entries are not ordered by last modification time: {:?} came after {:?}",
                        last_modification_time, entry_modification_time
                    );
                }
                last_modification_time = Some(entry_modification_time);
            }
        }
    }

    /// Test that merging a database with itself results in no changes.
    #[test]
    fn test_idempotence() {
        let mut destination_db = create_test_database();
        let source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);
        assert_eq!(destination_db.root().entries().count(), 1);
        assert_eq!(destination_db.root().groups().count(), 2);

        assert_eq!(destination_db.entries.len(), entry_count_before);
        assert_eq!(destination_db.groups.len(), group_count_before);

        // The two groups should be exactly the same after merging, since
        // nothing was performed during the merge.
        assert_eq!(destination_db, source_db);

        sleep();

        // Now modify an entry in the destination database, and merge again.
        destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .edit_tracking(|e| e.set_unprotected("Title", "entry1_updated"));

        // Merging should ignore the change, since destination is more recent.
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);
        let destination_db_just_after_merge = destination_db.clone();

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        // Merging twice in a row, even if the first merge updated the destination group,
        // should not create more changes.
        assert_eq!(destination_db_just_after_merge, destination_db);
    }

    /// Test that a new entry in source is added to destination when merging.
    #[test]
    fn test_add_new_entry() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // create a new entry in source_db and retain its id
        let new_entry_id = source_db
            .root_mut()
            .add_entry()
            .edit_tracking(|e| e.set_unprotected("Title", "new_entry"))
            .id();

        // merge source_db into destination_db -- this should add the new entry
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before + 1);
        assert_eq!(group_count_after, group_count_before);

        let root_entries_count = destination_db.root().entries().count();
        assert_eq!(root_entries_count, 2);

        let new_entry = destination_db
            .entry(new_entry_id)
            .expect("New entry should exist");
        assert_eq!(new_entry.get(fields::TITLE), Some("new_entry"));

        // Merging the same group again should not create a duplicate entry.
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before + 1);
        assert_eq!(group_count_after, group_count_before);

        let root_entries_count = destination_db.root().entries().count();
        assert_eq!(root_entries_count, 2);
    }

    /// Test that an entry that is marked as deleted in the destination database is not re-added
    /// when merging from source
    #[test]
    fn test_deleted_entry_in_destination() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // add a new entry in source_db that will be marked as deleted in destination_db
        let deleted_entry_id = source_db
            .root_mut()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "deleted_entry");
            })
            .id();

        // mark the entry as deleted in destination_db
        destination_db
            .deleted_objects
            .insert(deleted_entry_id.uuid(), Some(Times::now()));

        // merge source_db into destination_db -- the entry should not be added
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.entry(deleted_entry_id).is_none());
    }

    /// Test that an entry that is updated and moved to a group in source but that group is deleted
    /// later in dest should cause the group to be re-added and the entry to be moved there.
    #[test]
    fn test_updated_entry_under_deleted_group() {
        let mut destination_db = create_test_database();

        let modified_entry_id = destination_db
            .root_mut()
            .add_entry()
            .edit(|e| e.set_unprotected("Title", "original_title"))
            .id();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        let mut source_db = destination_db.clone();

        sleep();

        // perform the update of the entry in source_db and move it to the group that will be
        // deleted
        source_db
            .entry_mut(modified_entry_id)
            .unwrap()
            .track_changes()
            .edit(|e| {
                e.set_unprotected("Title", "modified_title");
            })
            .move_to(deleted_group_id)
            .unwrap();

        sleep();

        // delete the group in destination_db
        destination_db.group_mut(deleted_group_id).unwrap().remove();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the group should be re-added and the entry moved there
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 3); // recreate group, move entry, update entry

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before + 1);

        assert!(destination_db.group(deleted_group_id).is_some());
        assert!(destination_db.entry(modified_entry_id).is_some());
    }

    /// Test that a group that is marked as deleted in the destination database is not re-added
    /// when merging from source
    #[test]
    fn test_deleted_group_in_destination() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // add a new group in source_db
        let deleted_group_id = source_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        // mark the group as deleted in destination_db
        destination_db
            .deleted_objects
            .insert(deleted_group_id.uuid(), Some(Times::now()));

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(deleted_group_id).is_none());
    }

    /// Test that an entry that is marked as deleted in the source database is deleted from destination
    #[test]
    fn test_deleted_entry_in_source() {
        let mut destination_db = create_test_database();

        let deleted_entry_id = destination_db
            .root_mut()
            .add_entry()
            .edit_tracking(|e| e.set_unprotected("Title", "deleted_entry"))
            .id();

        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // mark the entry as deleted in source_db
        source_db
            .entry_mut(deleted_entry_id)
            .unwrap()
            .track_changes()
            .remove();

        // perform the merge - the entry should be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        // verify that the entry was deleted
        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before - 1);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.entry(deleted_entry_id).is_none());
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_entry_id.uuid()));
    }

    /// Test that a group that is marked as deleted in the source database is deleted from destination
    #[test]
    fn test_deleted_group_in_source() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // mark the entry as deleted in source_db
        source_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        // perform the merge - the entry should be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        // verify that the entry was deleted
        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before - 1);

        assert!(destination_db.group(deleted_group_id).is_none());
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
    }

    /// Test that an entry that is marked as deleted in the source database but modified in
    /// destination is not deleted
    #[test]
    fn test_deleted_entry_in_source_modified_in_destination() {
        let mut destination_db = create_test_database();

        let deleted_entry_id = destination_db
            .root_mut()
            .add_entry()
            .edit_tracking(|e| e.set_unprotected("Title", "deleted_entry"))
            .id();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let mut source_db = destination_db.clone();

        // mark the entry as deleted in source_db
        source_db
            .entry_mut(deleted_entry_id)
            .unwrap()
            .track_changes()
            .remove();

        sleep();

        // modify the entry in destination_db
        destination_db
            .entry_mut(deleted_entry_id)
            .unwrap()
            .edit_tracking(|e| e.set_unprotected("Title", "modified_in_destination"));

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.entry(deleted_entry_id).is_some());
        assert!(!destination_db
            .deleted_objects
            .contains_key(&deleted_entry_id.uuid()));
    }

    /// Test that a group subtree that is marked as deleted in the source database is deleted from
    /// destination
    #[test]
    fn test_group_subtree_deletion() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| {
                g.name = "deleted_group".to_string();
            })
            .id();

        let deleted_subgroup_id = destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .add_group()
            .edit(|g| {
                g.name = "deleted_subgroup".to_string();
            })
            .id();

        let deleted_entry_id = destination_db
            .group_mut(deleted_subgroup_id)
            .unwrap()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "deleted_entry");
            })
            .id();

        let mut source_db = destination_db.clone();

        // mark the entire group subtree as deleted in source_db
        source_db
            .root_mut()
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the entire subtree should be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 3);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before - 1);
        assert_eq!(group_count_after, group_count_before - 2);

        assert!(destination_db.entry(deleted_entry_id).is_none());
        assert!(destination_db.group(deleted_subgroup_id).is_none());
        assert!(destination_db.group(deleted_group_id).is_none());

        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_entry_id.uuid()));
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_subgroup_id.uuid()));
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
    }

    /// Test that a tree that was deleted in source, but contains a group that is newer in
    /// destination is only partially deleted.
    #[test]
    fn test_group_subtree_partial_deletion() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| {
                g.name = "deleted_group".to_string();
            })
            .id();

        let deleted_subgroup_id = destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .add_group()
            .edit(|g| {
                g.name = "deleted_subgroup".to_string();
            })
            .id();

        let deleted_entry_id = destination_db
            .group_mut(deleted_subgroup_id)
            .unwrap()
            .add_entry()
            .edit(|e| {
                e.set_unprotected("Title", "deleted_entry");
            })
            .id();

        let mut source_db = destination_db.clone();

        sleep();

        // mark the entire group subtree as deleted in source_db
        source_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        sleep();

        // modify the deleted subgroup in destination_db to be newer than the deletion time
        destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .edit(|g| {
                g.notes = Some("modified in destination".to_string());
            });

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the entry and subgroup should be deleted, but the group should
        // remain
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 2);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before - 1);
        assert_eq!(group_count_after, group_count_before - 1);

        assert!(destination_db.entry(deleted_entry_id).is_none());
        assert!(destination_db.group(deleted_subgroup_id).is_none());
        assert!(destination_db.group(deleted_group_id).is_some());

        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_entry_id.uuid()));
        assert!(destination_db
            .deleted_objects
            .contains_key(&deleted_subgroup_id.uuid()));
        assert!(!destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
    }

    /// Test that a group that is marked as deleted in the source database but modified in
    /// destination is not deleted
    #[test]
    fn test_deleted_group_in_source_modified_in_destination() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        let mut source_db = destination_db.clone();

        // mark the group as deleted in source_db
        source_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        sleep();

        // modify the group in destination_db
        destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .edit(|g| g.notes = Some("modified_in_destination".to_string()));

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the group should not be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(deleted_group_id).is_some());

        assert!(!destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
    }

    /// Test that a group that is marked as deleted in the source database but has new entries
    /// added in destination is not deleted
    #[test]
    fn test_deleted_group_has_new_entries() {
        let mut destination_db = create_test_database();

        let deleted_group_id = destination_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "deleted_group".to_string())
            .id();

        let mut source_db = destination_db.clone();

        // mark the group as deleted in source_db
        source_db
            .group_mut(deleted_group_id)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        sleep();

        // add a new entry to the deleted group in destination_db
        let new_entry_id = destination_db
            .group_mut(deleted_group_id)
            .unwrap()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "new_entry_in_deleted_group");
            })
            .id();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        // perform the merge - the group should not be deleted
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(deleted_group_id).is_some());
        assert!(destination_db.entry(new_entry_id).is_some());

        assert!(!destination_db
            .deleted_objects
            .contains_key(&deleted_group_id.uuid()));
        assert!(!destination_db.deleted_objects.contains_key(&new_entry_id.uuid()));
    }

    /// Test that a new entry in a non-root group in source is added to destination when merging.
    #[test]
    fn test_add_new_non_root_entry() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let new_entry_id = source_db
            .group_mut(GROUP1_ID)
            .unwrap()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "new_entry");
            })
            .id();

        // perform the merge - this should add the new entry
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before + 1);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.entry(new_entry_id).is_some());
    }

    // Test that a new entry in source under a new group/subgroup is added to destination when
    // merging.
    #[test]
    fn test_add_new_entry_new_group() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let new_group_id = source_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "new_group".to_string())
            .id();

        let new_subgroup_id = source_db
            .group_mut(new_group_id)
            .unwrap()
            .add_group()
            .edit(|g| g.name = "new_subgroup".to_string())
            .id();

        let new_entry_id = source_db
            .group_mut(new_subgroup_id)
            .unwrap()
            .add_entry()
            .edit_tracking(|e| {
                e.set_unprotected("Title", "new_entry");
            })
            .id();

        // perform the merge - this should add the new entry along with the new group and subgroup
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 3);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before + 1);
        assert_eq!(group_count_after, group_count_before + 2);

        assert!(destination_db.group(new_group_id).is_some());
        assert!(destination_db.group(new_subgroup_id).is_some());
        assert!(destination_db.entry(new_entry_id).is_some());
    }

    /// Test that an entry is relocated from one group to another in source and the relocation
    /// is reflected in destination when merging.
    #[test]
    fn test_entry_relocation_existing_group() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // before
        // root (ROOT_GROUP_ID)
        // ├── entry1 (ENTRY1_ID)
        // ├── group1 (GROUP1_ID)
        // │   └── subgroup1 (SUBGROUP1_ID)
        // │       └── entry2 (ENTRY2_ID)   <-- this entry
        // └── group2 (GROUP2_ID)
        //    └── subgroup2 (SUBGROUP2_ID)
        //
        // after
        // root (ROOT_GROUP_ID)
        // ├── entry1 (ENTRY1_ID)
        // ├── group1 (GROUP1_ID)
        // │   └── subgroup1 (SUBGROUP1_ID)
        // └── group2 (GROUP2_ID)
        //     ├── entry2 (ENTRY2_ID)   <-- moved here
        //     └── subgroup2 (SUBGROUP2_ID)
        //
        source_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = source_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should relocate the entry in destination_db
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(group_count_after, group_count_before);
        assert_eq!(entry_count_after, entry_count_before);

        assert!(destination_db.entry(ENTRY2_ID).is_some());

        let entry = destination_db.entry(ENTRY2_ID).unwrap();
        assert_eq!(entry.parent().id(), GROUP2_ID);
        assert_eq!(entry.times.location_changed, Some(location_changed_timestamp));
    }

    /// Test that an entry is relocated in source and modified in both source and destination
    /// and the correct content is kept after merging.
    #[test]
    fn test_entry_relocation_and_update() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // perform first edit of entry in source
        source_db.entry_mut(ENTRY2_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry2_modified_in_source");
        });

        // relocate entry in source
        source_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = source_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        sleep();

        // perform second edit of entry in destination
        destination_db.entry_mut(ENTRY2_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry2_modified_in_destination");
        });

        let entry_modified_timestamp = destination_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // perform the merge - this should relocate the entry in destination_db and keep the
        // content from destination_db since it was modified later
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(group_count_after, group_count_before);
        assert_eq!(entry_count_after, entry_count_before);

        // check that move occurred
        assert!(destination_db.entry(ENTRY2_ID).is_some());
        let entry = destination_db.entry(ENTRY2_ID).unwrap();
        assert_eq!(entry.parent().id(), GROUP2_ID);
        assert_eq!(entry.times.location_changed, Some(location_changed_timestamp));

        // check that content from destination is kept
        assert_eq!(entry.get(fields::TITLE), Some("entry2_modified_in_destination"));
        assert_eq!(entry.times.last_modification, Some(entry_modified_timestamp));
    }

    /// Test that if an entry is moved in source and modified in destination, the entry stays
    /// in the new location and gets the modifications.
    #[test]
    fn test_entry_relocation_in_destination_and_update() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // edit entry in source
        source_db.entry_mut(ENTRY2_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected(fields::TITLE, "entry2_modified_in_source");
        });

        let entry_modified_timestamp = source_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // relocate entry in destination
        destination_db
            .entry_mut(ENTRY2_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = destination_db
            .entry(ENTRY2_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should keep the location from destination and the content from
        // source
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(group_count_after, group_count_before);
        assert_eq!(entry_count_after, entry_count_before);

        // check that move occurred
        assert!(destination_db.entry(ENTRY2_ID).is_some());

        let entry = destination_db.entry(ENTRY2_ID).unwrap();
        assert_eq!(entry.parent().id(), GROUP2_ID);
        assert_eq!(entry.times.location_changed, Some(location_changed_timestamp));

        // check that content from source is kept
        assert_eq!(entry.get(fields::TITLE), Some("entry2_modified_in_source"));
        assert_eq!(entry.times.last_modification, Some(entry_modified_timestamp));
    }

    /// Test that an entry can be relocated into a newly created group
    #[test]
    fn test_entry_relocation_new_group() {
        let mut destination_db = create_test_database();

        let new_entry_id = destination_db
            .root_mut()
            .add_entry()
            .edit(|e| {
                e.set_unprotected("Title", "new_entry");
            })
            .id();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        let mut source_db = destination_db.clone();

        let new_group_id = source_db
            .root_mut()
            .add_group()
            .edit(|g| g.name = "new_group".to_string())
            .id();

        sleep();

        // modify the entry in source
        source_db.entry_mut(new_entry_id).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "new_entry_modified_in_source");
        });

        // relocate the entry to the new group in source
        source_db
            .entry_mut(new_entry_id)
            .unwrap()
            .track_changes()
            .move_to(new_group_id)
            .expect("move successful");

        // perform the merge - this should create the new group and update and relocate the entry there
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 3);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before + 1);

        assert!(destination_db.entry(new_entry_id).is_some());
        let entry = destination_db.entry(new_entry_id).unwrap();
        assert_eq!(entry.parent().id(), new_group_id);
        assert_eq!(entry.get(fields::TITLE), Some("new_entry_modified_in_source"));
    }

    /// Test that a group relocation in source is reflected in destination when merging.
    #[test]
    fn test_group_relocation() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // before
        // root (ROOT_GROUP_ID)
        // ├── entry1 (ENTRY1_ID)
        // ├── group1 (GROUP1_ID)
        // │   └── subgroup1 (SUBGROUP1_ID) <-- this group
        // │       └── entry2 (ENTRY2_ID)
        // └── group2 (GROUP2_ID)
        //    └── subgroup2 (SUBGROUP2_ID)
        //
        // after
        // root (ROOT_GROUP_ID)
        // ├── entry1 (ENTRY1_ID)
        // ├── group1 (GROUP1_ID)
        // └── group2 (GROUP2_ID)
        //    └── subgroup2 (SUBGROUP2_ID)
        //        └── subgroup1 (SUBGROUP1_ID) <-- moved here
        //            └── entry2 (ENTRY2_ID)

        source_db
            .group_mut(SUBGROUP1_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should relocate the group in destination_db
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());
        assert!(destination_db.entry(ENTRY2_ID).is_some());

        let group = destination_db.group(SUBGROUP1_ID).unwrap();
        assert_eq!(group.parent().unwrap().id(), GROUP2_ID);
        assert_eq!(group.times.location_changed, Some(location_changed_timestamp));
    }

    /// Test that an entry updated in destination is not touched when merging.
    #[test]
    fn test_update_in_destination_no_conflict() {
        let mut destination_db = create_test_database();
        let source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // update entry in destination
        destination_db.entry_mut(ENTRY1_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry1_updated");
        });

        // perform the merge - this should not change anything since source is older
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        // check that history is preserved
        let merged_history = destination_db.entry(ENTRY1_ID).unwrap().history.clone().unwrap();
        assert_history_ordered(&merged_history);
        assert_eq!(merged_history.entries.len(), 1);

        // check that we can find the old version of the entry
        let merged_entry = &merged_history.entries[0];
        assert_eq!(merged_entry.get(fields::TITLE), Some("entry1"));

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert_eq!(
            destination_db.entry(ENTRY1_ID).unwrap().get(fields::TITLE),
            Some("entry1_updated")
        );
    }

    /// Test that an entry updated in source is merged into destination when merging.
    #[test]
    fn test_update_in_source_no_conflict() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // update entry in source
        source_db.entry_mut(ENTRY1_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry1_updated");
        });

        // perform the merge - this should update the entry in destination_db
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        // check that history is preserved
        let merged_history = destination_db.entry(ENTRY1_ID).unwrap().history.clone().unwrap();
        assert_history_ordered(&merged_history);
        assert_eq!(merged_history.entries.len(), 1);

        // check that we can find the old version of the entry
        let merged_entry = &merged_history.entries[0];
        assert_eq!(merged_entry.get(fields::TITLE), Some("entry1"));

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        // check that the entry was updated
        assert_eq!(
            destination_db.entry(ENTRY1_ID).unwrap().get(fields::TITLE),
            Some("entry1_updated")
        );
    }

    /// Test that an entry updated in both source and destination is merged correctly.
    #[test]
    fn test_update_with_conflicts() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // update entry in destination
        destination_db.entry_mut(ENTRY1_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry1_updated_from_destination");
        });

        sleep();

        // update entry in source
        source_db.entry_mut(ENTRY1_ID).unwrap().edit_tracking(|e| {
            e.set_unprotected("Title", "entry1_updated_from_source");
        });

        // perform the merge - this should merge the changes from both databases, keeping the newer
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        // check that the entry was updated with the source change (newer)
        let entry = destination_db.entry(ENTRY1_ID).unwrap();
        assert_eq!(entry.get(fields::TITLE), Some("entry1_updated_from_source"));

        // check that history is preserved and contains both older versions
        let merged_history = entry.history.clone().unwrap();
        assert_history_ordered(&merged_history);
        assert_eq!(merged_history.entries.len(), 2);
        assert_eq!(
            merged_history.entries[0].get(fields::TITLE),
            Some("entry1_updated_from_destination")
        );
        assert_eq!(merged_history.entries[1].get(fields::TITLE), Some("entry1"));

        // Merging again should not result in any additional change.
        let merge_result = destination_db.merge(&destination_db.clone()).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);
    }

    /// Test that a group updated in source is merged into destination when merging.
    #[test]
    fn test_group_update_in_source() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        source_db.group_mut(SUBGROUP1_ID).unwrap().edit_tracking(|g| {
            g.name = "subgroup1_updated_name".to_string();
        });

        let modification_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // perform the merge - this should update the group in destination
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());

        assert_eq!(
            destination_db.group(SUBGROUP1_ID).unwrap().name,
            "subgroup1_updated_name"
        );
        assert_eq!(
            destination_db
                .group(SUBGROUP1_ID)
                .unwrap()
                .times
                .last_modification,
            Some(modification_timestamp)
        );
    }

    /// Test that a group updated in destination is not changed when merging.
    #[test]
    fn test_group_update_in_destination() {
        let mut destination_db = create_test_database();
        let source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        destination_db
            .group_mut(SUBGROUP1_ID)
            .unwrap()
            .edit_tracking(|g| {
                g.name = "subgroup1_updated_name".to_string();
            });

        let last_modification = destination_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // perform the merge - this should not change anything since source is older
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());
        assert_eq!(
            destination_db.group(SUBGROUP1_ID).unwrap().name,
            "subgroup1_updated_name"
        );

        assert_eq!(
            destination_db
                .group(SUBGROUP1_ID)
                .unwrap()
                .times
                .last_modification,
            Some(last_modification)
        );
    }

    /// Test that a group updated in source and relocated is merged correctly.
    #[test]
    fn test_group_update_and_relocation() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        source_db
            .group_mut(SUBGROUP1_ID)
            .unwrap()
            .track_changes()
            .edit(|g| {
                g.name = "subgroup1_updated_name".to_string();
            })
            .move_to(GROUP2_ID)
            .expect("move successful");

        let modification_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        let location_changed_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should update and relocate the group in destination
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 2);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());
        let group = destination_db.group(SUBGROUP1_ID).unwrap();
        assert_eq!(group.name, "subgroup1_updated_name");
        assert_eq!(group.parent().unwrap().id(), GROUP2_ID);
        assert_eq!(group.times.last_modification, Some(modification_timestamp));
        assert_eq!(group.times.location_changed, Some(location_changed_timestamp));
    }

    /// Test that a group updated in source and relocated in destionation is merged correctly.
    #[test]
    fn test_group_update_in_destination_and_relocation_in_source() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        let entry_count_before = destination_db.entries.len();
        let group_count_before = destination_db.groups.len();

        sleep();

        // rename group in source
        source_db.group_mut(SUBGROUP1_ID).unwrap().edit_tracking(|g| {
            g.name = "subgroup1_updated_name".to_string();
        });

        let modification_timestamp = source_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .last_modification
            .unwrap();

        // relocate group in destination
        destination_db
            .group_mut(SUBGROUP1_ID)
            .unwrap()
            .track_changes()
            .move_to(GROUP2_ID)
            .expect("move successful");

        let location_changed_timestamp = destination_db
            .group(SUBGROUP1_ID)
            .unwrap()
            .times
            .location_changed
            .unwrap();

        // perform the merge - this should update the group name from source and keep the new
        // location from destination
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let entry_count_after = destination_db.entries.len();
        let group_count_after = destination_db.groups.len();
        assert_eq!(entry_count_after, entry_count_before);
        assert_eq!(group_count_after, group_count_before);

        assert!(destination_db.group(SUBGROUP1_ID).is_some());
        let group = destination_db.group(SUBGROUP1_ID).unwrap();
        assert_eq!(group.name, "subgroup1_updated_name");
        assert_eq!(group.parent().unwrap().id(), GROUP2_ID);
        assert_eq!(group.times.last_modification, Some(modification_timestamp));
        assert_eq!(group.times.location_changed, Some(location_changed_timestamp));
    }

    #[test]
    fn test_merge_untracked_group_history() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        // this is an invalid edit as the last modified timestamp of the group is not updated
        source_db
            .group_mut(GROUP1_ID)
            .unwrap()
            .edit(|g| {
                g.name = "group1_updated_name".to_string();
            })
            .move_to(GROUP2_ID)
            .expect("move successful");

        assert_eq!(
            destination_db.group(GROUP1_ID).unwrap().times,
            source_db.group(GROUP1_ID).unwrap().times
        );

        // there will be an error during merge since the edit in source_db is not tracked and has
        // the same timestamp as the group in destination_db
        assert!(destination_db.merge(&source_db).is_err());

        // remove the timestamps to test warnings
        destination_db
            .group_mut(GROUP1_ID)
            .unwrap()
            .times
            .last_modification = None;
        destination_db
            .group_mut(GROUP1_ID)
            .unwrap()
            .times
            .location_changed = None;
        source_db.group_mut(GROUP1_ID).unwrap().times.last_modification = None;
        source_db.group_mut(GROUP1_ID).unwrap().times.location_changed = None;

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 3);
        assert_eq!(merge_result.events.len(), 0);
    }

    #[test]
    fn test_merge_untracked_entry_history() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        // this is an invalid edit as the last modified timestamp of the entry is not updated
        source_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .edit(|e| {
                e.set_unprotected("Title", "entry1_updated_title");
            })
            .move_to(GROUP2_ID)
            .expect("move successful");

        assert_eq!(
            destination_db.entry(ENTRY1_ID).unwrap().times,
            source_db.entry(ENTRY1_ID).unwrap().times
        );

        // there will be an error during merge since the edit in source_db is not tracked and has
        // the same timestamp as the entry in destination_db
        assert!(destination_db.merge(&source_db).is_err());

        // remove the timestamps to test warnings
        destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .times
            .last_modification = None;
        destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .times
            .location_changed = None;
        source_db.entry_mut(ENTRY1_ID).unwrap().times.last_modification = None;
        source_db.entry_mut(ENTRY1_ID).unwrap().times.location_changed = None;

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 3);
        assert_eq!(merge_result.events.len(), 0);
    }

    #[test]
    fn test_icon_added_in_source() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        sleep();

        // add a new icon in source
        let new_icon_id = {
            let mut source_entry = source_db.entry_mut(ENTRY1_ID).unwrap();
            let mut source_track = source_entry.track_changes();
            let new_icon_id = source_track.set_icon_custom_new(vec![1, 2, 3, 4]).id();

            new_icon_id
        };

        // perform the merge - this should add the new icon to destination and update the entry's icon reference
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 2);

        assert!(destination_db.custom_icon(new_icon_id).is_some());
    }

    #[test]
    fn test_icon_updated_in_source() {
        let mut destination_db = create_test_database();

        // add a new icon in destination
        let icon_id = destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .track_changes()
            .as_mut()
            .set_icon_custom_new(vec![1, 2, 3, 4])
            .id();

        let mut source_db = destination_db.clone();

        sleep();

        // update the icon in source
        let mut source_entry = source_db.entry_mut(ENTRY1_ID).unwrap();
        let mut source_icon = source_entry.custom_icon_mut().unwrap();
        source_icon.data = vec![5, 6, 7, 8];
        source_icon.last_modification_time = Some(Times::now());

        // perform the merge - this should update the icon in destination
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 1);

        let icon = destination_db.custom_icon(icon_id).unwrap();
        assert_eq!(icon.data, vec![5, 6, 7, 8]);
    }

    #[test]
    fn test_icon_updated_in_destination() {
        let mut destination_db = create_test_database();

        // add a new icon in destination
        let icon_id = destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .track_changes()
            .as_mut()
            .set_icon_custom_new(vec![1, 2, 3, 4])
            .id();

        let source_db = destination_db.clone();

        sleep();

        // update the icon in destination
        let mut destination_icon = destination_db.custom_icon_mut(icon_id).unwrap();
        destination_icon.data = vec![5, 6, 7, 8];
        destination_icon.last_modification_time = Some(Times::now());

        // perform the merge - this should keep the icon update in destination since it's newer
        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.warnings.len(), 0);
        assert_eq!(merge_result.events.len(), 0);

        let icon = destination_db.custom_icon(icon_id).unwrap();
        assert_eq!(icon.data, vec![5, 6, 7, 8]);
    }

    // ---- attachment merge tests ----
    //
    // These tests control modification times explicitly (instead of `sleep()`)
    // because attachment adds/removes via `EntryMut` do not touch timestamps.

    const ENTRY3_ID: EntryId = EntryId::from_uuid(uuid!("00000000-0000-0000-0000-000000000008"));

    fn ts_at(minute: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(12, minute, 0)
            .unwrap()
    }

    fn set_last_modification(db: &mut Database, id: EntryId, at: chrono::NaiveDateTime) {
        db.entry_mut(id).unwrap().times.last_modification = Some(at);
    }

    fn attach(db: &mut Database, id: EntryId, name: &str, bytes: &[u8]) {
        let mut entry = db.entry_mut(id).unwrap();
        entry.add_attachment(name, crate::db::Value::unprotected(bytes.to_vec()));
    }

    fn attachment_bytes(db: &Database, id: EntryId, name: &str) -> Option<Vec<u8>> {
        db.entry(id)?
            .attachment_by_name(name)
            .map(|a| a.data.get().clone())
    }

    /// An attachment added on the newer source side must land in the
    /// destination pool with its bytes, not be silently dropped.
    #[test]
    fn test_merge_attachment_added_in_source() {
        let mut destination_db = create_test_database();
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(0));
        let mut source_db = destination_db.clone();

        attach(&mut source_db, ENTRY1_ID, "invoice.pdf", b"pdf bytes");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        let merge_result = destination_db.merge(&source_db).unwrap();
        assert_eq!(merge_result.events.len(), 1);

        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "invoice.pdf").as_deref(),
            Some(b"pdf bytes".as_slice()),
            "attachment added on the newer source side must survive the merge"
        );
    }

    /// An attachment whose bytes were replaced on the newer source side must
    /// carry the new bytes into the destination.
    #[test]
    fn test_merge_attachment_replaced_in_source() {
        let mut destination_db = create_test_database();
        attach(&mut destination_db, ENTRY1_ID, "doc.txt", b"v1");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(0));
        let mut source_db = destination_db.clone();

        attach(&mut source_db, ENTRY1_ID, "doc.txt", b"v2");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        destination_db.merge(&source_db).unwrap();

        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "doc.txt").as_deref(),
            Some(b"v2".as_slice()),
            "replaced attachment bytes from the newer source side must win"
        );
    }

    /// An attachment removed on the newer source side must be removed from
    /// the destination entry as well.
    #[test]
    fn test_merge_attachment_removed_in_source() {
        let mut destination_db = create_test_database();
        attach(&mut destination_db, ENTRY1_ID, "old.txt", b"old bytes");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(0));
        let mut source_db = destination_db.clone();

        source_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .remove_attachment_by_name("old.txt");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        destination_db.merge(&source_db).unwrap();

        assert!(
            attachment_bytes(&destination_db, ENTRY1_ID, "old.txt").is_none(),
            "attachment removed on the newer source side must be removed by the merge"
        );
    }

    /// A brand-new entry from the source arrives with `AttachmentId`s minted
    /// in the *source* pool. Those ids must be remapped into the destination
    /// pool — otherwise they dangle or, worse, collide with an unrelated
    /// destination blob and show the wrong bytes.
    #[test]
    fn test_merge_new_entry_attachments_remapped_across_pools() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        // Destination pool: id 0 = "local blob".
        attach(&mut destination_db, ENTRY1_ID, "local.bin", b"local blob");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(0));

        // Source pool: id 0 = "remote blob", on a new entry — colliding ids,
        // different bytes.
        source_db
            .root_mut()
            .add_entry_with_id(ENTRY3_ID)
            .unwrap()
            .edit(|e| e.set_unprotected("Title", "entry3"));
        attach(&mut source_db, ENTRY3_ID, "remote.bin", b"remote blob");
        set_last_modification(&mut source_db, ENTRY3_ID, ts_at(1));

        destination_db.merge(&source_db).unwrap();

        assert_eq!(
            attachment_bytes(&destination_db, ENTRY3_ID, "remote.bin").as_deref(),
            Some(b"remote blob".as_slice()),
            "new entry's attachment must resolve to its own bytes, not a colliding local blob"
        );
        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "local.bin").as_deref(),
            Some(b"local blob".as_slice()),
            "existing local attachment must be untouched"
        );
    }

    /// Re-merging the same source after attachment remapping must be a no-op,
    /// even though the remapped destination ids no longer equal the source ids.
    #[test]
    fn test_remerge_after_attachment_remap_is_idempotent() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        attach(&mut destination_db, ENTRY1_ID, "local.bin", b"local blob");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(0));

        source_db
            .root_mut()
            .add_entry_with_id(ENTRY3_ID)
            .unwrap()
            .edit(|e| e.set_unprotected("Title", "entry3"));
        attach(&mut source_db, ENTRY3_ID, "remote.bin", b"remote blob");
        set_last_modification(&mut source_db, ENTRY3_ID, ts_at(1));

        destination_db.merge(&source_db).unwrap();
        let after_first_merge = destination_db.clone();

        let merge_result = destination_db.merge(&source_db).unwrap();

        assert_eq!(merge_result.events.len(), 0, "re-merge must produce no events");
        assert_eq!(
            destination_db, after_first_merge,
            "re-merging the same source must not change the database"
        );
    }

    /// Entries whose content is identical — including attachment names and
    /// bytes — must not count as diverged just because their pools assigned
    /// different `AttachmentId`s. With equal timestamps this previously
    /// surfaced as a hard `EntryModificationTimeNotUpdated` error.
    #[test]
    fn test_equal_content_with_different_attachment_ids_is_not_divergence() {
        let base = create_test_database();

        // Same (name, bytes) on both sides, added in opposite order so the
        // pool ids differ: dest has x.bin=0/same.txt=1, source the reverse.
        let mut destination_db = base.clone();
        attach(&mut destination_db, ENTRY2_ID, "x.bin", b"X");
        attach(&mut destination_db, ENTRY1_ID, "same.txt", b"S");

        let mut source_db = base;
        attach(&mut source_db, ENTRY1_ID, "same.txt", b"S");
        attach(&mut source_db, ENTRY2_ID, "x.bin", b"X");

        for db in [&mut destination_db, &mut source_db] {
            set_last_modification(db, ENTRY1_ID, ts_at(3));
            set_last_modification(db, ENTRY2_ID, ts_at(3));
        }

        let merge_result = destination_db
            .merge(&source_db)
            .expect("content-identical entries with drifted attachment ids must merge cleanly");

        assert_eq!(merge_result.events.len(), 0);
        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "same.txt").as_deref(),
            Some(b"S".as_slice())
        );
    }

    /// History versions imported from the source carry source-pool ids too;
    /// after the merge they must resolve to the correct bytes in the
    /// destination pool.
    #[test]
    fn test_merge_history_version_attachments_remapped() {
        let mut destination_db = create_test_database();
        let mut source_db = destination_db.clone();

        // Occupy destination id 0 with unrelated bytes to force a collision.
        attach(&mut destination_db, ENTRY2_ID, "pad.bin", b"PAD");
        set_last_modification(&mut destination_db, ENTRY2_ID, ts_at(1));
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(0));
        set_last_modification(&mut source_db, ENTRY2_ID, ts_at(0));

        // Source: v1 attached, archived into history, then replaced by v2.
        attach(&mut source_db, ENTRY1_ID, "doc.txt", b"v1");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(2));
        {
            let mut entry = source_db.entry_mut(ENTRY1_ID).unwrap();
            let pre_image = std::ops::Deref::deref(&entry).clone();
            entry.history.get_or_insert_default().add_entry(pre_image);
        }
        attach(&mut source_db, ENTRY1_ID, "doc.txt", b"v2");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        destination_db.merge(&source_db).unwrap();

        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "doc.txt").as_deref(),
            Some(b"v2".as_slice()),
            "live version must carry the newest bytes"
        );
        // The merged history holds the imported v1 version (and the archived
        // destination pre-image, which carried no attachment) — find the
        // version that references doc.txt and check its bytes.
        let entry = destination_db.entry(ENTRY1_ID).unwrap();
        let history_len = entry.history.as_ref().map_or(0, |h| h.entries.len());
        let historical_bytes = (0..history_len)
            .filter_map(|i| entry.historical(i))
            .find_map(|version| {
                version
                    .attachment_by_name("doc.txt")
                    .map(|a| a.data.get().clone())
            });
        assert_eq!(
            historical_bytes.as_deref(),
            Some(b"v1".as_slice()),
            "history version's attachment must resolve to v1 bytes, not a colliding blob"
        );
        assert_eq!(
            attachment_bytes(&destination_db, ENTRY2_ID, "pad.bin").as_deref(),
            Some(b"PAD".as_slice())
        );
    }

    fn attachment_id(db: &Database, id: EntryId, name: &str) -> Option<AttachmentId> {
        Some(db.entry(id)?.attachment_by_name(name)?.id())
    }

    /// Two source entries that reference byte-identical attachments (with
    /// distinct source-pool ids) must collapse to a single shared blob in the
    /// destination pool — cross-entry sharing is preserved, not duplicated.
    #[test]
    fn test_merge_cross_entry_attachment_sharing_is_preserved() {
        let mut destination_db = create_test_database();
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(0));
        set_last_modification(&mut destination_db, ENTRY2_ID, ts_at(0));
        let mut source_db = destination_db.clone();

        // Same bytes attached to two different entries: `add_attachment` mints
        // distinct source-pool ids (no within-db dedup on add).
        attach(&mut source_db, ENTRY1_ID, "a.bin", b"shared payload");
        attach(&mut source_db, ENTRY2_ID, "b.bin", b"shared payload");
        assert_ne!(
            attachment_id(&source_db, ENTRY1_ID, "a.bin"),
            attachment_id(&source_db, ENTRY2_ID, "b.bin"),
            "precondition: the two source attachments must have distinct ids"
        );
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));
        set_last_modification(&mut source_db, ENTRY2_ID, ts_at(5));

        destination_db.merge(&source_db).unwrap();

        assert_eq!(
            attachment_id(&destination_db, ENTRY1_ID, "a.bin"),
            attachment_id(&destination_db, ENTRY2_ID, "b.bin"),
            "byte-identical attachments must share one destination id after merge"
        );
        assert_eq!(
            destination_db.num_attachments(),
            1,
            "the shared payload must be stored once, not duplicated per entry"
        );
    }

    /// A source attachment whose bytes already exist in the destination pool
    /// (attached to an unrelated entry) must reuse that blob's id rather than
    /// allocating a duplicate.
    #[test]
    fn test_merge_reuses_existing_destination_blob() {
        let mut destination_db = create_test_database();
        attach(&mut destination_db, ENTRY1_ID, "local.bin", b"shared");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        set_last_modification(&mut destination_db, ENTRY2_ID, ts_at(0));
        assert_eq!(destination_db.num_attachments(), 1);

        // Source brings the same bytes on a different entry (newer, so it
        // updates the destination entry).
        let mut source_db = destination_db.clone();
        attach(&mut source_db, ENTRY2_ID, "copy.bin", b"shared");
        set_last_modification(&mut source_db, ENTRY2_ID, ts_at(5));

        destination_db.merge(&source_db).unwrap();

        assert_eq!(
            attachment_id(&destination_db, ENTRY2_ID, "copy.bin"),
            attachment_id(&destination_db, ENTRY1_ID, "local.bin"),
            "imported attachment must reuse the byte-identical blob already in the pool"
        );
        assert_eq!(
            destination_db.num_attachments(),
            1,
            "reusing an existing blob must not grow the pool"
        );
    }

    /// A source `name -> AttachmentId` reference whose blob is missing from the
    /// source pool (a dangling reference) must be dropped during remap, not
    /// carried into the destination as a reference to nonexistent bytes.
    #[test]
    fn test_merge_drops_dangling_source_reference() {
        let mut destination_db = create_test_database();
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(0));
        let mut source_db = destination_db.clone();

        attach(&mut source_db, ENTRY1_ID, "ghost.bin", b"soon gone");
        // Drop the blob from the pool but leave the entry's reference behind,
        // fabricating a dangling `name -> id` that remap must not propagate.
        let dangling = attachment_id(&source_db, ENTRY1_ID, "ghost.bin").unwrap();
        source_db.attachments.remove(&dangling);
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        destination_db
            .merge(&source_db)
            .expect("a dangling source reference must not break the merge");

        assert!(
            attachment_id(&destination_db, ENTRY1_ID, "ghost.bin").is_none(),
            "a reference whose blob is absent from the source pool must be dropped"
        );
    }

    /// Locks in the documented dedup contract flagged by the Codex P2 review:
    /// two distinct source attachments on the *same* entry that carry identical
    /// bytes collapse to a single destination id. This matches the KDBX
    /// format's byte-identical binary sharing; the test exists so any future
    /// change to that behavior is deliberate and visible.
    #[test]
    fn test_merge_distinct_equal_byte_attachments_dedup_on_same_entry() {
        let mut destination_db = create_test_database();
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(0));
        let mut source_db = destination_db.clone();

        attach(&mut source_db, ENTRY1_ID, "one.bin", b"identical");
        attach(&mut source_db, ENTRY1_ID, "two.bin", b"identical");
        assert_ne!(
            attachment_id(&source_db, ENTRY1_ID, "one.bin"),
            attachment_id(&source_db, ENTRY1_ID, "two.bin"),
            "precondition: distinct source ids for the two equal-byte attachments"
        );
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        destination_db.merge(&source_db).unwrap();

        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "one.bin").as_deref(),
            Some(b"identical".as_slice()),
        );
        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "two.bin").as_deref(),
            Some(b"identical".as_slice()),
        );
        assert_eq!(
            attachment_id(&destination_db, ENTRY1_ID, "one.bin"),
            attachment_id(&destination_db, ENTRY1_ID, "two.bin"),
            "equal-byte attachments are content-addressed to one shared id (documented dedup)"
        );
        assert_eq!(destination_db.num_attachments(), 1);
    }

    // ---- three-way merge (merge_with_ancestor) tests ----
    //
    // `self` = destination, `other` = source, plus a common `ancestor`. Timestamps are set
    // explicitly (no `sleep()`) so the one-sided-vs-conflict decision is deterministic.

    /// Set an entry's title without touching its timestamps or history, so the test controls
    /// modification times explicitly.
    fn set_title(db: &mut Database, id: EntryId, t: &str) {
        db.entry_mut(id).unwrap().edit(|e| {
            e.set_unprotected(fields::TITLE, t);
        });
    }

    fn title(db: &Database, id: EntryId) -> Option<String> {
        db.entry(id)?.get(fields::TITLE).map(str::to_string)
    }

    /// Changed on the `other` side only since the ancestor: take other's value, no warning.
    #[test]
    fn test_ancestor_one_sided_change_in_other() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_title(&mut source_db, ENTRY1_ID, "changed_in_other");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 0, "a one-sided change must not warn");
        assert_eq!(result.events.len(), 1);
        assert_eq!(
            title(&destination_db, ENTRY1_ID).as_deref(),
            Some("changed_in_other")
        );
    }

    /// Changed on the `self` side only since the ancestor: keep self's value, no warning, no event.
    #[test]
    fn test_ancestor_one_sided_change_in_self() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let source_db = ancestor_db.clone();

        set_title(&mut destination_db, ENTRY1_ID, "changed_in_self");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 0, "a one-sided change must not warn");
        assert_eq!(result.events.len(), 0, "self already holds the winning value");
        assert_eq!(
            title(&destination_db, ENTRY1_ID).as_deref(),
            Some("changed_in_self")
        );
    }

    /// Changed differently on both sides since the ancestor: exactly one warning, newest wins.
    #[test]
    fn test_ancestor_true_conflict_warns_and_newest_wins() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_title(&mut destination_db, ENTRY1_ID, "changed_in_self");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));

        set_title(&mut source_db, ENTRY1_ID, "changed_in_other");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(10));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 1, "a true conflict must warn exactly once");
        assert_eq!(
            title(&destination_db, ENTRY1_ID).as_deref(),
            Some("changed_in_other"),
            "the most recently modified side must win"
        );
    }

    /// Re-running the same three-way merge changes nothing (no events, no warnings).
    #[test]
    fn test_ancestor_merge_is_idempotent() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_title(&mut destination_db, ENTRY1_ID, "changed_in_self");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        set_title(&mut source_db, ENTRY1_ID, "changed_in_other");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(10));

        destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();
        let after_first = destination_db.clone();

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.events.len(), 0, "re-merge must produce no events");
        assert_eq!(result.warnings.len(), 0, "re-merge must produce no warnings");
        assert_eq!(
            destination_db, after_first,
            "re-merge must not change the database"
        );
    }

    /// Backward-compatibility: the two-way `merge` shim must not raise the three-way
    /// "modified in both databases" warning, even on a conflicting change.
    #[test]
    fn test_two_way_shim_no_ancestor_warning_on_conflict() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_title(&mut destination_db, ENTRY1_ID, "changed_in_self");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        set_title(&mut source_db, ENTRY1_ID, "changed_in_other");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(10));

        let result = destination_db.merge(&source_db).unwrap();

        assert_eq!(
            result.warnings.len(),
            0,
            "the two-way shim must not emit ancestor conflicts"
        );
        assert_eq!(
            title(&destination_db, ENTRY1_ID).as_deref(),
            Some("changed_in_other"),
            "two-way newest-wins still applies"
        );
    }

    /// Three-way × attachments: an attachment added only on the source side is imported silently.
    #[test]
    fn test_ancestor_one_sided_attachment_add_in_source() {
        let mut ancestor_db = create_test_database();
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        attach(&mut source_db, ENTRY1_ID, "file.pdf", b"pdf bytes");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(
            result.warnings.len(),
            0,
            "a one-sided attachment add must not warn"
        );
        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "file.pdf").as_deref(),
            Some(b"pdf bytes".as_slice()),
        );
    }

    /// Three-way × attachments: the same attachment changed differently on both sides is a true
    /// conflict — one warning, newest bytes win, and they resolve to the correct pool bytes.
    #[test]
    fn test_ancestor_attachment_conflict_warns_and_newest_bytes_win() {
        let mut ancestor_db = create_test_database();
        attach(&mut ancestor_db, ENTRY1_ID, "doc.txt", b"v0");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        attach(&mut destination_db, ENTRY1_ID, "doc.txt", b"v-self");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        attach(&mut source_db, ENTRY1_ID, "doc.txt", b"v-other");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(10));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(
            result.warnings.len(),
            1,
            "diverged attachments are a true conflict"
        );
        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "doc.txt").as_deref(),
            Some(b"v-other".as_slice()),
            "the newest side's attachment bytes must win and resolve correctly"
        );
    }

    /// Three-way × attachments: when the source did not change since the ancestor, the
    /// destination's own attachment changes are kept and nothing is imported from the source.
    #[test]
    fn test_ancestor_source_unchanged_keeps_destination_attachments() {
        let mut ancestor_db = create_test_database();
        attach(&mut ancestor_db, ENTRY1_ID, "keep.txt", b"orig");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let source_db = ancestor_db.clone();

        attach(&mut destination_db, ENTRY1_ID, "extra.txt", b"dest-only");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 0);
        assert_eq!(
            result.events.len(),
            0,
            "source unchanged since ancestor → nothing to apply"
        );
        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "extra.txt").as_deref(),
            Some(b"dest-only".as_slice()),
            "the destination's own attachment must be kept"
        );
        assert_eq!(
            attachment_bytes(&destination_db, ENTRY1_ID, "keep.txt").as_deref(),
            Some(b"orig".as_slice()),
        );
    }

    // ---- equal-timestamp three-way conflict tests (PR #13 review, P1) ----
    //
    // KDBX timestamps have one-second precision, so two branches can make divergent edits within
    // the same second. With an ancestor these must resolve (warn + deterministic keep-destination)
    // instead of hard-erroring; without one, the two-way `merge` shim keeps its legacy error.

    fn set_group_name(db: &mut Database, id: GroupId, name: &str) {
        db.group_mut(id).unwrap().name = name.to_string();
    }

    fn set_group_lm(db: &mut Database, id: GroupId, at: chrono::NaiveDateTime) {
        db.group_mut(id).unwrap().times.last_modification = Some(at);
    }

    /// Both sides edited the same entry differently within the same second: one warning, and the
    /// deterministic tie-break keeps the destination.
    #[test]
    fn test_equal_timestamp_true_conflict_keeps_destination() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_title(&mut destination_db, ENTRY1_ID, "dest_edit");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        set_title(&mut source_db, ENTRY1_ID, "source_edit");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(
            result.warnings.len(),
            1,
            "same-timestamp two-sided edit must warn once"
        );
        assert_eq!(result.events.len(), 0);
        assert_eq!(
            title(&destination_db, ENTRY1_ID).as_deref(),
            Some("dest_edit"),
            "the deterministic tie-break keeps the destination copy"
        );
    }

    /// Same timestamp, but only the source diverged from the ancestor: adopt the source, no warning.
    #[test]
    fn test_equal_timestamp_one_sided_source_adopts_source() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        // Destination keeps the ancestor's content but shares the source's timestamp.
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        set_title(&mut source_db, ENTRY1_ID, "source_edit");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 0, "a one-sided change must not warn");
        assert_eq!(result.events.len(), 1);
        assert_eq!(title(&destination_db, ENTRY1_ID).as_deref(), Some("source_edit"));
    }

    /// Same timestamp, but only the destination diverged: keep the destination, no warning/event.
    #[test]
    fn test_equal_timestamp_one_sided_dest_keeps_destination() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_title(&mut destination_db, ENTRY1_ID, "dest_edit");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 0);
        assert_eq!(result.events.len(), 0);
        assert_eq!(title(&destination_db, ENTRY1_ID).as_deref(), Some("dest_edit"));
    }

    /// The two-way `merge` shim (no ancestor) must preserve the legacy hard error on an
    /// equal-timestamp divergence.
    #[test]
    fn test_two_way_equal_timestamp_divergence_still_errors() {
        let base = create_test_database();
        let mut destination_db = base.clone();
        let mut source_db = base;

        set_title(&mut destination_db, ENTRY1_ID, "dest_edit");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        set_title(&mut source_db, ENTRY1_ID, "source_edit");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        let err = destination_db.merge(&source_db).unwrap_err();
        assert!(
            matches!(err, MergeError::EntryModificationTimeNotUpdated(id) if id == ENTRY1_ID),
            "two-way equal-timestamp divergence must still hard-error, got {:?}",
            err
        );
    }

    /// Groups get the same equal-timestamp treatment: three-way warns (not errors)...
    #[test]
    fn test_equal_timestamp_group_conflict_warns() {
        let mut ancestor_db = create_test_database();
        set_group_name(&mut ancestor_db, SUBGROUP1_ID, "base");
        set_group_lm(&mut ancestor_db, SUBGROUP1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_group_name(&mut destination_db, SUBGROUP1_ID, "dest_name");
        set_group_lm(&mut destination_db, SUBGROUP1_ID, ts_at(5));
        set_group_name(&mut source_db, SUBGROUP1_ID, "source_name");
        set_group_lm(&mut source_db, SUBGROUP1_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 1);
        assert_eq!(destination_db.group(SUBGROUP1_ID).unwrap().name, "dest_name");
    }

    /// ...while the two-way shim still hard-errors on an equal-timestamp group divergence.
    #[test]
    fn test_two_way_equal_timestamp_group_divergence_still_errors() {
        let base = create_test_database();
        let mut destination_db = base.clone();
        let mut source_db = base;

        set_group_name(&mut destination_db, SUBGROUP1_ID, "dest_name");
        set_group_lm(&mut destination_db, SUBGROUP1_ID, ts_at(5));
        set_group_name(&mut source_db, SUBGROUP1_ID, "source_name");
        set_group_lm(&mut source_db, SUBGROUP1_ID, ts_at(5));

        let err = destination_db.merge(&source_db).unwrap_err();
        assert!(
            matches!(err, MergeError::GroupModificationTimeNotUpdated(id) if id == SUBGROUP1_ID),
            "two-way equal-timestamp group divergence must still hard-error, got {:?}",
            err
        );
    }

    // ---- tombstone delete-vs-edit conflict tests (PR #13 review, P2) ----

    /// Deleted in the destination, edited in the source since the ancestor → conflict warning.
    #[test]
    fn test_tombstone_conflict_deleted_in_dest_edited_in_source() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .track_changes()
            .remove();
        set_title(&mut source_db, ENTRY1_ID, "edited_in_source");
        set_last_modification(&mut source_db, ENTRY1_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(
            result.warnings.len(),
            1,
            "editing an entry the other side deleted is a conflict"
        );
    }

    /// Edited in the destination, deleted in the source since the ancestor → conflict warning.
    #[test]
    fn test_tombstone_conflict_edited_in_dest_deleted_in_source() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_title(&mut destination_db, ENTRY1_ID, "edited_in_dest");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        source_db.entry_mut(ENTRY1_ID).unwrap().track_changes().remove();

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 1);
    }

    /// A one-sided delete (the surviving side is unchanged vs the ancestor) is not a conflict.
    #[test]
    fn test_tombstone_one_sided_delete_is_silent() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let source_db = ancestor_db.clone();

        // Only the destination deletes; the source keeps the ancestor's version unchanged.
        destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .track_changes()
            .remove();

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(
            result.warnings.len(),
            0,
            "a one-sided delete with no competing edit must not warn"
        );
    }

    // ---- custom-icon three-way tests (PR #13 review, P3) ----

    fn old_time(year: i32, month: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(year, month, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    /// Regression: the two-way shim must still take a newer source icon even when both timestamps
    /// predate the Unix epoch (the old code collapsed an absent ancestor to `epoch` and kept dest).
    #[test]
    fn test_two_way_icon_pre_epoch_source_newer_wins() {
        let mut destination_db = create_test_database();
        let icon_id = destination_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(vec![1, 2, 3, 4])
            .id();
        let mut source_db = destination_db.clone();

        destination_db
            .custom_icon_mut(icon_id)
            .unwrap()
            .last_modification_time = Some(old_time(1969, 1));
        {
            let mut source_icon = source_db.custom_icon_mut(icon_id).unwrap();
            source_icon.data = vec![5, 6, 7, 8];
            source_icon.last_modification_time = Some(old_time(1969, 6));
        }

        let result = destination_db.merge(&source_db).unwrap();

        assert_eq!(result.warnings.len(), 0);
        assert_eq!(
            destination_db.custom_icon(icon_id).unwrap().data,
            vec![5, 6, 7, 8],
            "a newer source icon must win even for pre-1970 timestamps"
        );
    }

    /// Three-way: an icon changed differently on both sides since the ancestor warns, newest wins.
    #[test]
    fn test_ancestor_icon_conflict_warns_and_newest_wins() {
        let mut ancestor_db = create_test_database();
        let icon_id = ancestor_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(vec![0, 0, 0, 0])
            .id();
        ancestor_db
            .custom_icon_mut(icon_id)
            .unwrap()
            .last_modification_time = Some(ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        {
            let mut di = destination_db.custom_icon_mut(icon_id).unwrap();
            di.data = vec![9, 9, 9, 9];
            di.last_modification_time = Some(ts_at(5));
        }
        {
            let mut si = source_db.custom_icon_mut(icon_id).unwrap();
            si.data = vec![8, 8, 8, 8];
            si.last_modification_time = Some(ts_at(10));
        }

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(
            result.warnings.len(),
            1,
            "diverged icons since the ancestor are a conflict"
        );
        assert_eq!(
            destination_db.custom_icon(icon_id).unwrap().data,
            vec![8, 8, 8, 8],
            "the most recently modified icon must win"
        );
    }

    // ---- round 2: equal-timestamp history preservation + group/icon tombstone conflicts ----

    fn history_titles(db: &Database, id: EntryId) -> Vec<String> {
        let entry = db.entry(id).unwrap();
        let n = entry.history.as_ref().map_or(0, |h| h.entries.len());
        (0..n)
            .filter_map(|i| entry.historical(i))
            .filter_map(|v| v.get(fields::TITLE).map(str::to_string))
            .collect()
    }

    /// Give `source` an intermediate tracked version, then a final live value.
    fn source_with_intermediate_history(source_db: &mut Database, intermediate: &str, final_title: &str) {
        set_title(source_db, ENTRY1_ID, intermediate);
        set_last_modification(source_db, ENTRY1_ID, ts_at(3));
        {
            let mut entry = source_db.entry_mut(ENTRY1_ID).unwrap();
            let pre_image = std::ops::Deref::deref(&entry).clone();
            entry.history.get_or_insert_default().add_entry(pre_image);
        }
        set_title(source_db, ENTRY1_ID, final_title);
        set_last_modification(source_db, ENTRY1_ID, ts_at(5));
    }

    /// Equal-timestamp true conflict: the destination's live value is kept, but the source's
    /// intermediate history version must still be merged in (not discarded at the tie-break exit).
    #[test]
    fn test_equal_timestamp_conflict_preserves_source_history() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_title(&mut destination_db, ENTRY1_ID, "dest_edit");
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        source_with_intermediate_history(&mut source_db, "src_intermediate", "src_final");

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(
            result.warnings.len(),
            1,
            "same-timestamp two-sided edit warns once"
        );
        assert_eq!(
            title(&destination_db, ENTRY1_ID).as_deref(),
            Some("dest_edit"),
            "tie-break keeps the destination's live value"
        );
        assert!(
            history_titles(&destination_db, ENTRY1_ID).contains(&"src_intermediate".to_string()),
            "the source's intermediate history version must survive the tie-break, got {:?}",
            history_titles(&destination_db, ENTRY1_ID)
        );
    }

    /// Equal-timestamp source adoption: the source's history is merged and the destination's
    /// pre-image is archived.
    #[test]
    fn test_equal_timestamp_source_adoption_preserves_histories() {
        let mut ancestor_db = create_test_database();
        set_title(&mut ancestor_db, ENTRY1_ID, "base");
        set_last_modification(&mut ancestor_db, ENTRY1_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        // Destination keeps the ancestor's content but shares the source's timestamp, so only the
        // source is the changed side.
        set_last_modification(&mut destination_db, ENTRY1_ID, ts_at(5));
        source_with_intermediate_history(&mut source_db, "src_intermediate", "src_final");

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 0, "a one-sided change must not warn");
        assert_eq!(title(&destination_db, ENTRY1_ID).as_deref(), Some("src_final"));
        let hist = history_titles(&destination_db, ENTRY1_ID);
        assert!(
            hist.contains(&"src_intermediate".to_string()),
            "source's intermediate history version must survive, got {:?}",
            hist
        );
        assert!(
            hist.contains(&"base".to_string()),
            "destination's pre-image must be archived, got {:?}",
            hist
        );
    }

    // Uses the empty SUBGROUP2 (no entries) so a group deletion doesn't drag entries along.

    /// Group deleted in the destination but edited in the source since the ancestor → conflict.
    #[test]
    fn test_group_tombstone_conflict_deleted_in_dest_edited_in_source() {
        let mut ancestor_db = create_test_database();
        set_group_name(&mut ancestor_db, SUBGROUP2_ID, "base");
        set_group_lm(&mut ancestor_db, SUBGROUP2_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        destination_db
            .group_mut(SUBGROUP2_ID)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();
        set_group_name(&mut source_db, SUBGROUP2_ID, "edited_in_source");
        set_group_lm(&mut source_db, SUBGROUP2_ID, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(
            result.warnings.len(),
            1,
            "editing a group the other side deleted is a conflict"
        );
    }

    /// Group edited in the destination but deleted in the source since the ancestor → conflict.
    #[test]
    fn test_group_tombstone_conflict_edited_in_dest_deleted_in_source() {
        let mut ancestor_db = create_test_database();
        set_group_name(&mut ancestor_db, SUBGROUP2_ID, "base");
        set_group_lm(&mut ancestor_db, SUBGROUP2_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        set_group_name(&mut destination_db, SUBGROUP2_ID, "edited_in_dest");
        set_group_lm(&mut destination_db, SUBGROUP2_ID, ts_at(5));
        source_db
            .group_mut(SUBGROUP2_ID)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 1);
    }

    /// A one-sided group delete (the surviving side is unchanged vs the ancestor) is silent.
    #[test]
    fn test_group_tombstone_one_sided_delete_is_silent() {
        let mut ancestor_db = create_test_database();
        set_group_name(&mut ancestor_db, SUBGROUP2_ID, "base");
        set_group_lm(&mut ancestor_db, SUBGROUP2_ID, ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let source_db = ancestor_db.clone();

        destination_db
            .group_mut(SUBGROUP2_ID)
            .unwrap()
            .track_changes()
            .remove()
            .unwrap();

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 0);
    }

    /// Delete a custom icon and leave a tombstone (there is no public tracked-remove for icons).
    fn tombstone_icon(db: &mut Database, icon_id: crate::db::CustomIconId, at: chrono::NaiveDateTime) {
        db.custom_icons.remove(&icon_id);
        db.deleted_objects.insert(icon_id.uuid(), Some(at));
    }

    /// Custom icon deleted in the destination but changed in the source since the ancestor → conflict.
    #[test]
    fn test_icon_tombstone_conflict_deleted_in_dest_changed_in_source() {
        let mut ancestor_db = create_test_database();
        let icon_id = ancestor_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(vec![1, 2, 3, 4])
            .id();
        ancestor_db
            .custom_icon_mut(icon_id)
            .unwrap()
            .last_modification_time = Some(ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        tombstone_icon(&mut destination_db, icon_id, ts_at(5));
        {
            let mut si = source_db.custom_icon_mut(icon_id).unwrap();
            si.data = vec![5, 6, 7, 8];
            si.last_modification_time = Some(ts_at(5));
        }

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(
            result.warnings.len(),
            1,
            "changing an icon the other side deleted is a conflict"
        );
    }

    /// Custom icon changed in the destination but deleted in the source since the ancestor → conflict.
    #[test]
    fn test_icon_tombstone_conflict_changed_in_dest_deleted_in_source() {
        let mut ancestor_db = create_test_database();
        let icon_id = ancestor_db
            .entry_mut(ENTRY1_ID)
            .unwrap()
            .set_icon_custom_new(vec![1, 2, 3, 4])
            .id();
        ancestor_db
            .custom_icon_mut(icon_id)
            .unwrap()
            .last_modification_time = Some(ts_at(0));

        let mut destination_db = ancestor_db.clone();
        let mut source_db = ancestor_db.clone();

        {
            let mut di = destination_db.custom_icon_mut(icon_id).unwrap();
            di.data = vec![9, 9, 9, 9];
            di.last_modification_time = Some(ts_at(5));
        }
        tombstone_icon(&mut source_db, icon_id, ts_at(5));

        let result = destination_db
            .merge_with_ancestor(&source_db, &ancestor_db)
            .unwrap();

        assert_eq!(result.warnings.len(), 1);
    }
}
