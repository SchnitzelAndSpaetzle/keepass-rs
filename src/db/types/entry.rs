use std::{
    collections::{HashMap, HashSet},
    ops::{Deref, DerefMut},
};

use thiserror::Error;
use uuid::Uuid;

use crate::{
    db::{
        attachment::{AttachmentMut, AttachmentRef},
        fields, Attachment, AttachmentId, AutoType, Color, CustomDataItem, CustomIcon, CustomIconId,
        CustomIconMut, CustomIconNotFoundError, CustomIconRef, GroupId, GroupMut, GroupRef, History, Icon,
        Times, Value,
    },
    Database,
};

/// Unique identifier for an [Entry]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serialization", derive(serde::Serialize))]
pub struct EntryId(Uuid);

impl EntryId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Build an `EntryId` from an existing [Uuid].
    ///
    /// Useful when an entry's identifier needs to be pinned (e.g. test fixtures or migrations).
    /// Pair with [Group::add_entry_with_id][crate::db::GroupMut::add_entry_with_id] to insert
    /// an entry under a chosen identifier.
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Get the Uuid contained inside
    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for EntryId {
    fn from(uuid: Uuid) -> Self {
        Self::from_uuid(uuid)
    }
}

/// A database entry containing several key-value fields.
#[derive(Debug, Eq, PartialEq, Clone)]
#[cfg_attr(feature = "serialization", derive(serde::Serialize))]
pub struct Entry {
    pub(crate) id: EntryId,
    pub(crate) parent: GroupId,

    /// the key-value fields of this entry, such as username and password.
    ///
    /// Common field names are available in [crate::db::fields].
    pub fields: HashMap<String, Value<String>>,

    /// AutoType settings for this entry
    pub autotype: Option<AutoType>,

    /// tags associated with this entry
    pub tags: Vec<String>,

    /// timestamps for this entry
    pub times: Times,

    /// custom data items associated with this entry
    pub custom_data: HashMap<String, CustomDataItem>,

    pub(crate) icon: Option<Icon>,

    /// foreground color for this entry
    pub foreground_color: Option<Color>,

    /// background color for this entry
    pub background_color: Option<Color>,

    /// URL override for this entry
    pub override_url: Option<String>,

    /// whether to enable password quality check for this entry
    pub quality_check: bool,

    /// attachments associated with this entry, mapped by attachment name to attachment ID
    pub(crate) attachments: HashMap<String, AttachmentId>,

    /// Identifier of the group that the Entry was previously contained in
    pub(crate) previous_parent_group: Option<GroupId>,

    /// history of this entry
    pub history: Option<History>,
}

impl Entry {
    pub(crate) fn new(parent: GroupId) -> Self {
        Entry::with_id(EntryId::new(), parent)
    }

    pub(crate) fn with_id(id: EntryId, parent: GroupId) -> Self {
        Entry {
            id,
            parent,
            fields: HashMap::new(),
            autotype: None,
            tags: Vec::new(),
            times: Times::new(),
            custom_data: HashMap::new(),
            icon: None,
            foreground_color: None,
            background_color: None,
            override_url: None,
            quality_check: true,
            attachments: HashMap::new(),
            history: Some(History::default()),
            previous_parent_group: None,
        }
    }

    /// Get the unique identifier for the [Entry]
    pub fn id(&self) -> EntryId {
        self.id
    }

    /// Get the icon of this entry, if it exists
    pub fn icon(&self) -> Option<&Icon> {
        self.icon.as_ref()
    }

