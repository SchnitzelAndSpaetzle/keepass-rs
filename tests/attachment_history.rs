//! Regression tests for safe attachment history retention and binary-pool compaction.
//!
//! These cover the behaviour described in
//! <https://github.com/SchnitzelAndSpaetzle/keepass-rs/issues/2>: deleting a live attachment
//! must not corrupt a history entry that still references it, referenced binaries must survive
//! save/reopen, freed ids must not be reused while a history version still points at them, and
//! [`Database::compact_attachments`] must prune orphans and re-index survivors to contiguous ids.
#![cfg(feature = "save_kdbx4")]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::{combo_by_label, save_to_vec, Combo};
use keepass::db::{Database, EntryId, Value};
use uuid::uuid;

const ENTRY_ID: EntryId = EntryId::from_uuid(uuid!("00000000-0000-0000-0000-0000000000a1"));

fn combo() -> Combo {
    combo_by_label("aes256+none+inner-chacha20+argon2d")
}

/// Build a database whose single entry holds attachment `A.bin` both in its live version and in a
/// history snapshot. The history snapshot is created by editing the entry through a change-tracking
/// handle, which records the pre-edit state (still referencing `A.bin`) into history on drop.
fn build_entry_with_history(combo: &Combo) -> Database {
    let mut db = Database::with_config(combo.get_config());

    {
        let mut root = db.root_mut();
        let mut e = root.add_entry_with_id(ENTRY_ID).unwrap();
        e.set_unprotected("Title", "e1");
        e.add_attachment("A.bin", Value::Unprotected(b"AAA".to_vec()));
    }

    {
        let mut e = db.entry_mut(ENTRY_ID).unwrap();
        let mut tracked = e.track_changes();
        tracked.set_unprotected("Title", "e1-v2");
    }

    // Sanity: both the live version and history[0] reference the attachment.
    let e = db.entry(ENTRY_ID).unwrap();
    assert_eq!(
        e.attachment_by_name("A.bin").unwrap().data.get().as_slice(),
        b"AAA",
        "live version should reference A.bin"
    );
    assert_eq!(
        e.historical(0)
            .unwrap()
            .attachment_by_name("A.bin")
            .unwrap()
            .data
            .get()
            .as_slice(),
        b"AAA",
        "history[0] should reference A.bin"
    );

    db
}

/// Deleting an attachment from the live entry must not break a history entry that still references
/// it (pure in-memory, no save).
#[test]
fn delete_live_attachment_keeps_history_ref() {
    let combo = combo();
    let mut db = build_entry_with_history(&combo);

    db.entry_mut(ENTRY_ID).unwrap().remove_attachment_by_name("A.bin");

    let e = db.entry(ENTRY_ID).unwrap();
    assert!(
        e.attachment_by_name("A.bin").is_none(),
        "live version should no longer reference A.bin"
    );

    let history = e.historical(0).expect("history entry should still exist");
    assert_eq!(
        history
            .attachment_by_name("A.bin")
            .expect("history keeps A.bin")
            .data
            .get()
            .as_slice(),
        b"AAA",
        "history version must still resolve to the original bytes"
    );
}

/// A history-only attachment must survive compaction + save + reopen.
#[test]
fn history_attachment_survives_save_reopen() {
    let combo = combo();
    let mut db = build_entry_with_history(&combo);

    db.entry_mut(ENTRY_ID).unwrap().remove_attachment_by_name("A.bin");
    db.compact_attachments();

    let bytes = save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");

    let e = parsed.entry(ENTRY_ID).unwrap();
    assert!(
        e.attachment_by_name("A.bin").is_none(),
        "live version should not reference A.bin"
    );

    let history = e.historical(0).expect("history entry should survive reopen");
    assert_eq!(
        history
            .attachment_by_name("A.bin")
            .expect("history keeps A.bin")
            .data
            .get()
            .as_slice(),
        b"AAA",
        "history attachment bytes must survive save/reopen"
    );
}

