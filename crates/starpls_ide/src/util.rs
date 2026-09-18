use starpls_syntax::SyntaxKind;
use starpls_syntax::SyntaxToken;
use starpls_syntax::TokenAtOffset;

pub(crate) fn pick_best_token(
    tokens: TokenAtOffset<SyntaxToken>,
    mut f: impl FnMut(SyntaxKind) -> usize,
) -> Option<SyntaxToken> {
    tokens.max_by_key(|token| f(token.kind()))
}

// TODO(withered-magic): This logic should probably be more sophisticated, but it works well
// enough for now.
pub(crate) fn unindent_doc(doc: &str) -> String {
    let mut is_in_code_block = false;
    unindent::unindent(doc)
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let num_trimmed = line.len() - trimmed.len();
            let mut s = String::new();

            if trimmed.starts_with("```") {
                is_in_code_block = !is_in_code_block;
            }

            (0..num_trimmed)
                .for_each(|_| s.push_str(if is_in_code_block { " " } else { "&nbsp;" }));
            s.push_str(trimmed);
            s.push_str("  ");
            s
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Convert Ruff's byte range at the editor protocol boundary.
pub(crate) fn text_range(range: ruff_text_size::TextRange) -> starpls_syntax::TextRange {
    starpls_syntax::TextRange::new(
        u32::from(range.start()).into(),
        u32::from(range.end()).into(),
    )
}

/// A cursor can touch a real token or a gap omitted by Ruff's lexer. Gaps must
/// participate in boundary tie-breaking, particularly after closing brackets.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CursorToken {
    Token(ruff_python_ast::token::Token),
    Gap(ruff_text_size::TextRange),
}

impl ruff_text_size::Ranged for CursorToken {
    fn range(&self) -> ruff_text_size::TextRange {
        match self {
            Self::Token(token) => token.range(),
            Self::Gap(range) => *range,
        }
    }
}

pub(crate) fn pick_source_token(
    tokens: &ruff_python_ast::token::Tokens,
    offset: ruff_text_size::TextSize,
    source_len: ruff_text_size::TextSize,
    mut weight: impl FnMut(CursorToken) -> usize,
) -> Option<CursorToken> {
    use ruff_text_size::Ranged;
    if offset > source_len {
        return None;
    }
    let split = tokens.partition_point(|token| token.start() < offset);
    let left = tokens[..split]
        .iter()
        .rev()
        .find(|token| !token.range().is_empty());
    let right = tokens[split..]
        .iter()
        .find(|token| !token.range().is_empty());
    let gap = ruff_text_size::TextRange::new(
        left.map_or(0.into(), Ranged::end),
        right.map_or(source_len, Ranged::start),
    );
    left.copied()
        .map(CursorToken::Token)
        .into_iter()
        .chain((!gap.is_empty()).then_some(CursorToken::Gap(gap)))
        .chain(right.copied().map(CursorToken::Token))
        .filter(|token| token.range().contains_inclusive(offset))
        .max_by_key(|token| weight(*token))
}

pub(crate) fn navigation_token(
    source: &str,
    tokens: &ruff_python_ast::token::Tokens,
    offset: ruff_text_size::TextSize,
) -> Option<CursorToken> {
    use ruff_python_ast::token::TokenKind;
    use ruff_text_size::Ranged;
    pick_source_token(
        tokens,
        offset,
        ruff_text_size::TextSize::of(source),
        |token| {
            let CursorToken::Token(token) = token else {
                return 0;
            };
            match token.kind() {
                TokenKind::Identifier => {
                    if &source[token.range()] == "load" {
                        1
                    } else {
                        2
                    }
                }
                TokenKind::Case => 2,
                TokenKind::Lazy => 2,
                TokenKind::Match => 2,
                TokenKind::Type => 2,
                TokenKind::Lpar => 0,
                TokenKind::Rpar => 0,
                TokenKind::Lsqb => 0,
                TokenKind::Rsqb => 0,
                TokenKind::Lbrace => 0,
                TokenKind::Rbrace => 0,
                TokenKind::Indent => 0,
                TokenKind::Comment => 0,
                TokenKind::NonLogicalNewline => 0,
                _ => 1,
            }
        },
    )
}

#[cfg(test)]
mod cursor_tests {
    use ruff_text_size::Ranged;

    #[test]
    fn native_selection_preserves_editor_boundaries() {
        for (input, expected) in [
            ("$0", None),
            ("name$0(", Some("name")),
            ("f()$0 ", Some(" ")),
            ("f()$0", Some(")")),
            ("def f():\n    pass\n$0name", Some("name")),
            ("match$0 = 1", Some("match")),
            ("load$0(\"defs\")", Some("load")),
            ("name$0\r\n", Some("name")),
            ("f(\"value\"$0,)", Some(",")),
            ("f(\"value\"$0)", Some("\"value\"")),
            ("é$0", Some("é")),
        ] {
            let (before, after) = input.split_once("$0").unwrap();
            let source = format!("{before}{after}");
            let offset = ruff_text_size::TextSize::of(before);
            let parsed = ruff_python_parser::parse_unchecked_source(
                &source,
                ruff_python_ast::PySourceType::Python,
            );
            let actual = super::navigation_token(&source, parsed.tokens(), offset)
                .map(|token| &source[token.range()]);
            assert_eq!(actual, expected, "{input}");
            assert!(super::navigation_token(
                &source,
                parsed.tokens(),
                ruff_text_size::TextSize::of(&source) + ruff_text_size::TextSize::new(1)
            )
            .is_none());
        }
    }
}
