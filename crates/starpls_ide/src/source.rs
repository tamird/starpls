use starpls_common::Db as _;
use starpls_common::FileId;
use starpls_common::Source;

use crate::Database;

pub(crate) fn source(db: &Database, file_id: FileId) -> Option<Source<'_>> {
    let file = db.get_file(file_id)?;
    Some(starpls_common::source(db, file))
}

#[cfg(test)]
mod tests {
    use starpls_common::Dialect;
    use starpls_common::FileId;

    use crate::Analysis;
    use crate::Change;

    #[test]
    fn source_and_index_follow_file_revisions() {
        let (mut analysis, _) = Analysis::new_for_test();
        let file_id = FileId(0);
        let mut change = Change::default();
        change.create_file(file_id, Dialect::Standard, None, String::new());
        analysis.apply_change(change);
        for (text, line_count) in [("a\n", 2), ("😀\r\nb\nc", 3), ("a\n", 2)] {
            let mut change = Change::default();
            change.update_file(file_id, text.to_owned());
            analysis.apply_change(change);
            let snapshot = analysis.snapshot();
            let source = snapshot.source(file_id).unwrap().unwrap();
            assert_eq!(source.text, text);
            assert_eq!(source.index.line_count(), line_count);
            let end = u32::try_from(text.len()).unwrap();
            assert_eq!(
                source.index.line_index(end.into()).to_zero_indexed(),
                line_count - 1
            );
        }
    }
}