    /// Get a field by name, taking care of unprotecting Protected values automatically
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(|v| v.as_str())
    }

    /// Set a field's value by name
    pub fn set(&mut self, key: impl Into<String>, value: Value<String>) {
        self.fields.insert(key.into(), value);
    }

    /// Set a field's unprotected value by name
    pub fn set_unprotected(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.set(key, Value::unprotected(value));
    }

    /// Set a field's protected value by name
    pub fn set_protected(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.set(key, Value::protected(value));
    }

    /// Convenience method for getting the raw value of the 'otp' field
    pub fn get_raw_otp_value(&self) -> Option<&str> {
        self.get(fields::OTP)
    }

    /// Convenience method for getting the value of the 'Title' field
    pub fn get_title(&self) -> Option<&str> {
        self.get(fields::TITLE)
    }

    /// Convenience method for getting the value of the 'UserName' field
    pub fn get_username(&self) -> Option<&str> {
        self.get(fields::USERNAME)
    }

    /// Convenience method for getting the value of the 'Password' field
    pub fn get_password(&self) -> Option<&str> {
        self.get(fields::PASSWORD)
    }

    /// Convenience method for getting the value of the 'URL' field
    pub fn get_url(&self) -> Option<&str> {
        self.get(fields::URL)
    }

    /// Rewrites this entry's attachment references — including those of its
    /// history versions — to point into `dest`'s pool, importing the
    /// referenced bytes from `source`'s pool. References whose bytes are
    /// missing from `source` are dropped. Blobs already present in `dest`
    /// with equal data are reused rather than duplicated.
    ///
    /// Use this when transplanting an [Entry] value from one [Database] into
    /// another (e.g. during a merge): [AttachmentId]s are only meaningful
    /// within the pool of the database that minted them.
    pub fn remap_attachments(&mut self, source: &Database, dest: &mut Database) {
        remap_attachment_map(&mut self.attachments, source, dest);
        if let Some(history) = self.history.as_mut() {
            for version in history.entries.iter_mut() {
                remap_attachment_map(&mut version.attachments, source, dest);
            }
        }
    }

    /// Whether two entries carry the same content, ignoring timestamps,
    /// history, and attachment pool ids. Attachments compare by name and
    /// bytes, resolved against each entry's own database — so two copies of
    /// an entry whose pools assigned different [AttachmentId]s to the same
    /// data still count as equivalent.
    pub fn content_equivalent(&self, self_db: &Database, other: &Entry, other_db: &Database) -> bool {
        let mut a = self.clone();
        a.times = Times::default();
        a.history = None;
        a.attachments = HashMap::new();

        let mut b = other.clone();
        b.times = Times::default();
        b.history = None;
        b.attachments = HashMap::new();

        if a != b {
            return false;
        }

        if self.attachments.len() != other.attachments.len() {
            return false;
        }
        self.attachments.iter().all(|(name, self_id)| {
            let Some(other_id) = other.attachments.get(name) else {
                return false;
            };
            let self_data = self_db
                .attachments
                .get(self_id)
                .map(|attachment| &attachment.data);
            let other_data = other_db
                .attachments
                .get(other_id)
                .map(|attachment| &attachment.data);
            match (self_data, other_data) {
                (Some(a), Some(b)) => a == b,
                // Only two equally-dangling references count as equal.
                (None, None) => true,
                _ => false,
            }
        })
    }
}

/// Rewrites a `name -> AttachmentId` map from `source`'s pool into `dest`'s,
/// importing bytes as needed. Names are visited in sorted order so id
/// assignment in `dest` is deterministic.
fn remap_attachment_map(map: &mut HashMap<String, AttachmentId>, source: &Database, dest: &mut Database) {
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort_unstable();
    for name in names {
        #[allow(clippy::unwrap_used)] // name was collected from the map's own keys
        let source_id = *map.get(&name).unwrap();
        match source.attachments.get(&source_id) {
            Some(attachment) => {
                let dest_id = dest.intern_attachment(&attachment.data);
                map.insert(name, dest_id);
            }
            None => {
                map.remove(&name);
            }
        }
    }
}

impl std::fmt::Display for EntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An immutable reference to an [Entry]. Implements [Deref] to [&Entry][Entry].
pub struct EntryRef<'a> {
    database: &'a Database,
    id: EntryId,
    history_index: Option<usize>,
}

