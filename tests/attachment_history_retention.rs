//! Regression tests for attachment binary retention across entry history.
//!
//! When an attachment is removed from the *live* entry, its binary must NOT be
//! garbage-collected if a historical version of the entry still references it,
//! and its [`AttachmentId`] must never be freed for reuse (which would let a
//! later `add_attachment` silently re-resolve a stale history reference to a
//! different binary). This mirrors how custom icons are already handled
//! (`set_icon_none` never GCs an icon that history may still need).
#![cfg(feature = "save_kdbx4")]
#![forbid(unsafe_code)]
// Tests assert on known-present values; mirrors the `#[allow(clippy::unwrap_used)]`
// on the in-crate `mod tests` (see src/db/types/entry.rs).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::combo_by_label;
use keepass::db::{Database, Value};

const COMBO: &str = "aes256+none+inner-chacha20+argon2d";

#[test]
fn attachment_referenced_by_history_survives_delete_and_reopen() {
    let combo = combo_by_label(COMBO);
    let mut db = Database::with_config(combo.get_config());

    let payload = b"top-secret-recovery-codes".to_vec();

    let entry_id = db
        .root_mut()
        .add_entry()
        .edit(|e| {
            e.set_unprotected("Title", "Bank");
            e.add_attachment("codes.bin", Value::Unprotected(payload.clone()));
        })
        .id();

    // Snapshot the current (attachment-bearing) state into history via a tracked
    // edit. history[0] now references the same attachment id as the live entry.
    db.entry_mut(entry_id)
        .unwrap()
        .edit_tracking(|e| e.set_unprotected("Title", "Bank (renamed)"));

    // Delete the attachment from the *live* entry.
    db.entry_mut(entry_id)
        .unwrap()
        .edit(|e| e.remove_attachment_by_name("codes.bin"));

    // The binary must be retained because a history version still references it.
    assert_eq!(
        db.num_attachments(),
        1,
        "binary was GC'd despite a history reference"
    );

    let bytes = common::save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen failed");

    let root = parsed.root();
    let entry = root.entries().next().expect("no entry under root");

    // The live entry no longer references the attachment...
    assert!(
        entry.attachment_by_name("codes.bin").is_none(),
        "live entry still references the deleted attachment"
    );

    // ...but the historical version still resolves the original bytes.
    let hist = entry.historical(0).expect("missing history version");
    let att = hist
        .attachment_by_name("codes.bin")
        .expect("history lost its attachment binary");
    assert_eq!(
        att.data.get().as_slice(),
        payload.as_slice(),
        "history attachment bytes changed across save/reopen"
    );
}

#[test]
fn deleting_an_attachment_never_reuses_its_id_into_a_history_reference() {
    let combo = combo_by_label(COMBO);
    let mut db = Database::with_config(combo.get_config());

    let original = b"original-bytes".to_vec();
    let entry_id = db
        .root_mut()
        .add_entry()
        .edit(|e| {
            e.add_attachment("a.bin", Value::Unprotected(original.clone()));
        })
        .id();

    let original_id = db
        .entry(entry_id)
        .unwrap()
        .attachment_by_name("a.bin")
        .unwrap()
        .id()
        .id();

    // Snapshot the attachment-bearing state into history, then delete from live.
    db.entry_mut(entry_id)
        .unwrap()
        .edit_tracking(|e| e.set_unprotected("Title", "v2"));
    db.entry_mut(entry_id)
        .unwrap()
        .edit(|e| e.remove_attachment_by_name("a.bin"));

    // Add a *different* attachment. Because the original id is still occupied
    // (never GC'd), AttachmentId::next_free must hand out a fresh id.
    let other = b"different-bytes".to_vec();
    db.entry_mut(entry_id).unwrap().edit(|e| {
        e.add_attachment("b.bin", Value::Unprotected(other.clone()));
    });
    let new_id = db
        .entry(entry_id)
        .unwrap()
        .attachment_by_name("b.bin")
        .unwrap()
        .id()
        .id();

    assert_ne!(new_id, original_id, "freed attachment id was reused");
    assert_eq!(db.num_attachments(), 2, "expected both binaries retained");

    // The history version still resolves to the ORIGINAL, uncorrupted bytes.
    let entry = db.entry(entry_id).unwrap();
    let hist = entry.historical(0).unwrap();
    let att = hist
        .attachment_by_name("a.bin")
        .expect("history lost its attachment");
    assert_eq!(
        att.data.get().as_slice(),
        original.as_slice(),
        "history attachment was corrupted by id reuse"
    );
}
