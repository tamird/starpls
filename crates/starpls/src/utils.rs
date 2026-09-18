use std::ops::Range;

use ruff_source_file::LineIndex;
use starpls_common::File;
use starpls_ide::LocationLink;

use crate::convert;
use crate::server::ServerSnapshot;

fn text_range(text: &str, index: &LineIndex, range: lsp_types::Range) -> Option<Range<usize>> {
    let start = convert::offset_from_lsp_position(text, index, range.start)?;
    let end = convert::offset_from_lsp_position(text, index, range.end)?;
    (start <= end).then_some(usize::from(start)..usize::from(end))
}

pub(crate) fn apply_document_content_changes(
    mut contents: String,
    content_changes: Vec<lsp_types::TextDocumentContentChangeEvent>,
) -> String {
    for change in content_changes {
        match change.range {
            Some(range) => {
                let index = LineIndex::from_source_text(&contents);
                let range = text_range(&contents, &index, range);
                if let Some(range) = range {
                    contents.replace_range(range, &change.text);
                }
            }
            None => contents = change.text,
        }
    }
    contents
}

pub(crate) fn response_from_locations<T, U>(
    snapshot: &ServerSnapshot,
    source_file_id: File,
    locations: T,
) -> U
where
    T: Iterator<Item = LocationLink>,
    U: From<Vec<lsp_types::Location>> + From<Vec<lsp_types::LocationLink>>,
{
    let source = match snapshot.analysis_snapshot.source(source_file_id) {
        Ok(source) => source,
        _ => return Vec::<lsp_types::Location>::new().into(),
    };

    let to_lsp_location = |location: LocationLink| -> Option<lsp_types::Location> {
        let location = match location {
            LocationLink::Local {
                target_range,
                target_file_id,
                ..
            } => {
                let target = snapshot.analysis_snapshot.source(target_file_id).ok()?;
                let range = convert::lsp_range_from_text_range(target_range, &target);
                lsp_types::Location {
                    uri: lsp_types::Url::from_file_path(
                        snapshot.analysis_snapshot.path(target_file_id),
                    )
                    .ok()?,
                    range: range?,
                }
            }
            LocationLink::External { target_path, .. } => lsp_types::Location {
                uri: lsp_types::Url::from_file_path(target_path).ok()?,
                range: Default::default(),
            },
        };

        Some(location)
    };

    let to_lsp_location_link = |location: LocationLink| -> Option<lsp_types::LocationLink> {
        let location_link = match location {
            LocationLink::Local {
                origin_selection_range,
                target_range,
                target_file_id,
                ..
            } => {
                let target = snapshot.analysis_snapshot.source(target_file_id).ok()?;
                let range = convert::lsp_range_from_text_range(target_range, &target);
                lsp_types::LocationLink {
                    origin_selection_range: origin_selection_range
                        .and_then(|range| convert::lsp_range_from_text_range(range, &source)),
                    target_range: range?,
                    target_selection_range: range?,
                    target_uri: lsp_types::Url::from_file_path(
                        snapshot.analysis_snapshot.path(target_file_id),
                    )
                    .ok()?,
                }
            }
            LocationLink::External {
                origin_selection_range,
                target_path,
            } => lsp_types::LocationLink {
                origin_selection_range: origin_selection_range
                    .and_then(|range| convert::lsp_range_from_text_range(range, &source)),
                target_range: Default::default(),
                target_selection_range: Default::default(),
                target_uri: lsp_types::Url::from_file_path(target_path).ok()?,
            },
        };

        Some(location_link)
    };

    if snapshot.config.has_text_document_definition_link_support() {
        locations
            .flat_map(to_lsp_location_link)
            .collect::<Vec<_>>()
            .into()
    } else {
        locations
            .flat_map(to_lsp_location)
            .collect::<Vec<_>>()
            .into()
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::Position;
    use lsp_types::Range;
    use lsp_types::TextDocumentContentChangeEvent;

    use super::apply_document_content_changes;

    fn edit(start: (u32, u32), end: (u32, u32), text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range::new(
                Position::new(start.0, start.1),
                Position::new(end.0, end.1),
            )),
            range_length: None,
            text: text.to_owned(),
        }
    }

    #[test]
    fn edits_use_each_preceding_revision() {
        let contents = apply_document_content_changes(
            "a😀\r\nb\n".to_owned(),
            vec![edit((0, 1), (0, 3), "xy\n"), edit((2, 0), (2, 1), "β")],
        );
        assert_eq!(contents, "axy\n\r\nβ\n");
        let contents = apply_document_content_changes(
            contents,
            vec![
                TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "😀".to_owned(),
                },
                edit((0, 0), (0, 2), "done"),
            ],
        );
        assert_eq!(contents, "done");
    }

    #[test]
    fn invalid_edits_preserve_contents() {
        for change in [
            edit((0, 1), (0, 2), "x"),
            edit((0, 2), (0, 0), "x"),
            edit((1, 0), (1, 0), "x"),
        ] {
            assert_eq!(
                apply_document_content_changes("😀".to_owned(), vec![change]),
                "😀"
            );
        }
    }
}