impl EntryRef<'_> {
    pub(crate) fn new(database: &Database, id: EntryId) -> EntryRef<'_> {
        EntryRef {
            database,
            id,
            history_index: None,
        }
    }

    pub(crate) fn new_historical(
        database: &Database,
        id: EntryId,
        history_index: Option<usize>,
    ) -> EntryRef<'_> {
        EntryRef {
            database,
            id,
            history_index,
        }
    }

    /// Get a reference to the parent group of this entry.
    pub fn parent(&self) -> GroupRef<'_> {
        #[allow(clippy::unwrap_used, clippy::missing_panics_doc)] // parent always exists
        self.database.group(self.parent).unwrap()
    }

    /// Get a reference to the previous parent group, if any
    pub fn previous_parent(&self) -> Option<GroupRef<'_>> {
        self.previous_parent_group
            .and_then(|id| self.database().group(id))
    }

    /// Gets an [EntryRef] to a historical version of the [Entry], if it exists
    pub fn historical(&self, index: usize) -> Option<EntryRef<'_>> {
        if let Some(h) = &self.history {
            if index < h.entries.len() {
                Some(EntryRef {
                    database: self.database,
                    id: self.id,
                    history_index: Some(index),
                })
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Get a reference to the underlying database
    pub fn database(&self) -> &Database {
        self.database
    }

    /// Get a reference to an attachment by id, if it exists.
    pub fn attachment(&self, id: AttachmentId) -> Option<AttachmentRef<'_>> {
        self.attachments
            .values()
            .find(|&attachment_id| *attachment_id == id)
            .cloned()
            .map(move |attachment_id| AttachmentRef::new(self.database, attachment_id))
    }

    /// Get a reference to an attachment by name, if it exists.
    pub fn attachment_by_name(&self, name: &str) -> Option<AttachmentRef<'_>> {
        self.attachments
            .get(name)
            .cloned()
            .map(move |attachment_id| AttachmentRef::new(self.database, attachment_id))
    }

    /// Get an iterator over the attachments of this entry.
    pub fn attachments(&self) -> impl Iterator<Item = AttachmentRef<'_>> {
        self.attachments
            .values()
            .cloned()
            .map(move |attachment_id| AttachmentRef::new(self.database, attachment_id))
    }

    /// Get an iterator over the (name, attachment) pairs of this entry.
    ///
    /// Useful when callers need both the attachment's filename (the key under
    /// which it is stored on the entry) and its data, since [`AttachmentRef`]
    /// itself does not expose the per-entry name.
    pub fn attachments_named(&self) -> impl Iterator<Item = (&str, AttachmentRef<'_>)> {
        self.attachments.iter().map(move |(name, &attachment_id)| {
            (name.as_str(), AttachmentRef::new(self.database, attachment_id))
        })
    }

    /// Get the custom icon of this entry, if it exists and is a custom icon.
    pub fn custom_icon(&self) -> Option<CustomIconRef<'_>> {
        if let Some(Icon::Custom(custom_icon_id)) = self.icon {
            Some(CustomIconRef::new(self.database, custom_icon_id))
        } else {
            None
        }
    }
}

impl Deref for EntryRef<'_> {
    type Target = Entry;

    #[allow(clippy::expect_used, clippy::missing_panics_doc)] // entry existence is guaranteed
    fn deref(&self) -> &Self::Target {
        // UNWRAP safety: EntryRef can only be constructed with a valid EntryId
        let entry = self.database.entries.get(&self.id).expect("Entry not found");

        if let Some(n) = self.history_index {
            // UNWRAP safety: history existance checked on EntryRef creation
            #[allow(clippy::unwrap_used, clippy::indexing_slicing)]
            &entry.history.as_ref().unwrap().entries[n]
        } else {
            entry
        }
    }
}

/// A mutable reference to an [Entry]. Implements [DerefMut] to [&mut Entry][Entry].
pub struct EntryMut<'a> {
    database: &'a mut Database,
    id: EntryId,
    history_index: Option<usize>,
}

