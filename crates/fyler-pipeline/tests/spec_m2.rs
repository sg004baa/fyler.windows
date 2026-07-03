//! パイプラインの仕様テスト = M2以降のacceptance criteria。
//!
//! DESIGN.md「行フォーマット」「diff判定ルール」「validateで弾くもの」を
//! 実行可能な形に固定したもの。**実装したら `#[ignore]` を外して通すこと。**
//! テストの期待値を変える場合は、先にDESIGN.mdの該当ルールと矛盾しないか確認する。

use std::collections::HashSet;

use fyler_core::editor::EditorLine;
use fyler_core::plan::FsOperation;
use fyler_core::tree::{
    BaselineEntry, BaselineTree, DesiredEntry, DesiredTree, EditContext, EntryKind,
};
use fyler_core::validate::ValidateError;
use fyler_core::{EntryId, TreePath};
use fyler_pipeline::{diff, parse, validate};

// ---- ヘルパー ----

fn lines(texts: &[&str]) -> Vec<EditorLine> {
    texts.iter().map(|t| EditorLine::new(*t)).collect()
}

fn baseline(entries: &[(u64, &str, EntryKind)]) -> BaselineTree {
    let mut tree = BaselineTree::new("C:/test-root");
    for (id, path, kind) in entries {
        tree.insert(BaselineEntry {
            id: EntryId(*id),
            path: TreePath::parse(path),
            kind: *kind,
        });
    }
    tree
}

fn desired_entry(id: Option<u64>, path: &str, kind: EntryKind, line: usize) -> DesiredEntry {
    DesiredEntry {
        id: id.map(EntryId),
        path: TreePath::parse(path),
        kind,
        line,
    }
}

fn no_collapse() -> EditContext {
    EditContext::default()
}

fn collapsed(ids: &[u64]) -> EditContext {
    EditContext {
        collapsed_dirs: ids.iter().copied().map(EntryId).collect::<HashSet<_>>(),
    }
}

// ---- parse(DESIGN.md「行フォーマット」の例そのまま) ----

#[test]
#[ignore = "unimplemented: M2 parse"]
fn parse_design_doc_example() {
    let buf = lines(&[
        "/012 src/",
        "/013   main.rs",
        "/014   lib.rs",
        "新規ファイル.txt",
    ]);
    let parsed = parse::parse(&buf);

    assert_eq!(parsed.len(), 4);

    assert_eq!(parsed[0].id, Some(EntryId(12)));
    assert_eq!(parsed[0].indent_spaces, 0);
    assert_eq!(parsed[0].name, "src");
    assert!(parsed[0].is_dir);

    assert_eq!(parsed[1].id, Some(EntryId(13)));
    assert_eq!(parsed[1].indent_spaces, 2);
    assert_eq!(parsed[1].name, "main.rs");
    assert!(!parsed[1].is_dir);

    // IDのない行 = CREATE候補
    assert_eq!(parsed[3].id, None);
    assert!(!parsed[3].id_broken);
    assert_eq!(parsed[3].name, "新規ファイル.txt");
}

#[test]
#[ignore = "unimplemented: M2 parse"]
fn parse_skips_empty_lines_but_keeps_line_numbers() {
    let buf = lines(&["/001 a.txt", "", "   ", "/002 b.txt"]);
    let parsed = parse::parse(&buf);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].line, 0);
    assert_eq!(parsed[1].line, 3); // 空行スキップ後も元の行番号
}

#[test]
#[ignore = "unimplemented: M2 parse"]
fn parse_flags_broken_id_prefix() {
    // 「/0だけ残っている等」(DESIGN.md)
    let buf = lines(&["/0", "/12x foo.txt"]);
    let parsed = parse::parse(&buf);
    assert!(parsed.iter().all(|p| p.id_broken));
}

#[test]
#[ignore = "unimplemented: M2 parse"]
fn to_desired_tree_builds_nested_paths() {
    let buf = lines(&["/012 src/", "/013   main.rs", "新規.txt"]);
    let tree = parse::to_desired_tree(&parse::parse(&buf)).unwrap();

    assert_eq!(tree.entries.len(), 3);
    assert_eq!(tree.entries[0].path, TreePath::parse("src"));
    assert_eq!(tree.entries[0].kind, EntryKind::Dir);
    assert_eq!(tree.entries[1].path, TreePath::parse("src/main.rs"));
    assert_eq!(tree.entries[2].path, TreePath::parse("新規.txt"));
}

