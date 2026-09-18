use std::marker::PhantomData;

use rowan::ast::AstNode;
use rowan::GreenNode;
use rowan::GreenNodeBuilder;
use rowan::Language;
use rowan::TextRange;

use crate::type_comments::parse_type_list;
use crate::type_comments::StrStep;
use crate::type_comments::StrWithTokens;
use crate::StarlarkLanguage;
use crate::SyntaxKind::*;
use crate::SyntaxNode;

const TYPE_COMMENT_PREFIX_STR: &str = "# type: ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
}

/// A parsed type-comment tree with ranges local to the comment.
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

fn build_type_comment(
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