impl EntryMut<'_> {
    pub(crate) fn new(database: &mut Database, id: EntryId) -> EntryMut<'_> {
        EntryMut {
            database,
            id,
            history_index: None,
        }
    }

    pub(crate) fn new_historical(
        database: &mut Database,
        id: EntryId,
        history_index: Option<usize>,
    ) -> EntryMut<'_> {
        EntryMut {
            database,
            id,
            history_index,
        }
    }

    /// Get an immutable reference to the entry.
    pub fn as_ref(&self) -> EntryRef<'_> {
        EntryRef {
            database: self.database,
            id: self.id,
            history_index: self.history_index,
        }
    }

    /// Convenience method to edit the entry in a closure.
    pub fn edit(&mut self, f: impl FnOnce(&mut EntryMut<'_>)) -> &mut Self {
        f(self);
        self
    }

    /// Convenience method to edit the entry in a closure, tracking changes.
    pub fn edit_tracking(&mut self, f: impl FnOnce(&mut EntryTrack<'_>)) -> &mut Self {
        {
            let mut tracked = self.track_changes();
            f(&mut tracked);
        }
        self
    }

    /// Convert this mutable reference into a history-tracking variant that will persist the
    /// current state of the entry into its history when dropped.
    ///
    /// NOTE: will always operate on the main Entry, not a historical version of it.
    pub fn track_changes(&mut self) -> EntryTrack<'_> {
        let mut historical: Entry = self.deref().deref().clone();

        // Remove history from the historical entry to avoid exponential growth
        historical.history = None;

        EntryTrack {
            database: self.database,
            id: self.id,
            historical,
        }
    }

    /// Get a mutable reference to the parent group of this entry.
    pub fn parent_mut(&mut self) -> GroupMut<'_> {
        #[allow(clippy::unwrap_used, clippy::missing_panics_doc)] // parent always exists
        self.database.group_mut(self.parent).unwrap()
    }

    /// Get a mutable reference to the previous parent group, if any
    pub fn previous_parent_mut(&mut self) -> Option<GroupMut<'_>> {
        self.previous_parent_group
            .and_then(move |id| self.database_mut().group_mut(id))
    }

    /// Get a mutable reference to an attachment by id, if it exists.
    pub fn attachment_mut(&mut self, id: AttachmentId) -> Option<AttachmentMut<'_>> {
        self.attachments
            .values()
            .find(|&attachment_id| *attachment_id == id)
            .cloned()
            .map(move |attachment_id| AttachmentMut::new(self.database, attachment_id))
    }

    /// Get a mutable reference to an attachment by name, if it exists.
    pub fn attachment_by_name_mut(&mut self, name: &str) -> Option<AttachmentMut<'_>> {
        self.attachments
            .get(name)
            .cloned()
            .map(move |attachment_id| AttachmentMut::new(self.database, attachment_id))
    }

    /// Apply a closure to each attachment of this entry, with mutable access.
    pub fn foreach_attachment_mut<F>(&mut self, mut f: F)
    where
        F: FnMut(AttachmentMut<'_>),
    {
        let attachments: Vec<AttachmentId> = self.attachments.values().copied().collect();
        for attachment_id in attachments {
            f(AttachmentMut::new(self.database, attachment_id));
        }
    }

    /// Add an attachment to this entry with the given name and data.
    pub fn add_attachment(&mut self, name: impl Into<String>, data: Value<Vec<u8>>) -> AttachmentMut<'_> {
        let id = AttachmentId::next_free(self.database);

        self.database.attachments.insert(id, Attachment { id, data });

        if let Some(old_id) = self.attachments.insert(name.into(), id) {
            // if there was an old attachment with this name, remove it
            self.remove_attachment_by_id(old_id);
        }

        AttachmentMut::new(self.database, id)
    }

    /// Remove an attachment by name from this entry version.
    ///
    /// This drops only this exact entry version's `name -> AttachmentId` reference; references from
    /// other versions (including history) are left intact. The binary is not garbage-collected from
    /// the in-memory pool here: an attachment still referenced by a history version must survive, and
    /// freed ids must not be reused while any version still points at them.
    /// [`Database::save`][crate::db::Database::save] already writes a compacted view, so a binary
    /// referenced by no live or history version is not persisted; call
    /// [`Database::compact_attachments`][crate::db::Database::compact_attachments] to also drop it
    /// from the in-memory pool.
    pub fn remove_attachment_by_name(&mut self, name: &str) {
        self.attachments.remove(name);
    }

    /// Remove an attachment by id from this entry version.
    ///
    /// Drops every `name -> AttachmentId` reference for `attachment_id` held by this exact entry
    /// version; see [`Self::remove_attachment_by_name`] for the retention and garbage-collection
    /// semantics.
    pub fn remove_attachment_by_id(&mut self, attachment_id: AttachmentId) {
        self.attachments.retain(|_, &mut id| id != attachment_id);
    }

    /// Remove the icon from this entry, if it exists.
    ///
    /// This clears only this exact entry version's `icon` field; other versions (including history)
    /// keep their own icon. Custom-icon referrers are derived from the `icon` field on demand (see
    /// [`Database::custom_icon_referrers`][crate::db::Database]), so no back-reference bookkeeping is
    /// needed here.
    pub fn set_icon_none(&mut self) {
        self.icon = None;
    }

    /// Set a built-in icon for this entry by its ID, removing any existing icon.
    pub fn set_icon_builtin(&mut self, icon_id: usize) {
        self.set_icon_none();
        self.icon = Some(Icon::BuiltIn(icon_id));
    }

    /// Set a custom icon for this entry by its ID, removing any existing icon.
    pub fn set_icon_custom(&mut self, custom_icon_id: CustomIconId) -> Result<(), CustomIconNotFoundError> {
        self.set_icon_none();

        if !self.database.custom_icons.contains_key(&custom_icon_id) {
            return Err(CustomIconNotFoundError(custom_icon_id));
        }

        self.icon = Some(Icon::Custom(custom_icon_id));

        Ok(())
    }

    /// Set a custom icon for this entry by providing the raw data, removing any existing icon.
    /// Returns a mutable reference to the newly created custom icon.
    pub fn set_icon_custom_new(&mut self, data: Vec<u8>) -> CustomIconMut<'_> {
        self.set_icon_none();

        let custom_icon_id = CustomIconId::new();

        self.database.custom_icons.insert(
            custom_icon_id,
            CustomIcon {
                id: custom_icon_id,
                groups: HashSet::new(),
                name: None,
                last_modification_time: Some(Times::now()),
                data,
            },
        );

        self.icon = Some(Icon::Custom(custom_icon_id));

        CustomIconMut::new(self.database, custom_icon_id)
    }

    /// Get a mutable reference to the custom icon of this entry, if it exists and is a custom
    /// icon.
    pub fn custom_icon_mut(&mut self) -> Option<CustomIconMut<'_>> {
        if let Some(Icon::Custom(custom_icon_id)) = self.icon {
            Some(CustomIconMut::new(self.database, custom_icon_id))
        } else {
            None
        }
    }

    /// Move this entry to another group.
    ///
    /// NOTE: will always operate on the main Entry, not a historical version of it.
    pub fn move_to(&mut self, group_id: GroupId) -> Result<(), DestinationGroupNotFoundError> {
        if !self.database.groups.contains_key(&group_id) {
            return Err(DestinationGroupNotFoundError(group_id));
        }

        let my_id = self.id;
        let previous_parent = self.parent;

        let mut parent = self.parent_mut();
        parent.entries.remove(&my_id);

        #[allow(clippy::unwrap_used, clippy::missing_panics_doc)] // group existence is checked
        let mut new_parent = self.database.group_mut(group_id).unwrap();
        new_parent.entries.insert(my_id);
        self.parent = group_id;
        self.previous_parent_group = Some(previous_parent);

        Ok(())
    }

    /// Get a mutable reference to the underlying database
    pub fn database_mut(&mut self) -> &mut Database {
        self.database
    }

    /// Remove this entry from the database, including all its attachments.
    #[allow(clippy::expect_used, clippy::missing_panics_doc)] // the entry and parent should always be found
    pub fn remove(self) {
        let id = self.id;

        // Custom-icon back-references need no cleanup here: referrers are derived on demand from each
        // entry's `icon` field (see `Database::custom_icon_referrers`), and this entry is removed from
        // `database.entries` below, so it can no longer appear as a referrer.

        // Collect the attachments this entry references in its live version or any history version,
        // then garbage-collect those that no surviving entry references. Referencing versions are
        // derived from the forward `name -> AttachmentId` maps, so removing an entry cannot leave a
        // dangling reference.
        let mut attachment_ids: HashSet<AttachmentId> = self.attachments.values().copied().collect();
        if let Some(history) = self.history.as_ref() {
            for hist_entry in &history.entries {
                attachment_ids.extend(hist_entry.attachments.values().copied());
            }
        }

        for attachment_id in attachment_ids {
            let referenced_elsewhere = self.database.entries.iter().any(|(&other_id, other)| {
                other_id != id
                    && (other.attachments.values().any(|&a| a == attachment_id)
                        || other.history.as_ref().is_some_and(|h| {
                            h.entries
                                .iter()
                                .any(|he| he.attachments.values().any(|&a| a == attachment_id))
                        }))
            });

            if !referenced_elsewhere {
                self.database.attachments.remove(&attachment_id);
            }
        }

        let entry = self.database.entries.remove(&self.id).expect("Entry not found");

        // Remove from parent group
        let mut parent = self
            .database
            .group_mut(entry.parent)
            .expect("Parent group not found");
        parent.entries.remove(&self.id);

        // Clear any group's last_top_visible_entry that pointed to this entry.
        // This field is a UI hint and should not hold a dangling EntryId.
        let group_ids: Vec<GroupId> = self.database.groups.keys().copied().collect();
        for group_id in group_ids {
            if let Some(group) = self.database.groups.get_mut(&group_id) {
                if group.last_top_visible_entry == Some(id) {
                    group.last_top_visible_entry = None;
                }
            }
        }
    }
}

