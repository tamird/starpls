use std::path::PathBuf;

use anyhow::anyhow;
use ruff_source_file::LineIndex;
use ruff_source_file::OneIndexed;
use ruff_source_file::PositionEncoding;
use ruff_source_file::SourceLocation;
use starpls_common::Diagnostic;
use starpls_common::DiagnosticTag;
use starpls_common::File;
use starpls_common::Severity;
use starpls_common::Source;
use starpls_ide::DocumentSymbol;
use starpls_ide::SymbolKind;
use starpls_ide::SymbolTag;
use starpls_syntax::TextRange;
use starpls_syntax::TextSize;

use crate::server::ServerSnapshot;

pub(crate) fn path_buf_from_url(url: &lsp_types::Url) -> anyhow::Result<PathBuf> {
    url.to_file_path()
        .map_err(|_| anyhow!("url is not a file: {}", url))
}

pub(crate) fn lsp_diagnostic_from_native(
    diagnostic: Diagnostic,
    source: &Source,
) -> Option<lsp_types::Diagnostic> {
    let range = diagnostic.range()?;
    let range = TextRange::new(
        u32::from(range.start()).into(),
        u32::from(range.end()).into(),
    );
    Some(lsp_types::Diagnostic {
        range: lsp_range_from_text_range(range, source)?,
        severity: Some(lsp_severity_from_native(diagnostic.severity())),
        code: None,
        code_description: None,
        source: Some("starpls".to_string()),
        message: diagnostic.headline_message().to_owned(),
        related_information: None,
        tags: diagnostic
            .primary_tags()
            .filter(|tags| !tags.is_empty())
            .map(|tags| {
                tags.iter()
                    .map(|tag| match tag {
                        DiagnosticTag::Unnecessary => lsp_types::DiagnosticTag::UNNECESSARY,
                        DiagnosticTag::Deprecated => lsp_types::DiagnosticTag::DEPRECATED,
                    })
                    .collect()
            }),
        data: None,
    })
}

pub(crate) fn lsp_range_from_text_range(
    text_range: TextRange,
    source: &Source,
) -> Option<lsp_types::Range> {
    let start = lsp_position_from_offset(&source.text, &source.index, text_range.start())?;
    let end = lsp_position_from_offset(&source.text, &source.index, text_range.end())?;
    Some(lsp_types::Range { start, end })
}

pub(crate) fn text_range_from_lsp_range(
    range: lsp_types::Range,
    source: &Source,
) -> Option<TextRange> {
    let start = offset_from_lsp_position(&source.text, &source.index, range.start)?;
    let end = offset_from_lsp_position(&source.text, &source.index, range.end)?;
    (start <= end).then(|| TextRange::new(start, end))
}

pub(crate) fn lsp_semantic_tokens(
    tokens: &[starpls_ide::SemanticToken],
    source: &Source,
) -> Vec<lsp_types::SemanticToken> {
    let mut result = Vec::with_capacity(tokens.len());
    let mut previous = lsp_types::Position::default();
    for token in tokens {
        let range = TextRange::new(
            u32::from(token.range.start()).into(),
            u32::from(token.range.end()).into(),
        );
        let Some(range) = lsp_range_from_text_range(range, source) else {
            continue;
        };
        // Single-line tokens work with every client and exclude newline bytes.
        for line in range.start.line..=range.end.line {
            let start = lsp_types::Position::new(
                line,
                if line == range.start.line {
                    range.start.character
                } else {
                    0
                },
            );
            let end = if line == range.end.line {
                range.end.character
            } else {
                let line_range = source
                    .index
                    .line_range(OneIndexed::from_zero_indexed(line as usize), &source.text);
                let text = source.text[line_range].trim_end_matches(['\r', '\n']);
                let Ok(length) = u32::try_from(text.encode_utf16().count()) else {
                    continue;
                };
                length
            };
            if end <= start.character {
                continue;
            }
            let delta_line = start.line - previous.line;
            let delta_start = if delta_line == 0 {
                start.character - previous.character
            } else {
                start.character
            };
            result.push(lsp_types::SemanticToken {
                delta_line,
                delta_start,
                length: end - start.character,
                token_type: token.token_type as u32,
                token_modifiers_bitset: token.modifiers.bits(),
            });
            previous = start;
        }
    }
    result
}

fn lsp_position_from_offset(
    text: &str,
    index: &LineIndex,
    offset: TextSize,
) -> Option<lsp_types::Position> {
    if !text.is_char_boundary(usize::from(offset)) {
        return None;
    }
    let location = index.source_location(u32::from(offset).into(), text, PositionEncoding::Utf16);
    let line = u32::try_from(location.line.to_zero_indexed()).ok()?;
    let character = u32::try_from(location.character_offset.to_zero_indexed()).ok()?;
    Some(lsp_types::Position { line, character })
}

