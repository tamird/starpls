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

    use crate::Analysis;

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
