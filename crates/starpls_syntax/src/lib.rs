pub use rowan::TextRange;
pub use rowan::TextSize;
pub use rowan::TokenAtOffset;

pub use crate::ast::Module;
pub use crate::parser::editor_tree;
pub use crate::parser::from_parsed_module;
pub use crate::parser::parse_module;
pub use crate::parser::parse_type_comments;
pub use crate::parser::ParseTree;
pub use crate::parser::SyntaxError;
pub use crate::parser::TypeComment;
pub use crate::syntax_kind::SyntaxKind;
pub use crate::validation::supports_expr;
pub use crate::validation::validate;

pub mod ast;
mod lexical;
mod parser;
mod ruff;
pub mod source;
mod syntax_kind;
mod type_comments;
mod unescape;
mod validation;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StarlarkLanguage {}

impl rowan::Language for StarlarkLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        raw.0.into()
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind.into())
    }
}

pub type SyntaxNode = rowan::SyntaxNode<StarlarkLanguage>;
pub type SyntaxToken = rowan::SyntaxToken<StarlarkLanguage>;
pub type SyntaxElement = rowan::SyntaxElement<StarlarkLanguage>;
pub type SyntaxNodeChildren = rowan::SyntaxNodeChildren<StarlarkLanguage>;

#[cfg(test)]
mod tests;