#[test]
#[ignore = "unimplemented: M2 parse"]
fn to_desired_tree_rejects_broken_prefix_and_bad_indent() {
    // broken prefix → 保存中断
    let buf = lines(&["/0"]);
    let errors = parse::to_desired_tree(&parse::parse(&buf)).unwrap_err();
    assert!(matches!(
        errors[0],
        ValidateError::BrokenIdPrefix { line: 0 }
    ));

    // 親を飛ばした深いインデント → InvalidIndent
    let buf = lines(&["/001 a/", "/002     too_deep.txt"]); // depth 0 → depth 2
    let errors = parse::to_desired_tree(&parse::parse(&buf)).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidateError::InvalidIndent { line: 1 }))
    );
}

// ---- validate(DESIGN.md「validateで弾くもの」) ----

#[test]
#[ignore = "unimplemented: M2 validate"]
fn validate_rejects_duplicate_names_in_same_dir() {
    let base = baseline(&[]);
    let desired = DesiredTree {
        entries: vec![
            desired_entry(None, "a.txt", EntryKind::File, 0),
            desired_entry(None, "a.txt", EntryKind::File, 1),
        ],
    };
    let errors = validate::validate(&base, &desired, &no_collapse());
    assert!(errors.iter().any(|e| matches!(
        e,
        ValidateError::DuplicateName { path } if *path == TreePath::parse("a.txt")
    )));
}

#[test]
#[ignore = "unimplemented: M2 validate"]
fn validate_rejects_windows_naming_violations() {
    let base = baseline(&[]);
    let desired = DesiredTree {
        entries: vec![
            desired_entry(None, "a<b.txt", EntryKind::File, 0), // 予約文字
            desired_entry(None, "CON.txt", EntryKind::File, 1), // 予約名(拡張子付き)
            desired_entry(None, "name.", EntryKind::File, 2),   // 末尾ピリオド
            desired_entry(None, "name ", EntryKind::File, 3),   // 末尾スペース
        ],
    };
    let errors = validate::validate(&base, &desired, &no_collapse());
    assert!(errors.iter().any(|e| matches!(
        e,
        ValidateError::ReservedChar {
            line: 0,
            ch: '<',
            ..
        }
    )));
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidateError::ReservedName { line: 1, .. }))
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidateError::InvalidTrailing { line: 2, .. }))
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidateError::InvalidTrailing { line: 3, .. }))
    );
}

#[test]
#[ignore = "unimplemented: M2 validate"]
fn validate_accepts_clean_tree() {
    let base = baseline(&[
        (1, "src", EntryKind::Dir),
        (2, "src/main.rs", EntryKind::File),
    ]);
    let desired = DesiredTree {
        entries: vec![
            desired_entry(Some(1), "src", EntryKind::Dir, 0),
            desired_entry(Some(2), "src/main.rs", EntryKind::File, 1),
        ],
    };
    assert!(validate::validate(&base, &desired, &no_collapse()).is_empty());
}

// ---- diff(DESIGN.md「diff判定ルール」) ----

#[test]
#[ignore = "unimplemented: M2 diff (rename)"]
fn diff_unchanged_tree_yields_empty_plan() {
    let base = baseline(&[(1, "a.txt", EntryKind::File)]);
    let desired = DesiredTree {
        entries: vec![desired_entry(Some(1), "a.txt", EntryKind::File, 0)],
    };
    let plan = diff::build_plan(&base, &desired, &no_collapse());
    assert!(plan.is_empty());
}

#[test]
#[ignore = "unimplemented: M2 diff (rename)"]
fn diff_rename_is_move_with_same_parent() {
    // M2のゴール: 「iでrenameを書いて:wするとダイアログに RENAME a → b が出る」
    let base = baseline(&[(1, "a.txt", EntryKind::File)]);
    let desired = DesiredTree {
        entries: vec![desired_entry(Some(1), "b.txt", EntryKind::File, 0)],
    };
    let plan = diff::build_plan(&base, &desired, &no_collapse());
    assert_eq!(
        plan.ops,
        vec![FsOperation::Move {
            id: EntryId(1),
            from: TreePath::parse("a.txt"),
            to: TreePath::parse("b.txt"),
        }]
    );
}

