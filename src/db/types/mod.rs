pub(crate) mod attachment;
pub(crate) mod autotype;
pub(crate) mod color;
pub(crate) mod custom_data;
pub(crate) mod entry;
pub(crate) mod group;
pub(crate) mod history;
pub(crate) mod icon;
pub(crate) mod meta;
pub(crate) mod times;
pub(crate) mod value;

use std::collections::HashMap;

pub use attachment::{Attachment, AttachmentId, AttachmentMut, AttachmentRef};
pub use autotype::{AutoType, AutoTypeAssociation, DataTransferObfuscation};
pub use color::{Color, ParseColorError};
pub use custom_data::{CustomDataItem, CustomDataValue};
pub use entry::{DestinationGroupNotFoundError, Entry, EntryId, EntryMut, EntryRef, EntryTrack};
pub use group::{
    DuplicateEntryIdError, DuplicateGroupIdError, Group, GroupId, GroupMut, GroupRef, GroupTrack,
    MoveGroupError,
};
pub use history::History;
pub use icon::{CustomIcon, CustomIconId, CustomIconMut, CustomIconNotFoundError, CustomIconRef, Icon};
pub use meta::{MemoryProtection, Meta};
pub use times::Times;
pub use value::Value;

use crate::config::DatabaseConfig;

use chrono::NaiveDateTime;
use uuid::Uuid;

/// A decrypted KeePass database
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serialization", derive(serde::Serialize))]
pub struct Database {
    /// Configuration settings of the database such as encryption and compression algorithms
    pub config: DatabaseConfig,

    /// Metadata of the KeePass database
    pub meta: Meta,

    /// Root node of the KeePass database
    pub(crate) root: GroupId,

    /// All attachments in the database, stored in a flat HashMap
    pub(crate) attachments: HashMap<AttachmentId, Attachment>,

    /// All custom icons in the database, stored in a flat HashMap
    pub(crate) custom_icons: HashMap<CustomIconId, CustomIcon>,

    /// All entries in the database, stored in a flat HashMap
    pub(crate) entries: HashMap<EntryId, Entry>,

    /// All groups in the database, stored in a flat HashMap
    pub(crate) groups: HashMap<GroupId, Group>,

    /// References to previously-deleted objects and their deletion times.
    pub deleted_objects: HashMap<Uuid, Option<NaiveDateTime>>,
}

