//! Starlark spelling and escape rules layered over Ruff's tokens.
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::Tokens;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use ruff_text_size::TextSize;

use crate::unescape::unescape_byte_string;
use crate::unescape::unescape_string;
use crate::SyntaxError;

pub(super) fn validate(source: &str, tokens: &Tokens, errors: &mut dyn FnMut(SyntaxError)) {
    let mut line_start = true;
    let mut previous_end = TextSize::new(0);
    for token in tokens.iter() {
        if line_start {
            let indentation = &source[TextRange::new(previous_end, token.start())];
            if indentation.contains('\t') {
                error(
                    errors,
                    TextRange::new(previous_end, token.start()),
                    "Starlark indentation must use spaces",
                );
            }
        }
        let text = &source[token.range()];
        match token.kind() {
            TokenKind::Newline => line_start = true,
            TokenKind::Indent => {
                if text.contains('\t') {
                    error(
                        errors,
                        token.range(),
                        "Starlark indentation must use spaces",
                    );
                }
            }
            TokenKind::NonLogicalNewline
            | TokenKind::Comment
            | TokenKind::Dedent
            | TokenKind::EndOfFile => {}
            TokenKind::Name => {
                line_start = false;
                if !text.is_ascii() {
                    error(errors, token.range(), "Starlark identifiers must be ASCII");
                }
            }
            TokenKind::Int | TokenKind::Float => {
                line_start = false;
                if text.contains('_') || text.starts_with("0b") || text.starts_with("0B") {
                    error(
                        errors,
                        token.range(),
                        "Unsupported Starlark numeric literal",
                    );
                }
            }
            TokenKind::String => {
                line_start = false;
                // Ruff supplies the token boundary. Decode its contents with
                // Starlark's existing rules, including raw escaped quotes and
                // the different ranges of string and byte escapes.
                let prefix_len = text
                    .find(['\'', '"'])
                    .expect("a string token has an opener");
                let prefix = &text[..prefix_len];
                if !matches!(prefix, "" | "r" | "b" | "br" | "rb") {
                    error(errors, token.range(), "Unsupported Starlark string prefix");
                }
                let triple = token.is_triple_quoted_string();
                let quote_len = if triple { 3 } else { 1 };
                let start = prefix_len + quote_len;
                let quote = &text[prefix_len..start];
                let end = if text.len() >= start + quote_len && text.ends_with(quote) {
                    text.len() - quote_len
                } else {
                    text.len()
                };
                let contents = &text[start..end];
                let contents_start = token.start() + TextSize::try_from(start).unwrap();
                let mut report = |range: std::ops::Range<usize>, message| {
                    let range = TextRange::new(
                        contents_start + TextSize::try_from(range.start).unwrap(),
                        contents_start + TextSize::try_from(range.end).unwrap(),
                    );
                    error(errors, range, message);
                };
                if prefix.contains('b') {
                    unescape_byte_string(contents, &mut |range, result| {
                        if let Err(err) = result {
                            report(range, crate::unescape::error_message(err));
                        }
                    });
                } else {
                    unescape_string(
                        contents,
                        prefix.contains('r'),
                        triple,
                        &mut |range, result| {
                            if let Err(err) = result {
                                report(range, crate::unescape::error_message(err));
                            }
                        },
                    );
                }
            }
            _ => line_start = false,
        }
        previous_end = token.end();
    }
}

fn error(errors: &mut dyn FnMut(SyntaxError), range: TextRange, message: &str) {
    errors(SyntaxError {
        message: message.to_owned(),
        range: rowan::TextRange::new(
            u32::from(range.start()).into(),
            u32::from(range.end()).into(),
        ),
    });
}
