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
