use super::*;

fn edit(line: u32, start: u32, end: u32, new: &str) -> lsp_types::TextEdit {
    lsp_types::TextEdit {
        range: lsp_types::Range {
            start: lsp_types::Position::new(line, start),
            end: lsp_types::Position::new(line, end),
        },
        new_text: new.to_string(),
    }
}

#[test]
fn applies_edits_bottom_up() {
    let mut text = String::from("fn one() {}\nfn two() {}\n");
    apply_text_edits(
        &mut text,
        &[edit(1, 3, 6, "double"), edit(0, 3, 6, "single")],
        false,
    )
    .unwrap();
    assert_eq!(text, "fn single() {}\nfn double() {}\n");
}

#[test]
fn rejects_overlapping_edits() {
    let mut text = String::from("abcdef\n");
    let err = apply_text_edits(&mut text, &[edit(0, 0, 3, "X"), edit(0, 2, 5, "Y")], false);
    assert!(err.is_err());
}

#[test]
fn utf16_ranges_map_through_multibyte_lines() {
    // "ab😀cd ef" — 😀 occupies utf-16 units 2..4; 'e' is unit 7.
    let mut text = String::from("ab😀cd ef\n");
    apply_text_edits(&mut text, &[edit(0, 7, 8, "E")], false).unwrap();
    assert_eq!(text, "ab😀cd Ef\n");
}

#[test]
fn same_position_inserts_keep_array_order() {
    // Spec test case: same-start inserts appear in array order in
    // the resulting text ("WorldHello", not "HelloWorld").
    let mut text = String::from("abc");
    apply_text_edits(
        &mut text,
        &[edit(0, 3, 3, "World"), edit(0, 3, 3, "Hello")],
        false,
    )
    .unwrap();
    assert_eq!(text, "abcWorldHello");
}

#[test]
fn mixed_inserts_and_replace_at_same_start() {
    // Any number of inserts at one position followed by a replace
    // at that position is a legal batch (spec): inserts land in
    // array order, the replace consumes the original bytes.
    let mut text = String::from("abcdef");
    apply_text_edits(
        &mut text,
        &[edit(0, 2, 2, "1"), edit(0, 2, 2, "2"), edit(0, 2, 4, "X")],
        false,
    )
    .unwrap();
    assert_eq!(text, "ab12Xef");
}

#[test]
fn invalid_utf8_splice_fails_closed() {
    // A range whose byte offsets split a multibyte char cannot
    // produce valid utf-8; the edit is refused, not corrupted.
    let mut text = String::from("ab😀cd\n");
    let err = apply_text_edits(
        &mut text,
        // 😀 occupies bytes 2..6; cutting at byte 4 splits it.
        // Send raw byte offsets via the utf-8 encoding flag.
        &[edit(0, 4, 6, "X")],
        true,
    );
    assert!(
        err.is_err(),
        "splice through a multibyte char must fail closed"
    );
}

#[test]
fn line_starts_count_every_newline() {
    assert_eq!(line_start_offsets("a\nbb\nccc\n"), vec![0, 2, 5, 9]);
    assert_eq!(line_start_offsets("no newline"), vec![0]);
}

#[tokio::test]
async fn rollback_on_mid_batch_failure() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file.txt");
    std::fs::write(&file, "original").unwrap();
    // A regular FILE sitting where the second op needs a directory:
    // create_dir_all cannot succeed, so op 2 fails after op 1 wrote.
    let barrier = dir.path().join("barrier");
    std::fs::write(&barrier, "i am a file").unwrap();
    let doomed = barrier.join("inner.txt");

    let plan = vec![
        PlannedOp {
            op: PlanOp::Edit {
                path: file.clone(),
                text: "changed".to_owned(),
            },
            undo: Undo::WriteBytes {
                path: file.clone(),
                bytes: b"original".to_vec(),
            },
            before_images: Vec::new(),
        },
        PlannedOp {
            op: PlanOp::Create { path: doomed },
            undo: Undo::RemoveFile(barrier.join("inner.txt")),
            before_images: Vec::new(),
        },
    ];
    let err = apply_plan(plan, dir.path())
        .await
        .expect_err("second op must fail");
    assert!(err.contains("rolled back"), "{err}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
}