/// Error type for when a destination [GroupId] is provided that does not exist in the database
#[derive(Error, Debug)]
#[error("Destination group {0} not found")]
pub struct DestinationGroupNotFoundError(pub(crate) GroupId);

impl Deref for EntryMut<'_> {
    type Target = Entry;

    #[allow(clippy::expect_used, clippy::missing_panics_doc)] // entry existence is guaranteed
    fn deref(&self) -> &Self::Target {
        // UNWRAP safety: EntryMut can only be constructed with a valid EntryId
        let entry = self.database.entries.get(&self.id).expect("Entry not found");

        if let Some(n) = self.history_index {
            // UNWRAP safety: history existence checked on EntryMut creation
            #[allow(clippy::unwrap_used, clippy::indexing_slicing)]
            &entry.history.as_ref().unwrap().entries[n]
        } else {
            entry
        }
    }
}

impl DerefMut for EntryMut<'_> {
    #[allow(clippy::expect_used, clippy::missing_panics_doc)] // entry existence is guaranteed
    fn deref_mut(&mut self) -> &mut Self::Target {
        // UNWRAP safety: EntryMut can only be constructed with a valid EntryId
        let entry = self.database.entries.get_mut(&self.id).expect("Entry not found");

        if let Some(n) = self.history_index {
            // UNWRAP safety: history existence checked on EntryMut creation
            #[allow(clippy::unwrap_used, clippy::indexing_slicing)]
            &mut entry.history.as_mut().unwrap().entries[n]
        } else {
            entry
        }
    }
}

