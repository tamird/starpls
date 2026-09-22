use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use ruff_db::system::InMemorySystem;
use ruff_db::system::SystemPath;
use ruff_db::system::WritableSystem;
use starpls_bazel::APIContext;
use starpls_bazel::Builtins;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::FileInfo;
use starpls_hir::Db as _;
use starpls_hir::Fixture;

use crate::Analysis;
use crate::FilePosition;
use crate::LocationLink;

fn hover_value(analysis: &Analysis, file_id: File) -> String {
    let file = file_id;
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
    let disk = InMemorySystem::default();
    disk.write_file(SystemPath::new("/dep.bzl"), "value = 1\n")
        .unwrap();
    let loader = Arc::new(crate::SimpleFileLoader::default());
    let mut analysis = Analysis::with_system(loader, Default::default(), disk);
    let mut fixture = Fixture::new(&mut analysis.db);
    let main = fixture.add_file(
        &mut analysis.db,
        "main.bzl",
        "load(\"/dep.bzl\", \"value\")\nresult = value\n",
    );
    let original = hover_value(&analysis, main);
    assert!(original.contains("Literal[1]"), "{original}");
    let dependency = analysis
        .file(Path::new("/dep.bzl"), Dialect::Bazel, None)
        .unwrap();
    let opened = analysis
        .open_document(
            Path::new("/dep.bzl"),
            Dialect::Bazel,
            None,
            "value = \"edited\"\n".into(),
            1,
        )
        .unwrap();
    assert_eq!(opened.source, dependency.source);
    let edited = hover_value(&analysis, main);
    assert!(edited.contains("edited"), "{edited}");
    analysis.close_document(Path::new("/dep.bzl")).unwrap();
    assert_eq!(hover_value(&analysis, main), original);
}

#[test]
fn imported_function_views_follow_reparsed_definitions() {
    let (mut analysis, loader) = Analysis::new_for_test();
    let mut fixture = Fixture::new(&mut analysis.db);
    let source = "load(\"dep.bzl\", \"value\")\nvalue(1)\n";
    let main = fixture.add_file(&mut analysis.db, "main.bzl", source);
    let original = "def value(first):\n    \"\"\"Original docs.\"\"\"\n    pass\n";
    let changed = "\n\ndef value(second, first = 0):\n    \"\"\"Changed docs.\"\"\"\n    pass\n";
    let dependency = fixture.add_file(&mut analysis.db, "dep.bzl", original);
    loader.add_files_from_fixture(&fixture);

    for (contents, labels, doc) in [
        (original, vec!["first"], "Original docs."),
        (changed, vec!["second", "first=0"], "Changed docs."),
        (original, vec!["first"], "Original docs."),
    ] {
        analysis.update_file(dependency, contents.into());

        let snapshot = analysis.snapshot();
        let help = snapshot
            .signature_help(FilePosition {
                file_id: main,
                pos: (source.rfind('1').unwrap() as u32).into(),
            })
            .unwrap()
            .unwrap();
        let [signature] = help.signatures.as_slice() else {
            panic!("expected one signature: {help:?}");
        };
        assert_eq!(signature.documentation.as_deref().map(str::trim), Some(doc));
        assert_eq!(
            signature
                .parameters
                .as_ref()
                .unwrap()
                .iter()
                .map(|param| param.label.as_str())
                .collect::<Vec<_>>(),
            labels,
        );
        let definitions = snapshot
            .goto_definition(
                FilePosition {
                    file_id: main,
                    pos: (source.rfind("value").unwrap() as u32).into(),
                },
                true,
            )
            .unwrap()
            .unwrap();
        let [LocationLink::Local {
            origin_selection_range: _,
            target_range: _,
            target_selection_range,
            target_file_id,
        }] = definitions.as_slice()
        else {
            panic!("expected one local definition: {definitions:?}");
        };
        assert_eq!(*target_file_id, dependency.source);
        assert_eq!(
            u32::from(target_selection_range.start()),
            contents.find("value").unwrap() as u32,
        );
    }
}

#[test]
fn accumulated_diagnostics_follow_edits() {
    for invalid in ["value = (\n", "1 = 2\n"] {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(invalid);
        let original = analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap();
        assert!(!original.is_empty());
        assert_eq!(
            analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap(),
            original
        );

        analysis.update_file(fixture.main_file(), "value = 1\n".into());

        assert!(analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap()
            .is_empty());

        analysis.update_file(fixture.main_file(), invalid.into());

        assert_eq!(
            analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap(),
            original
        );
    }
}

#[test]
fn fetched_modules_invalidate_missing_loads() {
    let disk = InMemorySystem::default();
    let loader = Arc::new(crate::SimpleFileLoader::default());
    let mut analysis = Analysis::with_system(loader, Default::default(), disk.clone());
    let mut fixture = Fixture::new(&mut analysis.db);
    let main = fixture.add_file(
        &mut analysis.db,
        "main.bzl",
        "load(\"/external.bzl\", \"value\")\nresult = value\n",
    );
    let missing = hover_value(&analysis, main);
    assert!(missing.contains("Unknown"), "{missing}");
    disk.write_file(SystemPath::new("/external.bzl"), "value = 42\n")
        .unwrap();
    // A file already cached as missing must be synchronized after fetching.
    assert_eq!(hover_value(&analysis, main), missing);
    analysis.invalidate_loads();
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

    analysis.update_file(other, "value = 3\n".into());

    analysis.db.executions.store(0, Ordering::Relaxed);
    assert_eq!(analysis.snapshot().diagnostics(main).unwrap(), before);
    assert_eq!(analysis.db.executions.load(Ordering::Relaxed), 0);
}