impl Database {
    /// Create a new database with a single root group and no entries, groups, or attachments.
    ///
    /// The root group will be assigned a new random UUID.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::new_with_root_id(GroupId::new())
    }

    /// Create a new database with the given configuration and a single root group.
    ///
    /// The root group will be assigned a new random UUID.
    pub fn with_config(config: DatabaseConfig) -> Self {
        Self::with_data(config, GroupId::new())
    }

    pub(crate) fn new_with_root_id(root_id: GroupId) -> Self {
        let root = Group::with_id(root_id, None);

        let mut groups = HashMap::new();
        groups.insert(root_id, root);

        Database {
            config: DatabaseConfig::default(),
            meta: Meta::default(),
            root: root_id,
            attachments: HashMap::new(),
            custom_icons: HashMap::new(),
            entries: HashMap::new(),
            groups,
            deleted_objects: HashMap::new(),
        }
    }

    pub(crate) fn with_data(config: DatabaseConfig, root_id: GroupId) -> Self {
        let root = Group::with_id(root_id, None);

        let mut groups = HashMap::new();
        groups.insert(root_id, root);

        Database {
            config,
            meta: Meta::default(),
            root: root_id,
            attachments: HashMap::new(),
            custom_icons: HashMap::new(),
            entries: HashMap::new(),
            groups,
            deleted_objects: HashMap::new(),
        }
    }

    /// Get an immutable reference to the root group of the database.
    pub fn root(&self) -> GroupRef<'_> {
        GroupRef::new(self, self.root)
    }

    /// Get a mutable reference to the root group of the database.
    pub fn root_mut(&mut self) -> GroupMut<'_> {
        GroupMut::new(self, self.root)
    }

    /// Get an immutable reference to the recycle bin group, if it exists
    pub fn recycle_bin(&self) -> Option<GroupRef<'_>> {
        let recyclebin_id = self.meta.recyclebin_uuid.map(GroupId::from_uuid)?;
        self.group(recyclebin_id)
    }

    /// Get a mutable reference to the recycle bin group, if it exists
    pub fn recycle_bin_mut(&mut self) -> Option<GroupMut<'_>> {
        let recyclebin_id = self.meta.recyclebin_uuid.map(GroupId::from_uuid)?;
        self.group_mut(recyclebin_id)
    }

    /// Get the number of attachments in the database
    pub fn num_attachments(&self) -> usize {
        self.attachments.len()
    }

    /// Get the number of custom icons in the database
    pub fn num_custom_icons(&self) -> usize {
        self.custom_icons.len()
    }

    /// Get the number of entries in the database
    pub fn num_entries(&self) -> usize {
        self.entries.len()
    }

    /// Get the number of groups in the database, including the root group and the recycle bin (if it exists)
    pub fn num_groups(&self) -> usize {
        self.groups.len()
    }

    /// Iterate over all attachments with immutable access.
    pub fn iter_all_attachments(&self) -> impl Iterator<Item = AttachmentRef<'_>> + '_ {
        self.attachments
            .keys()
            .map(move |id| AttachmentRef::new(self, *id))
    }

    /// Iterate over all attachments with mutable access. The provided closure is
    /// called for each `AttachmentMut` and borrows are limited to the closure body.
    pub fn foreach_attachment_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(AttachmentMut<'_>),
    {
        let ids: Vec<AttachmentId> = self.attachments.keys().copied().collect();
        for id in ids {
            f(AttachmentMut::new(self, id));
        }
    }

    /// Iterate over all entries with immutable access.
    pub fn iter_all_entries(&self) -> impl Iterator<Item = EntryRef<'_>> + '_ {
        self.entries.keys().map(move |id| EntryRef::new(self, *id))
    }

    /// Iterate over all entries with mutable access. The provided closure is
    /// called for each `EntryMut` and borrows are limited to the closure body.
    pub fn foreach_entry_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(EntryMut<'_>),
    {
        let ids: Vec<EntryId> = self.entries.keys().copied().collect();
        for id in ids {
            f(EntryMut::new(self, id));
        }
    }

    /// Iterate over all custom icons with immutable access.
    pub fn iter_all_custom_icons(&self) -> impl Iterator<Item = CustomIconRef<'_>> + '_ {
        self.custom_icons
            .keys()
            .map(move |id| CustomIconRef::new(self, *id))
    }

    /// Iterate over all custom icons with mutable access. The provided closure is
    /// called for each `CustomIconMut` and borrows are limited to the closure body.
    pub fn foreach_custom_icon_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(CustomIconMut<'_>),
    {
        let ids: Vec<CustomIconId> = self.custom_icons.keys().copied().collect();
        for id in ids {
            f(CustomIconMut::new(self, id));
        }
    }

    /// Iterate over all groups with immutable access. This includes the root group and the recycle
    /// bin (if it exists).
    pub fn iter_all_groups(&self) -> impl Iterator<Item = GroupRef<'_>> + '_ {
        self.groups.keys().map(move |id| GroupRef::new(self, *id))
    }

    /// Iterate over all groups with mutable access. The provided closure is
    /// called for each `GroupMut` and borrows are limited to the closure body.
    pub fn foreach_group_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(GroupMut<'_>),
    {
        let ids: Vec<GroupId> = self.groups.keys().copied().collect();
        for id in ids {
            f(GroupMut::new(self, id));
        }
    }

    /// Get an immutable reference to the attachment with the given ID, if it exists
    pub fn attachment(&self, id: AttachmentId) -> Option<AttachmentRef<'_>> {
        self.attachments
            .contains_key(&id)
            .then(move || AttachmentRef::new(self, id))
    }

    /// Get a mutable reference to the attachment with the given ID, if it exists
    pub fn attachment_mut(&mut self, id: AttachmentId) -> Option<AttachmentMut<'_>> {
        self.attachments
            .contains_key(&id)
            .then(move || AttachmentMut::new(self, id))
    }

    /// Map each still-referenced attachment id to the `(EntryId, history_index)` pairs that
    /// reference it across every live and history entry. This walk over the entries' `name ->
    /// AttachmentId` maps is the authoritative source of truth for which binaries are in use.
    pub(crate) fn attachment_references(
        &self,
    ) -> HashMap<AttachmentId, std::collections::HashSet<(EntryId, Option<usize>)>> {
        use std::collections::HashSet;

        let mut referenced: HashMap<AttachmentId, HashSet<(EntryId, Option<usize>)>> = HashMap::new();
        for (&entry_id, entry) in &self.entries {
            for &attachment_id in entry.attachments.values() {
                referenced
                    .entry(attachment_id)
                    .or_default()
                    .insert((entry_id, None));
            }
            if let Some(history) = entry.history.as_ref() {
                for (i, hist_entry) in history.entries.iter().enumerate() {
                    for &attachment_id in hist_entry.attachments.values() {
                        referenced
                            .entry(attachment_id)
                            .or_default()
                            .insert((entry_id, Some(i)));
                    }
                }
            }
        }
        referenced
    }

    /// Collect the `(EntryId, history_index)` versions that currently reference the given attachment
    /// id, derived from the authoritative forward `name -> AttachmentId` maps (not from the cached
    /// back-reference set, which cannot be kept accurate across history-index shifts). With
    /// `include_historical` false, only the live versions are returned.
    pub(crate) fn attachment_referrers(
        &self,
        id: AttachmentId,
        include_historical: bool,
    ) -> Vec<(EntryId, Option<usize>)> {
        let mut referrers = Vec::new();
        for (&entry_id, entry) in &self.entries {
            if entry.attachments.values().any(|&a| a == id) {
                referrers.push((entry_id, None));
            }
            if include_historical {
                if let Some(history) = entry.history.as_ref() {
                    for (i, hist_entry) in history.entries.iter().enumerate() {
                        if hist_entry.attachments.values().any(|&a| a == id) {
                            referrers.push((entry_id, Some(i)));
                        }
                    }
                }
            }
        }
        referrers
    }

    /// Build the stable `old -> new` contiguous attachment-id remapping over ids that are still
    /// referenced and present in the pool. Unreferenced (deleted) binaries are excluded. Ordered by
    /// the underlying id for deterministic output.
    pub(crate) fn attachment_compaction_remap(&self) -> HashMap<AttachmentId, AttachmentId> {
        let referenced = self.attachment_references();
        let mut old_ids: Vec<AttachmentId> = referenced
            .keys()
            .copied()
            .filter(|id| self.attachments.contains_key(id))
            .collect();
        old_ids.sort_by_key(|id| id.id());

        old_ids
            .iter()
            .enumerate()
            .map(|(new_index, &old_id)| (old_id, AttachmentId::new(new_index)))
            .collect()
    }

    /// Whether the attachment pool would change under compaction, i.e. it contains binaries that no
    /// live or history version references (deleted bytes that would otherwise be written back out) or
    /// its ids are not a contiguous `0..n` range. Used by the save path to compact only when needed.
    pub(crate) fn attachments_need_compaction(&self) -> bool {
        let remap = self.attachment_compaction_remap();
        remap.len() != self.attachments.len() || remap.iter().any(|(old, new)| old != new)
    }

    /// Compact the attachment binary pool in place (KeePassXC-style).
    ///
    /// Attachment deletion ([`EntryMut::remove_attachment_by_name`][crate::db::EntryMut::remove_attachment_by_name])
    /// only drops a single entry version's reference and never garbage-collects the binary, so that
    /// attachments still referenced by a history version survive and freed ids are never reused.
    /// This method performs the deferred cleanup:
    ///
    /// 1. walks every live and history entry to determine which attachments are still referenced,
    /// 2. removes binaries referenced by no live or history version,
    /// 3. re-indexes the surviving binaries to a contiguous `0..n` id range, and
    /// 4. rewrites every live/history `name -> AttachmentId` reference (and each attachment's
    ///    back-reference set) to the new ids.
    ///
    /// Re-indexing to contiguous ids is required for a correct save/reopen round-trip: the KDBX
    /// binary pool is reloaded positionally, so gaps in the id range would otherwise misalign
    /// references. [`Database::save`] already serializes an equivalent compacted view, so deleted
    /// binaries are never written back even if this is not called; use this to also drop them from
    /// the in-memory pool (e.g. to reflect the change in [`Database::num_attachments`]).
    pub fn compact_attachments(&mut self) {
        // Rewrite an entry version's `name -> AttachmentId` map through `remap`, dropping any
        // reference whose target is not retained.
        fn remap_refs(
            attachments: &mut HashMap<String, AttachmentId>,
            remap: &HashMap<AttachmentId, AttachmentId>,
        ) {
            attachments.retain(|_, id| remap.contains_key(id));
            for id in attachments.values_mut() {
                if let Some(&new_id) = remap.get(id) {
                    *id = new_id;
                }
            }
        }

        let referenced = self.attachment_references();
        let remap = self.attachment_compaction_remap();

        // Rewrite every live and history attachment reference through the mapping.
        for entry in self.entries.values_mut() {
            remap_refs(&mut entry.attachments, &remap);
            if let Some(history) = entry.history.as_mut() {
                for hist_entry in &mut history.entries {
                    remap_refs(&mut hist_entry.attachments, &remap);
                }
            }
        }

        // Rebuild the pool with the new contiguous ids, dropping orphans. The cached back-reference
        // set stores only live (`None`) references; historical referrers are derived on demand (see
        // `attachment_referrers`) so that no stale positional history index is ever persisted.
        let mut old_pool = std::mem::take(&mut self.attachments);
        for (&old_id, &new_id) in &remap {
            if let Some(mut attachment) = old_pool.remove(&old_id) {
                attachment.id = new_id;
                attachment.entries = referenced
                    .get(&old_id)
                    .map(|refs| refs.iter().copied().filter(|(_, hist)| hist.is_none()).collect())
                    .unwrap_or_default();
                self.attachments.insert(new_id, attachment);
            }
        }
    }

    /// Get an immutable reference to the custom icon with the given ID, if it exists
    pub fn custom_icon(&self, id: CustomIconId) -> Option<CustomIconRef<'_>> {
        self.custom_icons
            .contains_key(&id)
            .then(move || CustomIconRef::new(self, id))
    }

    /// Get a mutable reference to the custom icon with the given ID, if it exists
    pub fn custom_icon_mut(&mut self, id: CustomIconId) -> Option<CustomIconMut<'_>> {
        self.custom_icons
            .contains_key(&id)
            .then(move || CustomIconMut::new(self, id))
    }

    /// Get an immutable reference to the entry with the given ID, if it exists
    pub fn entry(&self, id: EntryId) -> Option<EntryRef<'_>> {
        self.entries
            .contains_key(&id)
            .then(move || EntryRef::new(self, id))
    }

    /// Get a mutable reference to the entry with the given ID, if it exists
    pub fn entry_mut(&mut self, id: EntryId) -> Option<EntryMut<'_>> {
        self.entries
            .contains_key(&id)
            .then(move || EntryMut::new(self, id))
    }

    /// Get an immutable reference to the group with the given ID, if it exists
    pub fn group(&self, id: GroupId) -> Option<GroupRef<'_>> {
        self.groups
            .contains_key(&id)
            .then(move || GroupRef::new(self, id))
    }

    /// Get a mutable reference to the group with the given ID, if it exists
    pub fn group_mut(&mut self, id: GroupId) -> Option<GroupMut<'_>> {
        self.groups
            .contains_key(&id)
            .then(move || GroupMut::new(self, id))
    }
}
