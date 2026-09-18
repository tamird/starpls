//! Source details shared by native lowering and editor syntax.

use std::str::Chars;

use ruff_python_ast::token::parentheses_iterator;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::Tokens;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use ruff_text_size::TextSize;

/// A suite includes trailing comments and whitespace up to its dedent. Missing
/// colons have no suite, even if Python's recovery supplied body statements.
pub fn suite_range(
    tokens: &Tokens,
    body: &[Stmt],
    after: TextSize,
    fallback_end: TextSize,
    next_clause: Option<TextSize>,
) -> Option<TextRange> {
    let fallback_end = next_clause.map_or(fallback_end, |next| fallback_end.min(next));
    let header_end = body
        .first()
        .map_or(fallback_end, Ranged::start)
        .min(fallback_end)
        .max(after);
    let header = tokens.in_range(TextRange::new(after, header_end));
    let colon = header
        .iter()
        .find(|token| token.kind() == TokenKind::Colon)?;
    let start = colon.end();
    let Some(first) = body.first() else {
        return Some(TextRange::new(start, fallback_end.max(start)));
    };
    let mut depth = 0;
    let mut end = fallback_end;
    for token in tokens.after(start) {
        let boundary = match token.kind() {
            TokenKind::Indent => {
                depth += 1;
                None
            }
            TokenKind::Dedent => {
                depth -= 1;
                (depth == 0).then_some(token.start())
            }
            TokenKind::Newline => {
                (depth == 0 && first.start() < token.start()).then_some(token.end())
            }
            _ => None,
        };
        if let Some(boundary) = boundary {
            // Recovery may omit a dedent. Never consume a later clause.
            end = next_clause.map_or(boundary, |next| boundary.min(next));
            break;
        }
    }
    Some(TextRange::new(start, end.max(start)))
}

/// Includes only parentheses owned by this expression, inside its real parent.
pub fn expr_range(expr: &Expr, parent: AnyNodeRef<'_>, tokens: &Tokens) -> TextRange {
    parentheses_iterator(expr.into(), Some(parent), tokens)
        .take_while(|range| range.start() >= parent.start())
        .last()
        .unwrap_or_else(|| expr.range())
}

/// Decode Starlark strings from their original token, retaining the body offset.
/// Starlark's raw escapes and byte rules differ from Python's decoded values.
pub fn string_value(text: &str) -> Option<(Box<str>, u32)> {
    let mut cursor = Cursor::new(text);
    let mut is_raw = false;
    let mut is_bytes = false;

    // Determine the string's prefix.
    // "r" -> raw string
    // "b" -> bytes
    // "rb", "br" -> raw bytes
    match cursor.first() {
        Some('r') => {
            is_raw = true;
            cursor.bump();
            if let Some('b') = cursor.first() {
                is_bytes = true;
                cursor.bump();
            }
        }
        Some('b') => {
            is_bytes = true;
            cursor.bump();
            if let Some('r') = cursor.first() {
                is_raw = true;
                cursor.bump();
            }
        }
        None => return None,
        _ => {}
    }

    // Determine the opening quote, whether the string literal is triple
    // quoted, and if it's terminated.
    let suffix = match cursor.first() {
        Some('\'') => {
            cursor.bump();
            match (cursor.first(), cursor.second()) {
                (Some('\''), Some('\'')) => {
                    cursor.bump();
                    cursor.bump();
                    "'''"
                }
                _ => "'",
            }
        }
        Some('"') => {
            cursor.bump();
            match (cursor.first(), cursor.second()) {
                (Some('"'), Some('"')) => {
                    cursor.bump();
                    cursor.bump();
                    "\"\"\""
                }
                _ => "\"",
            }
        }
        _ => return None,
    };

    if is_bytes || !cursor.text().ends_with(suffix) {
        return None;
    }

    let mut ok = true;
    let mut s = std::string::String::new();
    crate::unescape::unescape_string(
        &cursor.text()[..cursor.text().len() - suffix.len()],
        is_raw,
        suffix.len() == 3,
        &mut |_, res| match res {
            Ok(c) => s.push(c),
            Err(_) => ok = false,
        },
    );

    ok.then(|| {
        (
            s.into_boxed_str(),
            (text.len() - cursor.text().len()) as u32,
        )
    })
}

struct Cursor<'a> {
    chars: Chars<'a>,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Cursor {
            chars: text.chars(),
        }
    }

    fn bump(&mut self) -> Option<char> {
        self.chars.next()
    }

    fn first(&self) -> Option<char> {
        self.chars.clone().next()
    }

    fn second(&self) -> Option<char> {
        let mut chars = self.chars.clone();
        chars.next();
        chars.next()
    }

    fn text(&self) -> &str {
        self.chars.as_str()
    }
}

/// A parameter's source includes a variadic marker and default parentheses.
pub fn parameter_range(
    param: ruff_python_ast::AnyParameterRef<'_>,
    parameters: &ruff_python_ast::Parameters,
    tokens: &Tokens,
) -> TextRange {
    use ruff_python_ast::AnyParameterRef;
    match param {
        AnyParameterRef::NonVariadic(param) => {
            param.default.as_deref().map_or(param.range(), |default| {
                param
                    .range()
                    .cover(expr_range(default, param.into(), tokens))
            })
        }
        AnyParameterRef::Variadic(param) => {
            let marker = if parameters
                .kwarg
                .as_deref()
                .is_some_and(|kwarg| kwarg.range() == param.range())
            {
                TokenKind::DoubleStar
            } else {
                TokenKind::Star
            };
            let start = tokens
                .before(param.name.start())
                .iter()
                .rev()
                .find(|token| token.kind() == marker)
                .map_or(param.start(), Ranged::start);
            TextRange::new(start, param.end())
        }
    }
}

/// Ruff omits the bare `*` from parameters, but it occupies a Starlark function
/// type-comment slot and separates parameter comments.
pub fn bare_star_range(
    parameters: &ruff_python_ast::Parameters,
    tokens: &Tokens,
) -> Option<TextRange> {
    let ruff_python_ast::Parameters {
        node_index: _,
        range,
        posonlyargs: _,
        args,
        vararg,
        kwonlyargs,
        kwarg: _,
    } = parameters;
    if vararg.is_some() {
        return None;
    }
    let first = kwonlyargs.first()?;
    let start = args.last().map_or(range.start(), Ranged::end);
    tokens
        .in_range(TextRange::new(start, first.start()))
        .iter()
        .find(|token| token.kind() == TokenKind::Star)
        .map(Ranged::range)
}