pub(crate) fn offset_from_lsp_position(
    text: &str,
    index: &LineIndex,
    pos: lsp_types::Position,
) -> Option<TextSize> {
    if pos.line as usize >= index.line_count() {
        return None;
    }
    let location = SourceLocation {
        line: OneIndexed::from_zero_indexed(pos.line as usize),
        character_offset: OneIndexed::from_zero_indexed(pos.character as usize),
    };
    let range = index.line_range(location.line, text);
    let line = text[range].trim_end_matches(['\r', '\n']);
    let length = u32::try_from(line.len()).ok()?;
    let end = (u32::from(range.start()) + length).into();
    // LSP clamps columns beyond the text of a line, excluding its newline.
    let offset = index
        .offset(location, text, PositionEncoding::Utf16)
        .min(end);
    let actual = index.source_location(offset, text, PositionEncoding::Utf16);
    // Ruff rounds positions inside a surrogate pair forward. Such a position
    // cannot identify a byte boundary for an editor operation.
    if actual.character_offset > location.character_offset {
        return None;
    }
    Some(u32::from(offset).into())
}

pub(crate) fn text_size_from_lsp_position(
    snapshot: &ServerSnapshot,
    file_id: File,
    pos: lsp_types::Position,
) -> anyhow::Result<Option<TextSize>> {
    let source = snapshot.analysis_snapshot.source(file_id)?;
    Ok(offset_from_lsp_position(&source.text, &source.index, pos))
}

fn lsp_severity_from_native(severity: Severity) -> lsp_types::DiagnosticSeverity {
    match severity {
        Severity::Error => lsp_types::DiagnosticSeverity::ERROR,
        Severity::Fatal => lsp_types::DiagnosticSeverity::ERROR,
        Severity::Warning => lsp_types::DiagnosticSeverity::WARNING,
        Severity::Info => lsp_types::DiagnosticSeverity::INFORMATION,
    }
}

#[allow(deprecated)]
pub(crate) fn lsp_document_symbol_from_native(
    DocumentSymbol {
        name,
        detail,
        kind,
        tags,
        range,
        selection_range,
        children,
    }: DocumentSymbol,
    source: &Source,
) -> Option<lsp_types::DocumentSymbol> {
    Some(lsp_types::DocumentSymbol {
        name,
        detail,
        kind: match kind {
            SymbolKind::File => lsp_types::SymbolKind::FILE,
            SymbolKind::Module => lsp_types::SymbolKind::MODULE,
            SymbolKind::Namespace => lsp_types::SymbolKind::NAMESPACE,
            SymbolKind::Package => lsp_types::SymbolKind::PACKAGE,
            SymbolKind::Class => lsp_types::SymbolKind::CLASS,
            SymbolKind::Method => lsp_types::SymbolKind::METHOD,
            SymbolKind::Property => lsp_types::SymbolKind::PROPERTY,
            SymbolKind::Field => lsp_types::SymbolKind::FIELD,
            SymbolKind::Constructor => lsp_types::SymbolKind::CONSTRUCTOR,
            SymbolKind::Enum => lsp_types::SymbolKind::ENUM,
            SymbolKind::Interface => lsp_types::SymbolKind::INTERFACE,
            SymbolKind::Function => lsp_types::SymbolKind::FUNCTION,
            SymbolKind::Variable => lsp_types::SymbolKind::VARIABLE,
            SymbolKind::Constant => lsp_types::SymbolKind::CONSTANT,
            SymbolKind::String => lsp_types::SymbolKind::STRING,
            SymbolKind::Number => lsp_types::SymbolKind::NUMBER,
            SymbolKind::Boolean => lsp_types::SymbolKind::BOOLEAN,
            SymbolKind::Array => lsp_types::SymbolKind::ARRAY,
            SymbolKind::Object => lsp_types::SymbolKind::OBJECT,
            SymbolKind::Key => lsp_types::SymbolKind::KEY,
            SymbolKind::Null => lsp_types::SymbolKind::NULL,
            SymbolKind::EnumMember => lsp_types::SymbolKind::ENUM_MEMBER,
            SymbolKind::Struct => lsp_types::SymbolKind::STRUCT,
            SymbolKind::Event => lsp_types::SymbolKind::EVENT,
            SymbolKind::Operator => lsp_types::SymbolKind::OPERATOR,
            SymbolKind::TypeParameter => lsp_types::SymbolKind::TYPE_PARAMETER,
        },
        tags: tags.map(|tags| {
            tags.into_iter()
                .map(|tag| match tag {
                    SymbolTag::Deprecated => lsp_types::SymbolTag::DEPRECATED,
                })
                .collect()
        }),
        range: lsp_range_from_text_range(range, source)?,
        selection_range: lsp_range_from_text_range(selection_range, source)?,
        children: children.map(|children| {
            children
                .into_iter()
                .filter_map(|child| lsp_document_symbol_from_native(child, source))
                .collect()
        }),
        deprecated: None,
    })
}

#[cfg(test)]
mod tests {
    use lsp_types::Position;
    use ruff_source_file::LineIndex;

    use super::lsp_position_from_offset;
    use super::offset_from_lsp_position;

