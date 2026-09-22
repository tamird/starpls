#[cfg(test)]
mod tests {
    use starpls_common::Dialect;

    use crate::Analysis;
    use crate::FilePosition;

    #[test]
    fn structural_ranges_follow_loads_build_calls_and_edits() {
        let source = "load(\"//:defs.bzl\", \"rule\")\nrule(\n    name = \"😀app\",\n    srcs = [\n        \"main.cc\",\n    ],\n)\n";
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        let file = fixture.main_file();
        for suffix in ["", "# last line\n", ""] {
            let contents = format!("{suffix}{source}");
            analysis.update_file(file, contents.clone());
            let snapshot = analysis.snapshot();
            let ranges = snapshot
                .selection_ranges(FilePosition {
                    file_id: file,
                    pos: u32::try_from(contents.find("defs").unwrap())
                        .unwrap()
                        .into(),
                })
                .unwrap();
            assert_eq!(
                ranges
                    .last()
                    .map(|range| &contents[usize::from(range.start())..usize::from(range.end())]),
                Some("//:defs.bzl")
            );
            assert!(ranges
                .windows(2)
                .all(|pair| pair[0].contains_range(pair[1])));
            let folds = snapshot.folding_ranges(file).unwrap();
            let text: Vec<_> = folds.iter().map(|fold| &contents[fold.range]).collect();
            assert!(
                text.iter()
                    .any(|text| text.contains("name =") && text.contains("srcs =")),
                "{text:?}"
            );
            assert!(
                text.iter()
                    .any(|text| text.contains("main.cc") && !text.contains("srcs =")),
                "{text:?}"
            );
        }
    }

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
