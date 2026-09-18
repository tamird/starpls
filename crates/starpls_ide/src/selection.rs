//! Borrowed roles under a cursor, shared by hover and definition navigation.

use ruff_python_ast::find_node::CoveringNode;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprAttribute;
use ruff_python_ast::ExprCall;
use ruff_python_ast::ExprName;
use ruff_python_ast::ExprStringLiteral;
use ruff_python_ast::Keyword;
use ruff_python_ast::Parameter;
use ruff_python_ast::StmtFunctionDef;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;

pub(crate) enum Selection<'a> {
    Reference(&'a ExprName),
    Attribute(&'a ExprAttribute),
    Definition(&'a StmtFunctionDef),
    Parameter(&'a Parameter),
    Keyword {
        keyword: &'a Keyword,
        call: &'a ExprCall,
    },
    LoadModule(&'a ExprCall),
    LoadItem(ArgOrKeyword<'a>),
    String(&'a ExprStringLiteral),
}

pub(crate) fn classify<'a>(node: &CoveringNode<'a>, range: TextRange) -> Option<Selection<'a>> {
    // Load arguments are opaque Starlark declarations, even when Python's
    // recovery contains expression nodes inside an invalid argument.
    for (child, parent) in node.ancestors().zip(node.ancestors().skip(1)) {
        let AnyNodeRef::ExprCall(call) = child else {
            continue;
        };
        let AnyNodeRef::StmtExpr(_) = parent else {
            continue;
        };
        let Expr::Name(name) = call.func.as_ref() else {
            continue;
        };
        if name.id != "load" {
            continue;
        }
        for (index, arg) in call.arguments.iter_source_order().enumerate() {
            if !arg.range().contains_range(range) {
                continue;
            }
            return match arg {
                ArgOrKeyword::Arg(_) => Some(if index == 0 {
                    Selection::LoadModule(call)
                } else {
                    Selection::LoadItem(arg)
                }),
                ArgOrKeyword::Keyword(keyword) => {
                    if keyword
                        .arg
                        .as_ref()
                        .is_some_and(|name| name.range().contains_range(range))
                    {
                        return None;
                    }
                    Some(Selection::LoadItem(arg))
                }
            };
        }
        return None;
    }
    match node.node() {
        AnyNodeRef::ExprName(name) => Some(Selection::Reference(name)),
        AnyNodeRef::Identifier(_) => match node.parent()? {
            AnyNodeRef::ExprAttribute(expr) => Some(Selection::Attribute(expr)),
            AnyNodeRef::StmtFunctionDef(def) => Some(Selection::Definition(def)),
            AnyNodeRef::Parameter(param) => Some(Selection::Parameter(param)),
            AnyNodeRef::Keyword(keyword) => {
                let call = node.ancestors().find_map(|node| match node {
                    AnyNodeRef::ExprCall(call) => Some(call),
                    _ => None,
                })?;
                Some(Selection::Keyword { keyword, call })
            }
            _ => None,
        },
        AnyNodeRef::StringLiteral(_) => node.ancestors().find_map(|node| match node {
            AnyNodeRef::ExprStringLiteral(expr) => Some(Selection::String(expr)),
            _ => None,
        }),
        _ => None,
    }
}

/// Type comments have their own tokens, which outrank surrounding trivia.
/// An adjacent identifier still outranks the comment prefix at its start.
pub(crate) fn type_comment_at_cursor<'a>(
    comments: &'a [starpls_syntax::TypeComment],
    offset: ruff_text_size::TextSize,
    selected: crate::util::CursorToken,
    source: &str,
) -> Option<&'a starpls_syntax::TypeComment> {
    use ruff_python_ast::token::TokenKind;

    use crate::util::CursorToken;
    let end = comments.partition_point(|comment| comment.range.start() <= offset);
    let comment = comments.get(end.checked_sub(1)?)?;
    if !comment.range.contains_inclusive(offset) {
        return None;
    }
    if offset == comment.range.start() {
        if let CursorToken::Token(token) = selected {
            let identifier = match token.kind() {
                TokenKind::Identifier => &source[token.range()] != "load",
                TokenKind::Case => true,
                TokenKind::Lazy => true,
                TokenKind::Match => true,
                TokenKind::Type => true,
                _ => false,
            };
            if token.end() == offset && identifier {
                return None;
            }
        }
    }
    Some(comment)
}
