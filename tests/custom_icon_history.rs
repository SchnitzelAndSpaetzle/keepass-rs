//! Regression tests for safe custom-icon history back-references.
//!
//! These cover the behaviour described in
//! <https://github.com/SchnitzelAndSpaetzle/keepass-rs/issues/4>: a `CustomIcon` referenced only by
//! a history version must keep resolving to the correct version after `History::add_entry` shifts the
//! snapshots, enumerating referrers must never point at a stale positional history index or panic, and
//! removing the icon must strip the reference from the real (shifted) version. This mirrors the
//! attachment hardening in `tests/attachment_history.rs` (issue #2 / #3); the difference is that custom
//! icons are keyed by a stable `CustomIconId` (UUID), so no binary-pool compaction is involved.
#![cfg(feature = "save_kdbx4")]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::{combo_by_label, save_to_vec, Combo};
use keepass::db::{CustomIconId, Database, EntryId};
use uuid::uuid;

const ENTRY_ID: EntryId = EntryId::from_uuid(uuid!("00000000-0000-0000-0000-0000000000c1"));
const ICON_DATA: &[u8] = b"\x89PNG\r\n\x1a\nFAKE-ICON";

fn combo() -> Combo {
    combo_by_label("aes256+none+inner-chacha20+argon2d")
}

/// Build a database whose single entry holds a custom icon both in its live version and in a history
/// snapshot. The history snapshot is created by editing the entry through a change-tracking handle,
/// which records the pre-edit state (still referencing the icon) into history on drop.
fn build_entry_with_history(combo: &Combo) -> (Database, CustomIconId) {
    let mut db = Database::with_config(combo.get_config());

    let icon_id = {
        let mut root = db.root_mut();
        let mut e = root.add_entry_with_id(ENTRY_ID).unwrap();
        e.set_unprotected("Title", "e1");
        e.set_icon_custom_new(ICON_DATA.to_vec()).id()
    };

    {
        let mut e = db.entry_mut(ENTRY_ID).unwrap();
        let mut tracked = e.track_changes();
        tracked.set_unprotected("Title", "e1-v2");
    }

    // Sanity: both the live version and history[0] reference the icon.
    let e = db.entry(ENTRY_ID).unwrap();
    assert_eq!(
        e.custom_icon().unwrap().id(),
        icon_id,
        "live version should reference the custom icon"
    );
    assert_eq!(
        e.historical(0).unwrap().custom_icon().unwrap().id(),
        icon_id,
        "history[0] should reference the custom icon"
    );

    (db, icon_id)
}

/// Removing the icon from the live entry must not break a history entry that still references it
/// (pure in-memory, no save).
#[test]
fn delete_live_icon_keeps_history_ref() {
    let combo = combo();
    let (mut db, icon_id) = build_entry_with_history(&combo);

    db.entry_mut(ENTRY_ID).unwrap().set_icon_none();

    let e = db.entry(ENTRY_ID).unwrap();
    assert!(
        e.custom_icon().is_none(),
        "live version should no longer reference the icon"
    );
    assert_eq!(
        e.historical(0).unwrap().custom_icon().unwrap().id(),
        icon_id,
        "history version must still resolve to the icon"
    );
    assert_eq!(db.num_custom_icons(), 1, "the icon must remain in the pool");
}

/// Build a database, reopened from disk, where the custom icon is referenced only by `history[0]`,
/// then a change-tracking edit inserts a new snapshot at index 0 (shifting the real reference to
/// `history[1]`). Returns the reopened, edited database.
fn history_only_icon_then_shifted(combo: &Combo) -> (Database, CustomIconId) {
    let (mut db, icon_id) = build_entry_with_history(combo);

    // Make the icon history-only, then round-trip so the back-reference set is rebuilt on load.
    db.entry_mut(ENTRY_ID).unwrap().set_icon_none();
    let bytes = save_to_vec(&db, combo.get_key());
    let mut db = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");
    assert_eq!(
        db.num_custom_icons(),
        1,
        "icon is retained via history across save/reopen"
    );

    // A change-tracked edit pushes a new snapshot (which does not reference the icon) to index 0,
    // shifting the snapshot that does reference the icon from index 0 to index 1.
    {
        let mut e = db.entry_mut(ENTRY_ID).unwrap();
        let mut tracked = e.track_changes();
        tracked.set_unprotected("Title", "e1-v3");
    }

    (db, icon_id)
}

