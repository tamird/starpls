use std::sync::atomic::Ordering;

use starpls_bazel::APIContext;
use starpls_bazel::Builtins;
use starpls_common::Db as _;
use starpls_common::Dialect;
use starpls_common::FileId;
use starpls_common::FileInfo;
use starpls_hir::Db as _;
use starpls_hir::Fixture;

use crate::Analysis;
use crate::Change;
use crate::FilePosition;
use crate::LoadFileResult;

fn hover_value(analysis: &Analysis, file_id: FileId) -> String {
    let file = analysis.db.get_file(file_id).unwrap();
    let pos = file.contents(&analysis.db).rfind("value").unwrap();
    analysis
        .snapshot()
        .hover(FilePosition {
            file_id,
            pos: (pos as u32).into(),
        })
        .unwrap()
        .unwrap()
        .contents
        .value
}

#[test]
fn imported_types_follow_edits_and_open_buffers() {
    let (mut analysis, loader) = Analysis::new_for_test();
    let mut fixture = Fixture::new(&mut analysis.db);
    let main = fixture.add_file(
        &mut analysis.db,
        "main.bzl",
        "load(\"dep.bzl\", \"value\")\nresult = value\n",
    );
    let dependency = FileId(1);
    loader.files.insert(
        "dep.bzl".into(),
        LoadFileResult {
            file_id: dependency,
            dialect: Dialect::Bazel,
            info: None,
            contents: Some("value = 1\n".into()),
        },
    );
    assert!(analysis.db.get_file(dependency).is_none());
    let original = hover_value(&analysis, main);
    assert!(original.contains("Literal[1]"), "{original}");

    let file = analysis.db.get_file(dependency).unwrap();
    let dialect = file.dialect(&analysis.db);
    let info = file.info(&analysis.db);
    let mut change = Change::default();
    change.update_file(dependency, "value = \"edited\"\n".into());
    analysis.apply_change(change);
    let edited = hover_value(&analysis, main);
    assert!(edited.contains("edited"), "{edited}");

    // Opening an already loaded dependency must update the same Salsa input.
    let mut change = Change::default();
    change.create_file(dependency, dialect, info, "value = 1\n".into());
    analysis.apply_change(change);
    assert_eq!(analysis.db.get_file(dependency), Some(file));
    assert_eq!(hover_value(&analysis, main), original);
}

#[test]
fn fetched_modules_invalidate_missing_loads() {
    let (mut analysis, loader) = Analysis::new_for_test();
    let mut fixture = Fixture::new(&mut analysis.db);
    let main = fixture.add_file(
        &mut analysis.db,
        "main.bzl",
        "load(\"external.bzl\", \"value\")\nresult = value\n",
    );
    let missing = hover_value(&analysis, main);
    assert!(missing.contains("Unknown"), "{missing}");

    loader.files.insert(
        "external.bzl".into(),
        LoadFileResult {
            file_id: FileId(1),
            dialect: Dialect::Bazel,
            info: None,
            contents: Some("value = 42\n".into()),
        },
    );
    let mut change = Change::default();
    change.invalidate_loads();
    analysis.apply_change(change);
    let fetched = hover_value(&analysis, main);
    assert!(fetched.contains("Literal[42]"), "{fetched}");
}

#[test]
fn unrelated_edits_reuse_inference() {
    let (mut analysis, _) = Analysis::new_for_test();
    let mut fixture = Fixture::new(&mut analysis.db);
    let main = fixture.add_file(&mut analysis.db, "main.bzl", "value = 1 + 2\n");
    let other = fixture.add_file(&mut analysis.db, "other.bzl", "value = 0\n");
    let before = analysis.snapshot().diagnostics(main).unwrap();
    let mut change = Change::default();
    change.update_file(other, "value = 3\n".into());
    analysis.apply_change(change);
    analysis.db.executions.store(0, Ordering::Relaxed);
    assert_eq!(analysis.snapshot().diagnostics(main).unwrap(), before);
    assert_eq!(analysis.db.executions.load(Ordering::Relaxed), 0);
}

#[test]
fn diagnostics_do_not_depend_on_prior_requests() {
    let source = "def f():\n    value = 1\n    return value + \"bad\"\n";
    let (analysis, _) = Analysis::from_single_file_fixture(source);
    let diagnostics = analysis.snapshot().diagnostics(FileId(0)).unwrap();
    assert!(!diagnostics.is_empty());
    assert_eq!(
        analysis.snapshot().diagnostics(FileId(0)).unwrap(),
        diagnostics
    );

    let (other, _) = Analysis::from_single_file_fixture(source);
    hover_value(&other, FileId(0));
    assert_eq!(
        other.snapshot().diagnostics(FileId(0)).unwrap(),
        diagnostics
    );
}