#[tokio::test]
async fn effects_carry_relative_keys_and_before_images() {
    // Tier-2 contract of the LSP write path (docs/rewind-design.md):
    // effect keys are workspace-relative like every other loom
    // write tool, and the pre-images captured at plan time ride the
    // effects of the committed batch. Hand-built plan, because
    // planning itself needs a live server process.
    use loom_kernel::artifact::store_artifact;
    use loom_kernel::store::MemoryArtifactStore;

    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("src").join("lib.rs");
    std::fs::create_dir_all(raw.parent().unwrap()).unwrap();
    std::fs::write(&raw, "fn old() {}\n").unwrap();
    // Production paths arrive canonical (resolve_within output);
    // match that here — macOS tempdirs are symlinked (/var →
    // /private/var) and workspace_relative strips against the
    // canonical form only.
    let file = raw.canonicalize().unwrap();
    let late = {
        let raw = dir.path().join("notes.txt");
        std::fs::write(&raw, "late").unwrap();
        raw.canonicalize().unwrap()
    };

    let artifacts = MemoryArtifactStore::new();
    let lib_ref = store_artifact(&artifacts, "pre_image", b"fn old() {}\n").unwrap();

    let plan = vec![
        PlannedOp {
            op: PlanOp::Edit {
                path: file.clone(),
                text: "fn new() {}\n".to_owned(),
            },
            undo: Undo::WriteBytes {
                path: file.clone(),
                bytes: b"fn old() {}\n".to_vec(),
            },
            before_images: vec![BeforeImage::captured("src/lib.rs", lib_ref.clone())],
        },
        PlannedOp {
            op: PlanOp::Delete { path: late },
            undo: Undo::WriteBytes {
                path: dir.path().join("notes.txt"),
                bytes: b"late".to_vec(),
            },
            before_images: vec![BeforeImage::absent("notes.txt")],
        },
    ];
    let root = dir.path().canonicalize().unwrap();
    let (effects, notes) = apply_plan(plan, &root).await.unwrap();

    // Relative keys: the transcript claims workspace paths, not
    // machine-specific absolute ones.
    assert_eq!(effects.files_written, vec!["src/lib.rs".to_owned()]);
    assert_eq!(effects.files_deleted, vec!["notes.txt".to_owned()]);
    // Pre-images keyed by the same strings, in plan order.
    assert_eq!(effects.before_images.len(), 2);
    assert_eq!(effects.before_images[0].path, "src/lib.rs");
    assert_eq!(effects.before_images[1].path, "notes.txt");
    assert!(effects.before_images[1].content.is_none());
    // Bodies resolve: what the restore will write back is exactly
    // the pre-edit content.
    let stored = artifacts
        .get(&lib_ref.id)
        .expect("store read")
        .expect("pre-image body stored at plan time")
        .body;
    assert_eq!(stored, b"fn old() {}\n");
    // Sync notes stay absolute — they key open documents.
    assert_eq!(
        notes,
        vec![(
            file.to_string_lossy().into_owned(),
            "fn new() {}\n".to_owned()
        )]
    );
}

#[tokio::test]
async fn undo_restores_replaced_rename_target() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("a.txt");
    let to = dir.path().join("b.txt");
    std::fs::write(&from, "moved").unwrap();
    std::fs::write(&to, "clobbered original").unwrap();

    let undo = Undo::RenameBack {
        from: from.clone(),
        to: to.clone(),
        replaced: Some(b"clobbered original".to_vec()),
    };
    tokio::fs::rename(&from, &to).await.unwrap();
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "moved");
    undo.rollback().await.unwrap();
    assert_eq!(std::fs::read_to_string(&from).unwrap(), "moved");
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "clobbered original");
}
use super::text::line_start_offsets;
