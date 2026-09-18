#[cfg(test)]
mod tests {
    use starpls_common::Dialect;

    use crate::Analysis;

    #[test]
    fn source_and_index_follow_file_revisions() {
        let (mut analysis, _) = Analysis::new_for_test();
        let file = analysis
            .open_document(
                std::path::Path::new("main.star"),
                Dialect::Standard,
                None,
                String::new(),
                0,
            )
            .unwrap();
        for (text, line_count) in [("a\n", 2), ("😀\r\nb\nc", 3), ("a\n", 2)] {
            analysis.update_file(file, text.to_owned());
            let snapshot = analysis.snapshot();
            let source = snapshot.source(file).unwrap();
            assert_eq!(source.text.as_str(), text);
            assert_eq!(source.index.line_count(), line_count);
            let end = u32::try_from(text.len()).unwrap();
            assert_eq!(
                source.index.line_index(end.into()).to_zero_indexed(),
                line_count - 1
            );
        }
    }
}