#[test]
fn prelude_selection_is_a_query_input() {
    let (mut analysis, _) = Analysis::new_for_test();
    let mut fixture = Fixture::new(&mut analysis.db);
    let main = fixture.add_file_with_options(
        &mut analysis.db,
        "BUILD",
        "result = value\n",
        Dialect::Bazel,
        Some(FileInfo::Bazel {
            api_context: APIContext::Build,
            is_external: false,
        }),
    );
    let first = fixture.add_prelude_file(&mut analysis.db, "value = 1\n");
    let initial = hover_value(&analysis, main);
    assert!(initial.contains("Literal[1]"), "{initial}");
    fixture.add_prelude_file(&mut analysis.db, "value = \"next\"\n");
    let next = hover_value(&analysis, main);
    assert!(next.contains("next"), "{next}");
    analysis.set_bazel_prelude_file(first);
    assert_eq!(hover_value(&analysis, main), initial);
}

#[test]
fn builtin_changes_update_existing_query_dependencies() {
    let (mut analysis, _) = Analysis::from_single_file_fixture("value = attr\n");
    let original = analysis.db.get_builtin_defs(&Dialect::Bazel);
    let builtins = original.builtins(&analysis.db).clone();
    let original_hover = hover_value(&analysis, FileId(0));
    analysis.set_builtin_defs(Builtins::default(), Builtins::default());
    let empty_hover = hover_value(&analysis, FileId(0));
    assert_ne!(empty_hover, original_hover);
    analysis.set_builtin_defs(builtins, Builtins::default());
    assert_eq!(hover_value(&analysis, FileId(0)), original_hover);
}

#[test]
fn importing_a_function_does_not_check_unrelated_exports() {
    let (mut analysis, loader) = Analysis::new_for_test();
    let mut fixture = Fixture::new(&mut analysis.db);
    let main = fixture.add_file(
        &mut analysis.db,
        "main.bzl",
        "load(\"dep.bzl\", \"value\")\nresult = value\n",
    );
    fixture.add_file(
        &mut analysis.db,
        "dep.bzl",
        "load(\"unused.bzl\", \"unused\")\ndef value():\n    return unused\n",
    );
    loader.add_files_from_fixture(&analysis.db, &fixture);
    let hover = hover_value(&analysis, main);
    assert!(hover.contains("def value()"), "{hover}");
    assert_eq!(*loader.requests.lock().unwrap(), ["dep.bzl"]);
}

#[test]
fn cyclic_load_diagnostics_are_independent_of_query_order() {
    let (mut analysis, loader) = Analysis::new_for_test();
    let mut fixture = Fixture::new(&mut analysis.db);
    let first = fixture.add_file(
        &mut analysis.db,
        "first.bzl",
        "load(\"second.bzl\", \"value\")\nvalue = value\n",
    );
    let second = fixture.add_file(
        &mut analysis.db,
        "second.bzl",
        "load(\"first.bzl\", \"value\")\nvalue = value\n",
    );
    let own = fixture.add_file(
        &mut analysis.db,
        "own.bzl",
        "load(\"own.bzl\", \"value\")\nvalue = value\n",
    );
    loader.add_files_from_fixture(&analysis.db, &fixture);
    for (file, expected) in [
        (first, "Detected circular import"),
        (second, "Detected circular import"),
        (own, "Cannot load the current file"),
    ] {
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(
            diagnostics.iter().any(|d| d.message.starts_with(expected)),
            "{diagnostics:?}"
        );
        hover_value(&analysis, file);
        assert_eq!(analysis.snapshot().diagnostics(file).unwrap(), diagnostics);
    }
}

#[test]
fn pending_edits_cancel_inference_snapshots() {
    let (mut analysis, _) = Analysis::from_single_file_fixture("value = 1 + 2\n");
    let snapshot = analysis.snapshot();
    assert!(snapshot.diagnostics(FileId(0)).is_ok());
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let mut change = Change::default();
            change.update_file(FileId(0), "value = 3\n".into());
            analysis.apply_change(change);
        });
        // The writer waits for this snapshot. Even a cached query must observe
        // cancellation and release it; no timing assumptions are needed.
        while snapshot.diagnostics(FileId(0)).is_ok() {}
        drop(snapshot);
        writer.join().unwrap();
    });
    assert!(analysis.snapshot().diagnostics(FileId(0)).is_ok());
}