/// A variant of [EntryMut] that will persist the history of the entry when dropped.
#[clippy::has_significant_drop]
pub struct EntryTrack<'a> {
    database: &'a mut Database,
    id: EntryId,

    historical: Entry,
}

impl EntryTrack<'_> {
    /// Turn this tracked entry into a normal mutable reference to the entry
    pub fn as_mut(&mut self) -> EntryMut<'_> {
        EntryMut {
            database: self.database,
            id: self.id,
            history_index: None,
        }
    }

    /// Move this entry to another group, tracking the change in history.
    pub fn move_to(&mut self, group_id: GroupId) -> Result<(), DestinationGroupNotFoundError> {
        self.as_mut().move_to(group_id)?;
        self.times.location_changed = Some(Times::now());
        Ok(())
    }

    /// Remove this entry from the database, tracking the change in history.
    pub fn remove(mut self) {
        let this = self.as_mut();
        this.database
            .deleted_objects
            .insert(this.id.uuid(), Some(Times::now()));

        // use EntryMut::remove to handle actual removal
        this.remove();
    }

    /// Convenience method to edit the entry in a closure, tracking changes.
    pub fn edit(&mut self, f: impl FnOnce(&mut EntryTrack<'_>)) -> &mut Self {
        f(self);
        self.times.last_modification = Some(Times::now());
        self
    }

    /// Set a field value, tracking changes. See [crate::db::fields] for common field names.
    pub fn set(&mut self, key: impl Into<String>, value: Value<String>) {
        let mut this = self.as_mut();
        this.set(key, value);
        this.times.last_modification = Some(Times::now());
    }

    /// Set a protected field value, tracking changes. See [crate::db::fields] for common field names.
    pub fn set_protected(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let mut this = self.as_mut();
        this.set_protected(key, value);
        this.times.last_modification = Some(Times::now());
    }

    /// Set an unprotected field value, tracking changes. See [crate::db::fields] for common field names.
    pub fn set_unprotected(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let mut this = self.as_mut();
        this.set_unprotected(key, value);
        this.times.last_modification = Some(Times::now());
    }

    /// Add an attachment, tracking changes.
    pub fn add_attachment(&mut self, name: impl Into<String>, data: Value<Vec<u8>>) -> AttachmentMut<'_> {
        self.times.last_modification = Some(Times::now());
        let mut this = self.as_mut();
        let id = this.add_attachment(name, data).id;

        AttachmentMut::new(self.database, id)
    }

    /// Remove the entry's icon, tracking changes.
    pub fn set_icon_none(&mut self) {
        let mut this = self.as_mut();
        this.set_icon_none();
        this.times.last_modification = Some(Times::now());
    }

    /// Set a built-in icon for this entry by its ID, tracking changes.
    pub fn set_icon_builtin(&mut self, icon_id: usize) {
        let mut this = self.as_mut();
        this.set_icon_builtin(icon_id);
        this.times.last_modification = Some(Times::now());
    }

    /// Set a custom icon for this entry by its ID, tracking changes.
    pub fn set_icon_custom(&mut self, custom_icon_id: CustomIconId) -> Result<(), CustomIconNotFoundError> {
        let mut this = self.as_mut();
        this.set_icon_custom(custom_icon_id)?;
        this.times.last_modification = Some(Times::now());
        Ok(())
    }

    /// Set a custom icon for this entry by providing the raw data, tracking changes. Returns a mutable reference to the newly created custom icon.
    pub fn set_icon_custom_new(&mut self, data: Vec<u8>) -> CustomIconMut<'_> {
        self.set_icon_none();

        let custom_icon_id = CustomIconId::new();

        self.database.custom_icons.insert(
            custom_icon_id,
            CustomIcon {
                id: custom_icon_id,
                groups: HashSet::new(),
                name: None,
                last_modification_time: Some(Times::now()),
                data,
            },
        );

        self.icon = Some(Icon::Custom(custom_icon_id));

        CustomIconMut::new(self.database, custom_icon_id)
    }
}