#[test]
#[ignore = "unimplemented: M3 diff (create/delete)"]
fn diff_missing_id_is_delete_and_new_line_is_create() {
    let base = baseline(&[(1, "old.txt", EntryKind::File)]);
    let desired = DesiredTree {
        entries: vec![desired_entry(None, "new.txt", EntryKind::File, 0)],
    };
    let plan = diff::build_plan(&base, &desired, &no_collapse());
    assert!(plan.ops.contains(&FsOperation::Create {
        path: TreePath::parse("new.txt"),
        kind: EntryKind::File,
    }));
    assert!(plan.ops.contains(&FsOperation::Delete {
        id: EntryId(1),
        path: TreePath::parse("old.txt"),
    }));
    assert_eq!(plan.ops.len(), 2);
}

#[test]
#[ignore = "unimplemented: M4 diff (move)"]
fn diff_move_to_other_directory() {
    let base = baseline(&[
        (1, "src", EntryKind::Dir),
        (2, "src/main.rs", EntryKind::File),
        (3, "dst", EntryKind::Dir),
    ]);
    let desired = DesiredTree {
        entries: vec![
            desired_entry(Some(1), "src", EntryKind::Dir, 0),
            desired_entry(Some(3), "dst", EntryKind::Dir, 1),
            desired_entry(Some(2), "dst/main.rs", EntryKind::File, 2),
        ],
    };
    let plan = diff::build_plan(&base, &desired, &no_collapse());
    assert_eq!(
        plan.ops,
        vec![FsOperation::Move {
            id: EntryId(2),
            from: TreePath::parse("src/main.rs"),
            to: TreePath::parse("dst/main.rs"),
        }]
    );
}

#[test]
#[ignore = "unimplemented: M4 diff (copy)"]
fn diff_duplicated_id_is_copy() {
    // yy → p: baselineと同一パスの行が元位置、残りがCOPY
    let base = baseline(&[(1, "a.txt", EntryKind::File)]);
    let desired = DesiredTree {
        entries: vec![
            desired_entry(Some(1), "a.txt", EntryKind::File, 0),
            desired_entry(Some(1), "b.txt", EntryKind::File, 1),
        ],
    };
    let plan = diff::build_plan(&base, &desired, &no_collapse());
    assert_eq!(
        plan.ops,
        vec![FsOperation::Copy {
            src: EntryId(1),
            from: TreePath::parse("a.txt"),
            to: TreePath::parse("b.txt"),
        }]
    );
}

#[test]
#[ignore = "unimplemented: M4 diff (collapsed dir)"]
fn diff_collapsed_dir_moves_as_one_op_and_children_are_not_deleted() {
    // collapsedなディレクトリの子孫はバッファに現れないが、DELETEではない。
    // rename/moveは子孫ごと = planには親1件のみ。
    let base = baseline(&[
        (1, "src", EntryKind::Dir),
        (2, "src/main.rs", EntryKind::File),
    ]);
    let desired = DesiredTree {
        entries: vec![desired_entry(Some(1), "lib", EntryKind::Dir, 0)],
    };
    let plan = diff::build_plan(&base, &desired, &collapsed(&[1]));
    assert_eq!(
        plan.ops,
        vec![FsOperation::Move {
            id: EntryId(1),
            from: TreePath::parse("src"),
            to: TreePath::parse("lib"),
        }]
    );
}

#[test]
#[ignore = "unimplemented: M3 diff (expanded dir children delete)"]
fn diff_expanded_dir_missing_children_are_deleted() {
    // 展開中(collapsedでない)ディレクトリの子孫がバッファから消えていればDELETE
    let base = baseline(&[
        (1, "src", EntryKind::Dir),
        (2, "src/main.rs", EntryKind::File),
    ]);
    let desired = DesiredTree {
        entries: vec![desired_entry(Some(1), "src", EntryKind::Dir, 0)],
    };
    let plan = diff::build_plan(&base, &desired, &no_collapse());
    assert_eq!(
        plan.ops,
        vec![FsOperation::Delete {
            id: EntryId(2),
            path: TreePath::parse("src/main.rs"),
        }]
    );
}