#[test]
fn diagnostics_do_not_depend_on_prior_requests() {
    let source = "def f():\n    value = 1\n    return value + \"bad\"\n";
    let (analysis, fixture) = Analysis::from_single_file_fixture(source);
    let diagnostics = analysis
        .snapshot()
        .diagnostics(fixture.main_file())
        .unwrap();
    assert!(!diagnostics.is_empty());
    assert_eq!(
        analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap(),
        diagnostics
    );

    let (other, other_fixture) = Analysis::from_single_file_fixture(source);
    hover_value(&other, other_fixture.main_file());
    assert_eq!(
        other
            .snapshot()
            .diagnostics(other_fixture.main_file())
            .unwrap(),
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
    let second = fixture.add_file_with_options(
        &mut analysis.db,
        "other_prelude",
        "value = \"next\"\n",
        Dialect::Bazel,
        first.info,
    );
    analysis.set_bazel_prelude_file(second);
    let next = hover_value(&analysis, main);
    assert!(next.contains("next"), "{next}");
    analysis.set_bazel_prelude_file(first);
    assert_eq!(hover_value(&analysis, main), initial);
}

#[test]
fn builtin_changes_update_existing_query_dependencies() {
    let (mut analysis, fixture) = Analysis::from_single_file_fixture("value = attr\n");
    let original = analysis.db.get_builtin_defs(&Dialect::Bazel);
    let builtins = original.builtins(&analysis.db).clone();
    let original_hover = hover_value(&analysis, fixture.main_file());
    analysis
        .set_builtin_defs(Builtins::default(), Default::default())
        .unwrap();
    let empty_hover = hover_value(&analysis, fixture.main_file());
    assert_ne!(empty_hover, original_hover);
    analysis
        .set_builtin_defs(builtins, Default::default())
        .unwrap();
    assert_eq!(hover_value(&analysis, fixture.main_file()), original_hover);
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
    loader.add_files_from_fixture(&fixture);
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
    loader.add_files_from_fixture(&fixture);
    for (file, expected) in [
        (first, "Detected circular import"),
        (second, "Detected circular import"),
        (own, "Cannot load the current file"),
    ] {
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.headline_message().starts_with(expected)),
            "{diagnostics:?}"
        );
        hover_value(&analysis, file);
        assert_eq!(analysis.snapshot().diagnostics(file).unwrap(), diagnostics);
    }
}

#[test]
fn pending_edits_cancel_inference_snapshots() {
    let (mut analysis, fixture) = Analysis::from_single_file_fixture("value = 1 + 2\n");
    let snapshot = analysis.snapshot();
    let old_source = snapshot.source(fixture.main_file()).unwrap();
    let old_stamp = snapshot.document(Path::new("main.bzl")).unwrap().stamp();
    assert!(snapshot.diagnostics(fixture.main_file()).is_ok());
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            analysis.update_file(fixture.main_file(), "value = 3\n".into());
        });
        // The writer waits for this snapshot. Even a cached query must observe
        // cancellation and release it; no timing assumptions are needed.
        while snapshot.diagnostics(fixture.main_file()).is_ok() {}
        assert!(snapshot.open_file(Path::new("main.bzl")).is_err());
        drop(snapshot);
        writer.join().unwrap();
    });
    assert!(analysis.snapshot().diagnostics(fixture.main_file()).is_ok());
    assert_eq!(old_source.text.as_str(), "value = 1 + 2\n");
    assert_eq!(old_source.index.line_count(), 2);
    assert_ne!(
        old_stamp,
        analysis.document(Path::new("main.bzl")).unwrap().stamp()
    );
}

#[test]
fn physical_sources_support_distinct_host_contexts() {
    let (mut analysis, _) = Analysis::new_for_test();
    let text = "load(\"query.bzl\", \"value\")\nvalue = 1\n";
    let query = analysis
        .open_document(
            Path::new("query.bzl"),
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Cquery,
                is_external: false,
            }),
            text.into(),
            1,
        )
        .unwrap();
    let module = analysis
        .file(
            Path::new("query.bzl"),
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Bzl,
                is_external: false,
            }),
        )
        .unwrap();
    assert_eq!(query.source, module.source);
    assert_ne!(query, module);
    let diagnostics = analysis.snapshot().diagnostics(query).unwrap();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.headline_message() == "Cannot load the current file"),
        "{diagnostics:?}"
    );
}

#[test]
fn invalid_utf8_loads_report_read_errors() {
    let disk = InMemorySystem::default();
    disk.write_file_bytes(SystemPath::new("/bad.bzl"), &[0xff])
        .unwrap();
    let loader = Arc::new(crate::SimpleFileLoader::default());
    let mut analysis = Analysis::with_system(loader, Default::default(), disk);
    let main = analysis
        .open_document(
            Path::new("/main.bzl"),
            Dialect::Bazel,
            None,
            "load(\"/bad.bzl\", \"value\")\n".into(),
            1,
        )
        .unwrap();
    let diagnostics = analysis.snapshot().diagnostics(main).unwrap();
    assert!(
        diagnostics
            .iter()
            .any(|d| d.headline_message().contains("cannot read /bad.bzl")),
        "{diagnostics:?}"
    );
    assert!(analysis
        .file(Path::new("/bad.bzl"), Dialect::Bazel, None)
        .is_err());
}

#[test]
fn arbitrary_extensions_remain_plain_text() {
    let (mut analysis, _) = Analysis::new_for_test();
    let file = analysis
        .open_document(
            Path::new("main.ipynb"),
            Dialect::Standard,
            None,
            "value = 1\n".into(),
            1,
        )
        .unwrap();
    let source = analysis.snapshot().source(file).unwrap();
    assert_eq!(source.text.as_str(), "value = 1\n");
    assert!(source.text.read_error().is_none());
    assert!(!source.text.is_notebook());
}