/// Adding a new attachment after deleting an old (history-referenced) one must not make the history
/// version resolve to the new bytes (i.e. the freed id must not be reused).
#[test]
fn add_after_delete_does_not_alias_history() {
    let combo = combo();
    let mut db = build_entry_with_history(&combo);

    {
        let mut e = db.entry_mut(ENTRY_ID).unwrap();
        e.remove_attachment_by_name("A.bin");
        e.add_attachment("B.bin", Value::Unprotected(b"BBB".to_vec()));
    }

    // In-memory: history must still see AAA, live sees BBB.
    {
        let e = db.entry(ENTRY_ID).unwrap();
        assert_eq!(
            e.historical(0)
                .unwrap()
                .attachment_by_name("A.bin")
                .unwrap()
                .data
                .get()
                .as_slice(),
            b"AAA",
            "history must not alias the newly added attachment"
        );
        assert_eq!(
            e.attachment_by_name("B.bin").unwrap().data.get().as_slice(),
            b"BBB",
        );
    }

    db.compact_attachments();
    let bytes = save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");

    let e = parsed.entry(ENTRY_ID).unwrap();
    assert_eq!(
        e.historical(0)
            .unwrap()
            .attachment_by_name("A.bin")
            .unwrap()
            .data
            .get()
            .as_slice(),
        b"AAA",
        "history must still resolve to AAA after save/reopen"
    );
    assert_eq!(
        e.attachment_by_name("B.bin").unwrap().data.get().as_slice(),
        b"BBB",
    );
    assert_eq!(
        parsed.num_attachments(),
        2,
        "both A (history) and B (live) must be retained"
    );
}

/// An attachment referenced by no live or history version must be pruned by compaction and must not
/// be resurrected on save/reopen.
#[test]
fn orphan_attachment_pruned_before_save() {
    let combo = combo();
    let mut db = Database::with_config(combo.get_config());

    {
        let mut root = db.root_mut();
        let mut e = root.add_entry_with_id(ENTRY_ID).unwrap();
        e.set_unprotected("Title", "e1");
        e.add_attachment("A.bin", Value::Unprotected(b"AAA".to_vec()));
    }

    // Removing the only (live) reference no longer GCs immediately; the orphan lingers until compaction.
    db.entry_mut(ENTRY_ID).unwrap().remove_attachment_by_name("A.bin");
    assert_eq!(db.num_attachments(), 1, "orphan should linger before compaction");

    db.compact_attachments();
    assert_eq!(db.num_attachments(), 0, "orphan should be pruned by compaction");

    let bytes = save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");
    assert_eq!(
        parsed.num_attachments(),
        0,
        "pruned orphan must not be resurrected"
    );
}

/// A pool with non-contiguous ids (a higher-id attachment surviving after a lower one is removed)
/// must be compacted so the higher ref survives save/reopen.
#[test]
fn compaction_reindexes_high_refs() {
    let combo = combo();
    let mut db = Database::with_config(combo.get_config());

    {
        let mut root = db.root_mut();
        let mut e = root.add_entry_with_id(ENTRY_ID).unwrap();
        e.add_attachment("A.bin", Value::Unprotected(b"AAA".to_vec())); // id 0
        e.add_attachment("B.bin", Value::Unprotected(b"BBB".to_vec())); // id 1
        e.add_attachment("C.bin", Value::Unprotected(b"CCC".to_vec())); // id 2
    }

    db.entry_mut(ENTRY_ID).unwrap().remove_attachment_by_name("B.bin");
    assert_eq!(db.num_attachments(), 3, "B lingers until compaction");

    db.compact_attachments();
    assert_eq!(
        db.num_attachments(),
        2,
        "B pruned, A and C re-indexed contiguously"
    );

    let bytes = save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");
    assert_eq!(parsed.num_attachments(), 2);

    let e = parsed.entry(ENTRY_ID).unwrap();
    assert_eq!(
        e.attachment_by_name("A.bin").unwrap().data.get().as_slice(),
        b"AAA"
    );
    assert_eq!(
        e.attachment_by_name("C.bin").unwrap().data.get().as_slice(),
        b"CCC",
        "the higher-id attachment must survive re-indexing and save/reopen"
    );
    assert!(e.attachment_by_name("B.bin").is_none());
}