impl Deref for EntryTrack<'_> {
    type Target = Entry;

    #[allow(clippy::expect_used, clippy::missing_panics_doc)] // entry existence is guaranteed
    fn deref(&self) -> &Self::Target {
        self.database.entries.get(&self.id).expect("Entry not found")
    }
}

impl DerefMut for EntryTrack<'_> {
    #[allow(clippy::expect_used, clippy::missing_panics_doc)] // entry existence is guaranteed
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.database.entries.get_mut(&self.id).expect("Entry not found")
    }
}

impl Drop for EntryTrack<'_> {
    fn drop(&mut self) {
        // see if the entry is still there (it might have been removed)
        if let Some(entry) = self.database.entries.get_mut(&self.id) {
            let parent_id = entry.parent;
            let historical = std::mem::replace(&mut self.historical, Entry::new(parent_id));

            entry.history.get_or_insert_default().add_entry(historical);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {

    use crate::{
        db::{fields, Value},
        Database,
    };

    #[test]
    fn test_entry() {
        let mut db = Database::new();

        let entry_id = db
            .root_mut()
            .add_entry()
            .edit(|e| {
                e.set_unprotected(fields::TITLE, "Entry 1");
                e.set(
                    fields::USERNAME,
                    crate::db::Value::unprotected("user".to_string()),
                );
                e.set_protected(fields::PASSWORD, "asdf");

                e.set_icon_custom_new(vec![1, 2, 3]);
            })
            .id();

        assert_eq!(db.num_attachments(), 0);
        assert_eq!(db.num_entries(), 1);

        assert_eq!(
            db.entry(entry_id).unwrap().history.clone().unwrap().entries.len(),
            0
        );

        assert_eq!(db.entry(entry_id).unwrap().get(fields::TITLE).unwrap(), "Entry 1");

        db.entry_mut(entry_id).unwrap().edit_tracking(|e| {
            e.set_unprotected(fields::TITLE, "Modified Entry 1");
            e.set(
                fields::USERNAME,
                crate::db::Value::unprotected(format!("modified_{}", e.get(fields::USERNAME).unwrap())),
            );

            e.add_attachment("Attachment 1", Value::protected(b"Attachment data".to_vec()));
        });

        assert_eq!(db.num_attachments(), 1);
        assert_eq!(db.num_entries(), 1);
        assert_eq!(
            db.entry(entry_id).unwrap().history.clone().unwrap().entries.len(),
            1
        );

        assert!(db
            .entry(entry_id)
            .unwrap()
            .attachments
            .contains_key("Attachment 1"));

        assert_eq!(
            db.entry(entry_id).unwrap().get(fields::TITLE).unwrap(),
            "Modified Entry 1"
        );

        // test moving to a non-existent group returns an error and does not modify the entry
        assert!(db
            .entry_mut(entry_id)
            .unwrap()
            .move_to(crate::db::GroupId::new())
            .is_err());

        db.entry_mut(entry_id).unwrap().edit(|e| {
            let mut att = e.attachment_by_name_mut("Attachment 1").unwrap();

            att.data = Value::unprotected(b"Modified attachment data".to_vec());
        });

        db.entry_mut(entry_id).unwrap().remove();

        assert_eq!(db.num_entries(), 0);
        assert_eq!(db.num_attachments(), 0);
    }
}