    fn source(text: &str) -> (starpls_common::Source, starpls_common::File) {
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = crate::document::DefaultFileLoader::new(
            std::sync::Arc::new(starpls_bazel::client::BazelCLI::new("bazel")),
            Default::default(),
            None,
            Default::default(),
            sender,
            false,
        );
        let mut analysis = starpls_ide::Analysis::with_system(
            std::sync::Arc::new(loader),
            Default::default(),
            ruff_db::system::InMemorySystem::default(),
        );
        let file = analysis
            .open_document(
                std::path::Path::new("main.star"),
                starpls_common::Dialect::Standard,
                None,
                text.into(),
                0,
            )
            .unwrap();
        (analysis.snapshot().source(file).unwrap(), file)
    }

    #[test]
    fn diagnostic_conversion_preserves_protocol_fields() {
        let (source, file) = source("😀x\n");
        for (severity, tag, severity_number, tag_number) in [
            (starpls_common::Severity::Error, None, 1, None),
            (
                starpls_common::Severity::Warning,
                Some(starpls_common::DiagnosticTag::Unnecessary),
                2,
                Some(1),
            ),
            (
                starpls_common::Severity::Info,
                Some(starpls_common::DiagnosticTag::Deprecated),
                3,
                Some(2),
            ),
        ] {
            let diagnostic = starpls_common::diagnostic(
                file,
                starpls_common::DiagnosticId::lint("type-check"),
                severity,
                starpls_syntax::TextRange::new(4.into(), 5.into()),
                "message",
                tag,
            );
            let converted = super::lsp_diagnostic_from_native(diagnostic, &source).unwrap();
            let mut expected = serde_json::json!({
                "range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 3}},
                "severity": severity_number,
                "source": "starpls",
                "message": "message",
            });
            if let Some(tag) = tag_number {
                expected["tags"] = serde_json::json!([tag]);
            }
            assert_eq!(serde_json::to_value(converted).unwrap(), expected);
        }
    }

    #[test]
    fn semantic_tokens_split_multiline_strings_and_encode_utf16_deltas() {
        let text = "x = \"\"\"😀\r\nβ\r\n\"\"\"; y\n";
        let (source, _) = source(text);
        let tokens = [
            starpls_ide::SemanticToken {
                range: (4.into()..20.into()).into(),
                token_type: starpls_ide::SemanticTokenType::String,
                modifiers: starpls_ide::SemanticTokenModifier::empty(),
            },
            starpls_ide::SemanticToken {
                range: (22.into()..23.into()).into(),
                token_type: starpls_ide::SemanticTokenType::Variable,
                modifiers: starpls_ide::SemanticTokenModifier::READONLY,
            },
        ];
        let actual = super::lsp_semantic_tokens(&tokens, &source);
        let positions: Vec<_> = actual
            .iter()
            .map(|token| (token.delta_line, token.delta_start, token.length))
            .collect();
        assert_eq!(positions, [(0, 4, 5), (1, 0, 1), (1, 0, 3), (0, 5, 1)]);
        assert_eq!(
            actual[3].token_type,
            starpls_ide::SemanticTokenType::Variable as u32
        );
        assert_eq!(
            actual[3].token_modifiers_bitset,
            starpls_ide::SemanticTokenModifier::READONLY.bits()
        );
    }

    #[test]
    fn utf16_positions_preserve_bom_and_crlf() {
        let text = "\u{feff}a😀\r\nβ\n";
        let index = LineIndex::from_source_text(text);
        for (offset, line, character) in [
            (0, 0, 0),
            (3, 0, 1),
            (4, 0, 2),
            (8, 0, 4),
            (10, 1, 0),
            (12, 1, 1),
            (13, 2, 0),
        ] {
            let position = Position::new(line, character);
            assert_eq!(
                lsp_position_from_offset(text, &index, offset.into()),
                Some(position)
            );
            assert_eq!(
                offset_from_lsp_position(text, &index, position),
                Some(offset.into())
            );
        }
        assert_eq!(
            offset_from_lsp_position(text, &index, Position::new(0, 3)),
            None
        );
        assert_eq!(
            offset_from_lsp_position(text, &index, Position::new(3, 0)),
            None
        );
        assert_eq!(lsp_position_from_offset(text, &index, 2.into()), None);
        assert_eq!(lsp_position_from_offset(text, &index, 14.into()), None);
        assert_eq!(
            offset_from_lsp_position(text, &index, Position::new(0, 99)),
            Some(8.into())
        );
        assert_eq!(
            offset_from_lsp_position(text, &index, Position::new(1, 99)),
            Some(12.into())
        );
    }
    #[test]
    fn line_endings_use_the_parser_convention() {
        for text in ["a\nb", "a\r\nb", "a\rb"] {
            let index = LineIndex::from_source_text(text);
            let offset = u32::try_from(text.find('b').unwrap()).unwrap();
            let position = Position::new(1, 0);
            assert_eq!(
                lsp_position_from_offset(text, &index, offset.into()),
                Some(position)
            );
            assert_eq!(
                offset_from_lsp_position(text, &index, position),
                Some(offset.into())
            );
            assert_eq!(
                offset_from_lsp_position(text, &index, Position::new(0, 99)),
                Some(1.into())
            );
        }
    }
}
