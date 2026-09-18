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
    use starpls_common::FileId;

    use crate::Analysis;

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
