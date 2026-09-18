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
                    .any(|diagnostic| diagnostic.message == "Code is unreachable"),
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
}