/// The common path (a single live attachment, no compaction) must round-trip unchanged.
#[test]
fn existing_live_attachment_unchanged() {
    let combo = combo();
    let mut db = Database::with_config(combo.get_config());

    {
        let mut root = db.root_mut();
        let mut e = root.add_entry_with_id(ENTRY_ID).unwrap();
        e.add_attachment("only.bin", Value::Unprotected(b"hello".to_vec()));
    }

    let bytes = save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");

    assert_eq!(parsed.num_attachments(), 1);
    assert_eq!(
        parsed
            .entry(ENTRY_ID)
            .unwrap()
            .attachment_by_name("only.bin")
            .unwrap()
            .data
            .get()
            .as_slice(),
        b"hello",
    );
}

/// A plain `Database::save` (without a manual `compact_attachments`) must not write back the bytes
/// of an attachment whose last live/history reference was removed.
#[test]
fn save_drops_deleted_attachment_without_manual_compaction() {
    let combo = combo();
    let mut db = Database::with_config(combo.get_config());

    {
        let mut root = db.root_mut();
        let mut e = root.add_entry_with_id(ENTRY_ID).unwrap();
        e.add_attachment("secret.bin", Value::Unprotected(b"top-secret".to_vec()));
    }

    // Delete the only reference, then save WITHOUT calling compact_attachments first.
    db.entry_mut(ENTRY_ID)
        .unwrap()
        .remove_attachment_by_name("secret.bin");
    assert_eq!(db.num_attachments(), 1, "deferred GC keeps the binary in memory");

    let bytes = save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");

    assert_eq!(
        parsed.num_attachments(),
        0,
        "deleted attachment bytes must not be written back by a plain save"
    );
    assert!(parsed
        .entry(ENTRY_ID)
        .unwrap()
        .attachment_by_name("secret.bin")
        .is_none());

    // The caller's in-memory database is untouched by the save (non-mutating compaction).
    assert_eq!(db.num_attachments(), 1, "save must not mutate the caller's pool");
}

/// A history-referenced attachment must still be written by a plain save even though a higher-id
/// sibling was deleted (compacted view must not drop or misalign it).
#[test]
fn save_keeps_history_attachment_without_manual_compaction() {
    let combo = combo();
    let mut db = build_entry_with_history(&combo);

    db.entry_mut(ENTRY_ID).unwrap().remove_attachment_by_name("A.bin");

    // No compact_attachments() call here.
    let bytes = save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");

    assert_eq!(parsed.num_attachments(), 1);
    let e = parsed.entry(ENTRY_ID).unwrap();
    assert!(
        e.attachment_by_name("A.bin").is_none(),
        "live no longer references A.bin"
    );
    assert_eq!(
        e.historical(0)
            .unwrap()
            .attachment_by_name("A.bin")
            .unwrap()
            .data
            .get()
            .as_slice(),
        b"AAA",
        "history attachment must survive a plain save"
    );
}

/// Removing an entry that, after reopen, references a binary only through a history version must not
/// leave a dangling back-reference. Reconstructed back-references include `(entry_id, Some(i))`, so a
/// naive removal that only visits the live version would orphan that back-reference and make
/// `AttachmentRef::entries(true)` panic when it dereferences the now-missing entry.
#[test]
fn removing_entry_clears_history_only_attachment_refs() {
    let combo = combo();
    let mut db = build_entry_with_history(&combo);

    // Make A.bin history-only, then round-trip so the back-reference set is rebuilt on load.
    db.entry_mut(ENTRY_ID).unwrap().remove_attachment_by_name("A.bin");
    db.compact_attachments();
    let bytes = save_to_vec(&db, combo.get_key());
    let mut db = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");
    assert_eq!(
        db.num_attachments(),
        1,
        "A.bin is retained via history before removal"
    );

    // Removing the entry must clean the history back-reference (no panic) and free the binary.
    db.entry_mut(ENTRY_ID).unwrap().remove();

    // Enumerating remaining attachments (including historical referrers) must not panic on a
    // dangling EntryRef.
    for attachment in db.iter_all_attachments() {
        let _ = attachment.entries(true).count();
    }
    assert_eq!(
        db.num_attachments(),
        0,
        "the orphaned history attachment must be freed"
    );
}

