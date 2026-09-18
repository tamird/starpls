use starpls_common::Diagnostic;
use starpls_common::File;
use starpls_hir::diagnostics_for_file;

use crate::Database;

pub(crate) fn diagnostics(db: &Database, file_id: File) -> Vec<Diagnostic> {
    let file = file_id;

    let diagnostics = starpls_hir::inference_diagnostics(db, file);

    // Limit the amount of syntax errors we send, as this many syntax errors probably means something
    // is really wrong with the file being analyzed.
    diagnostics_for_file(db, file)
        .take(128)
        .chain(diagnostics)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use starpls_hir::Fixture;

    use crate::Analysis;
    use crate::InferenceOptions;
    use crate::SimpleFileLoader;

    #[test]
    fn flow_reachability_is_invalidated_after_edits() {
        let options = InferenceOptions {
            use_code_flow_analysis: true,
            ..Default::default()
        };
        let mut analysis = Analysis::with_system(
            Arc::new(SimpleFileLoader::default()),
            options,
            ruff_db::system::InMemorySystem::default(),
        );
        let original = "fail()\nx = 1\n";
        let (fixture, _) = Fixture::from_single_file(&mut analysis.db, original);

        for (source, unreachable) in [
            (original, true),
            ("str()\nx = 1\n", false),
            (original, true),
        ] {
            analysis.update_file(fixture.main_file(), source.to_owned());

            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            assert_eq!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.headline_message() == "Code is unreachable"),
                unreachable,
                "{source}: {diagnostics:?}",
            );
        }
    }

    #[test]
    fn incomplete_headers_do_not_capture_later_statements() {
        for source in [
            "def broken()\nx = {\"k\": 1}\ny = 2\n",
            "if x\n    pass\ny = {\"a\": 1}\n",
            "if x:\nif y:\n    pass\nz = 1\n",
        ] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(source);
            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            assert!(!diagnostics.is_empty(), "{source}");
        }
    }
    #[test]
    fn type_ignore_only_suppresses_inference() {
        for input in ["\"x\" + 1", "fail(msg = \"x\")"] {
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(input);
            let file = fixture.main_file();
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(!diagnostics.is_empty(), "{input}");
            assert!(diagnostics
                .iter()
                .all(|diagnostic| diagnostic.id().is_lint()));
            analysis.update_file(file, format!("{input} # type: ignore\n"));
            assert!(analysis.snapshot().diagnostics(file).unwrap().is_empty());
        }
        let (analysis, fixture) = Analysis::from_single_file_fixture("1 = 2 # type: ignore\n");
        let diagnostics = analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.is_invalid_syntax()));

        let (analysis, fixture) =
            Analysis::from_single_file_fixture("\"x\" + (\n  1 # type: ignore\n)\n");
        assert!(analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn syntax_limit_does_not_limit_inference() {
        let input = "if True: pass\n".repeat(129) + "unknown\n";
        let (analysis, fixture) = Analysis::from_single_file_fixture(&input);
        let diagnostics = analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap();
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.is_invalid_syntax())
                .count(),
            128
        );
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.id().is_lint())
                .count(),
            1
        );
    }

    #[test]
    fn renderer_uses_snapshot_source() {
        let (analysis, fixture) = Analysis::from_single_file_fixture("value = 1 # type: string\n");
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(fixture.main_file()).unwrap();
        let rendered = snapshot
            .render_diagnostics(
                &diagnostics,
                &ruff_db::diagnostic::DisplayDiagnosticConfig::new("starpls"),
            )
            .unwrap();
        expect_test::expect![[r#"
            error[type-check]: Expression of type "Literal[1]" cannot be assigned to variable of type "string"
             --> main.bzl:1:9
              |
            1 | value = 1 # type: string
              |         ^

        "#]].assert_eq(&rendered);
    }
}