/// After a history-index shift, the icon must still resolve to the correct (shifted) history version,
/// `entries(true)` must not be stale or panic, and a save must round-trip it.
#[test]
fn history_index_shift_resolves_correct_version() {
    let combo = combo();
    let (db, icon_id) = history_only_icon_then_shifted(&combo);

    {
        let e = db.entry(ENTRY_ID).unwrap();
        assert!(
            e.historical(0).unwrap().custom_icon().is_none(),
            "the new snapshot at index 0 does not reference the icon"
        );
        assert_eq!(
            e.historical(1).unwrap().custom_icon().unwrap().id(),
            icon_id,
            "the shifted snapshot at index 1 still references the icon"
        );
    }

    // The derived back-reference enumeration finds exactly the shifted version, without panicking,
    // and every referrer it returns must actually reference the icon (a stale positional history
    // index would point at the icon-less snapshot at index 0 instead).
    let icon_ref = db.custom_icon(icon_id).unwrap();
    let referrers: Vec<_> = icon_ref.entries(true).collect();
    assert_eq!(referrers.len(), 1);
    assert!(
        referrers
            .iter()
            .all(|r| r.custom_icon().map(|ci| ci.id()) == Some(icon_id)),
        "derived referrer must actually reference the icon, not a stale history index"
    );

    // The icon survives a plain save/reopen and still resolves under the shifted index.
    let bytes = save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");
    assert_eq!(parsed.num_custom_icons(), 1);
    assert_eq!(
        parsed
            .entry(ENTRY_ID)
            .unwrap()
            .historical(1)
            .unwrap()
            .custom_icon()
            .unwrap()
            .id(),
        icon_id,
    );
}

/// After a history-index shift, removing the icon via `CustomIconMut::remove` must strip the reference
/// from the real (shifted) history version, not the wrong one, and must not panic.
#[test]
fn remove_after_history_shift_targets_correct_version() {
    let combo = combo();
    let (mut db, icon_id) = history_only_icon_then_shifted(&combo);

    db.custom_icon_mut(icon_id).unwrap().remove();

    let e = db.entry(ENTRY_ID).unwrap();
    assert!(
        e.historical(1).unwrap().custom_icon().is_none(),
        "remove must strip the reference from the actual (shifted) history version"
    );
    assert!(e.historical(0).unwrap().custom_icon().is_none());
    assert_eq!(db.num_custom_icons(), 0);
}

/// Removing an entry that, after reopen, references the icon only through a history version must leave
/// no dangling reference for `CustomIconRef::entries(true)` to trip over.
#[test]
fn removing_entry_clears_history_only_icon_refs() {
    let combo = combo();
    let (mut db, _icon_id) = build_entry_with_history(&combo);

    // Make the icon history-only, then round-trip so the back-reference set is rebuilt on load.
    db.entry_mut(ENTRY_ID).unwrap().set_icon_none();
    let bytes = save_to_vec(&db, combo.get_key());
    let mut db = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");
    assert_eq!(db.num_custom_icons(), 1);

    // Removing the entry must not leave a dangling history back-reference.
    db.entry_mut(ENTRY_ID).unwrap().remove();

    // Enumerating remaining custom icons (including historical referrers) must not panic on a
    // dangling EntryRef.
    for icon in db.iter_all_custom_icons() {
        let _ = icon.entries(true).count();
    }
}
