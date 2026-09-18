use std::marker::PhantomData;

use rowan::ast::AstNode;
use rowan::GreenNode;
use rowan::GreenNodeBuilder;
use rowan::Language;
use rowan::TextRange;

use crate::type_comments::parse_type_list;
use crate::type_comments::StrStep;
use crate::type_comments::StrWithTokens;
use crate::Module;
use crate::StarlarkLanguage;
use crate::SyntaxKind::*;
use crate::SyntaxNode;

const TYPE_COMMENT_PREFIX_STR: &str = "# type: ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
}

/// The result of parsing a Starlark module and constructing a Rowan syntax tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTree<T> {
    green: GreenNode,
    _ty: PhantomData<fn() -> T>,
}

impl<T> ParseTree<T> {
    fn new(green: GreenNode) -> Self {
        ParseTree {
            green,
            _ty: PhantomData,
        }
    }

    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }
}

impl<T: AstNode<Language = StarlarkLanguage>> ParseTree<T> {
    pub fn tree(&self) -> T {
        T::cast(self.syntax()).unwrap()
    }
}

pub fn parse_module(input: &str, errors_sink: &mut dyn FnMut(SyntaxError)) -> ParseTree<Module> {
    let parsed =
        ruff_python_parser::parse_unchecked_source(input, ruff_python_ast::PySourceType::Python);
    from_parsed_module(input, &parsed, errors_sink)
}

/// Applies Starlark validation and constructs the editor tree from a shared parse.
///
/// `input` must be the exact source text used to produce `parsed`.
pub fn from_parsed_module(
    input: &str,
    parsed: &ruff_python_parser::Parsed<ruff_python_ast::ModModule>,
    errors_sink: &mut dyn FnMut(SyntaxError),
) -> ParseTree<Module> {
    crate::validate(input, parsed, errors_sink);
    let comments = parse_type_comments(input, parsed.tokens(), errors_sink);
    editor_tree(input, parsed, &comments)
}

pub(super) fn build_type_comment(
    builder: &mut GreenNodeBuilder,
    text: &str,
    text_start: usize,
    errors_sink: &mut dyn FnMut(SyntaxError),
) {
    builder.start_node(StarlarkLanguage::kind_to_raw(TYPE_COMMENT));
    builder.token(
        StarlarkLanguage::kind_to_raw(TYPE_COMMENT_PREFIX),
        TYPE_COMMENT_PREFIX_STR,
    );

    let str_with_tokens = StrWithTokens::new(&text[TYPE_COMMENT_PREFIX_STR.len()..]);
    let output = parse_type_list(&str_with_tokens.to_input());

    str_with_tokens.build_with_trivia(output, &mut |str_step| match str_step {
        StrStep::Start { kind } => builder.start_node(StarlarkLanguage::kind_to_raw(kind)),
        StrStep::Finish => builder.finish_node(),
        StrStep::Token { kind, text } => builder.token(StarlarkLanguage::kind_to_raw(kind), text),
        StrStep::Error { message, pos } => {
            let offset = ((text_start + TYPE_COMMENT_PREFIX_STR.len()) as u32
                + str_with_tokens.token_pos(pos))
            .into();
            errors_sink(SyntaxError {
                message,
                range: TextRange::new(offset, offset),
            })
        }
    });

    builder.finish_node();
}

/// A type comment has its own small syntax tree; its ranges are relative to the
/// comment, while `range` locates it in the source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeComment {
    pub range: ruff_text_size::TextRange,
    pub parsed: ParseTree<crate::ast::TypeComment>,
}

pub fn parse_type_comments(
    source: &str,
    tokens: &ruff_python_ast::token::Tokens,
    errors: &mut dyn FnMut(SyntaxError),
) -> Vec<TypeComment> {
    use ruff_text_size::Ranged;
    tokens
        .iter()
        .filter_map(|token| {
            if token.kind() != ruff_python_ast::token::TokenKind::Comment {
                return None;
            }
            let text = &source[token.range()];
            if !text.starts_with(TYPE_COMMENT_PREFIX_STR) {
                return None;
            }
            let mut builder = GreenNodeBuilder::new();
            build_type_comment(&mut builder, text, usize::from(token.start()), errors);
            Some(TypeComment {
                range: token.range(),
                parsed: ParseTree::new(builder.finish()),
            })
        })
        .collect()
}

/// Constructs the transitional editor tree from already validated syntax and
/// parsed comments. Callers must use comments from the exact same source.
pub fn editor_tree(
    input: &str,
    parsed: &ruff_python_parser::Parsed<ruff_python_ast::ModModule>,
    comments: &[TypeComment],
) -> ParseTree<Module> {
    ParseTree::new(crate::ruff::parse(input, parsed, comments))
}
