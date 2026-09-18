use starpls_common::Db;
use starpls_common::Diagnostic;
use starpls_common::FileId;
use starpls_hir::diagnostics_for_file;

use crate::Database;

pub(crate) fn diagnostics(db: &Database, file_id: FileId) -> Vec<Diagnostic> {
    let file = match db.get_file(file_id) {
        Some(file) => file,
        None => return Vec::new(),
    };

    let diagnostics = db.gcx.with_tcx(db, |tcx| tcx.diagnostics_for_file(file));

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

    use starpls_common::FileId;
    use starpls_hir::Fixture;

    use crate::Analysis;
    use crate::Change;
    use crate::InferenceOptions;
    use crate::SimpleFileLoader;

    #[test]
    fn flow_reachability_is_invalidated_after_edits() {
        let options = InferenceOptions {
            use_code_flow_analysis: true,
            ..Default::default()
        };
        let mut analysis = Analysis::new(Arc::new(SimpleFileLoader::default()), options);
        let original = "fail()\nx = 1\n";
        Fixture::from_single_file(&mut analysis.db, original);

        for (source, unreachable) in [
            (original, true),
            ("str()\nx = 1\n", false),
            (original, true),
        ] {
            let mut change = Change::default();
            change.update_file(FileId(0), source.to_owned());
            analysis.apply_change(change);
            let diagnostics = analysis.snapshot().diagnostics(FileId(0)).unwrap();
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
            let (analysis, _) = Analysis::from_single_file_fixture(source);
            let diagnostics = analysis.snapshot().diagnostics(FileId(0)).unwrap();
            assert!(!diagnostics.is_empty(), "{source}");
        }
    }
}