/// Build a database, reopened from disk, where attachment `A.bin` is referenced only by `history[0]`,
/// then a change-tracking edit inserts a new snapshot at index 0 (shifting the real reference to
/// `history[1]`). Returns the reopened, edited database.
fn history_only_attachment_then_shifted(combo: &Combo) -> Database {
    let mut db = build_entry_with_history(combo);
    db.entry_mut(ENTRY_ID).unwrap().remove_attachment_by_name("A.bin");
    db.compact_attachments();
    let bytes = save_to_vec(&db, combo.get_key());
    let mut db = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");

    // A change-tracked edit pushes a new snapshot (which does not reference A.bin) to index 0,
    // shifting the snapshot that does reference A.bin from index 0 to index 1.
    {
        let mut e = db.entry_mut(ENTRY_ID).unwrap();
        let mut tracked = e.track_changes();
        tracked.set_unprotected("Title", "e1-v3");
    }
    db
}

/// After a history-index shift, the attachment must still resolve to the correct (shifted) history
/// version, `entries(true)` must not be stale or panic, and a save must round-trip it.
#[test]
fn history_index_shift_resolves_correct_version() {
    let combo = combo();
    let db = history_only_attachment_then_shifted(&combo);

    {
        let e = db.entry(ENTRY_ID).unwrap();
        assert!(
            e.historical(0).unwrap().attachment_by_name("A.bin").is_none(),
            "the new snapshot at index 0 does not reference A.bin"
        );
        assert_eq!(
            e.historical(1)
                .unwrap()
                .attachment_by_name("A.bin")
                .unwrap()
                .data
                .get()
                .as_slice(),
            b"AAA",
            "the shifted snapshot at index 1 still references A.bin"
        );
    }

    // The derived back-reference enumeration finds exactly the shifted version, without panicking.
    let att_id = db
        .entry(ENTRY_ID)
        .unwrap()
        .historical(1)
        .unwrap()
        .attachment_by_name("A.bin")
        .unwrap()
        .id();
    assert_eq!(db.attachment(att_id).unwrap().entries(true).count(), 1);

    // The attachment survives a plain save/reopen and still resolves under the shifted index.
    let bytes = save_to_vec(&db, combo.get_key());
    let parsed = Database::open(&mut bytes.as_slice(), combo.get_key()).expect("reopen");
    assert_eq!(parsed.num_attachments(), 1);
    assert_eq!(
        parsed
            .entry(ENTRY_ID)
            .unwrap()
            .historical(1)
            .unwrap()
            .attachment_by_name("A.bin")
            .unwrap()
            .data
            .get()
            .as_slice(),
        b"AAA",
    );
}

/// After a history-index shift, removing the attachment via `AttachmentMut::remove` must strip the
/// reference from the real (shifted) history version, not the wrong one, and must not panic.
#[test]
fn remove_after_history_shift_targets_correct_version() {
    let combo = combo();
    let mut db = history_only_attachment_then_shifted(&combo);

    let att_id = db
        .entry(ENTRY_ID)
        .unwrap()
        .historical(1)
        .unwrap()
        .attachment_by_name("A.bin")
        .unwrap()
        .id();

    db.attachment_mut(att_id).unwrap().remove();

    let e = db.entry(ENTRY_ID).unwrap();
    assert!(
        e.historical(1).unwrap().attachment_by_name("A.bin").is_none(),
        "remove must strip the reference from the actual (shifted) history version"
    );
    assert!(e.historical(0).unwrap().attachment_by_name("A.bin").is_none());
    assert_eq!(db.num_attachments(), 0);
}
