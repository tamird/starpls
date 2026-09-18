//! The local grammar for Starlark type comments. Module syntax uses Ruff AST.

use std::marker::PhantomData;

pub use rowan::ast::AstNode;

use crate::StarlarkLanguage;
use crate::SyntaxKind::*;
use crate::SyntaxKind::{self};
use crate::SyntaxNode;
use crate::SyntaxNodeChildren;
use crate::SyntaxToken;

/// A macro for defining AST nodes. The `AstNode` trait is automatically implemented.
macro_rules! ast_node {
    (
        $(#[doc = $doc:expr])*$node:ident => $kind:ident
        $(child $($child:ident -> $child_node:ident),+;)*
        $(child_token $($child_token:ident -> $child_token_kind:ident),+;)*
        $(children $($children:ident -> $children_node:ident),+;)*
    ) => {
        $(#[doc = $doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $node {
            pub(crate) syntax: SyntaxNode,
        }

        impl AstNode for $node {
            type Language = StarlarkLanguage;

            fn can_cast(kind: SyntaxKind) -> bool {
                kind == $kind
            }

            fn cast(syntax: SyntaxNode) -> Option<Self> {
                if Self::can_cast(syntax.kind()) {
                    Some(Self { syntax })
                } else {
                    None
                }
            }

            fn syntax(&self) -> &SyntaxNode {
                &self.syntax
            }
        }

        impl $node {
        $($(
            pub fn $child(&self) -> Option<$child_node> {
                child(&self.syntax)
            }
        )+)*

        $($(
            pub fn $child_token(&self) -> Option<SyntaxToken> {
                token(self.syntax(), $child_token_kind)
            }
        )+)*

        $($(
            pub fn $children(&self) -> AstChildren<$children_node> {
                AstChildren::new(&self.syntax)
            }
        )+)*
        }
    };
}

pub struct AstChildren<N> {
    inner: SyntaxNodeChildren,
    phantom: PhantomData<N>,
}

impl<N> AstChildren<N> {
    fn new(parent: &SyntaxNode) -> Self {
        AstChildren {
            inner: parent.children(),
            phantom: PhantomData,
        }
    }
}

impl<N: AstNode<Language = StarlarkLanguage>> Iterator for AstChildren<N> {
    type Item = N;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.find_map(N::cast)
    }
}

fn child<N: AstNode<Language = StarlarkLanguage>>(parent: &SyntaxNode) -> Option<N> {
    parent.children().find_map(N::cast)
}

fn token(parent: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    parent
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| token.kind() == kind)
}

ast_node! {
    TypeComment => TYPE_COMMENT
    child body -> TypeCommentBody;
}

impl TypeComment {
    pub fn type_(&self) -> Option<Type> {
        self.body().and_then(|body| body.type_())
    }

    pub fn function_type(&self) -> Option<FunctionType> {
        self.body().and_then(|body| body.function_type())
    }
}

ast_node! {
    TypeCommentBody => TYPE_COMMENT_BODY
    child type_ -> Type;
    child function_type -> FunctionType;
    child ignore -> IgnoreType;
}

ast_node! {
    TypeList => TYPE_LIST
    children types -> Type;
}

pub enum Type {
    PathType(PathType),
    UnionType(UnionType),
    NoneType(NoneType),
    EllipsisType(EllipsisType),
}

impl AstNode for Type {
    type Language = StarlarkLanguage;

    fn can_cast(kind: SyntaxKind) -> bool
    where
        Self: Sized,
    {
        matches!(kind, PATH_TYPE | UNION_TYPE | NONE_TYPE | ELLIPSIS_TYPE)
    }

    fn cast(syntax: SyntaxNode) -> Option<Self>
    where
        Self: Sized,
    {
        Some(match syntax.kind() {
            PATH_TYPE => Self::PathType(PathType { syntax }),
            UNION_TYPE => Self::UnionType(UnionType { syntax }),
            NONE_TYPE => Self::NoneType(NoneType { syntax }),
            ELLIPSIS_TYPE => Self::EllipsisType(EllipsisType { syntax }),
            _ => return None,
        })
    }

    fn syntax(&self) -> &rowan::SyntaxNode<Self::Language> {
        match self {
            Type::PathType(type_) => type_.syntax(),
            Type::UnionType(type_) => type_.syntax(),
            Type::NoneType(type_) => type_.syntax(),
            Type::EllipsisType(type_) => type_.syntax(),
        }
    }
}

ast_node! {
    PathType => PATH_TYPE
    child generic_arguments -> GenericArguments;
    children segments -> PathSegment;
}

ast_node! {
    UnionType => UNION_TYPE
    children types -> Type;
}

ast_node! {
    NoneType => NONE_TYPE
}

ast_node! {
    EllipsisType => ELLIPSIS_TYPE
}

ast_node! {
    GenericArguments => GENERIC_ARGUMENTS
    children types -> Type;
}

ast_node! {
    PathSegment => PATH_SEGMENT
    child_token value -> IDENT;
}

ast_node! {
    IgnoreType => IGNORE_TYPE
}

ast_node! {
    FunctionType => FUNCTION_TYPE
    child parameter_types -> ParameterTypes;
    child ret_type -> Type;
}

ast_node! {
    ParameterTypes => PARAMETER_TYPES
    children types -> ParameterType;
}

pub enum ParameterType {
    Simple(SimpleParameterType),
    ArgsList(ArgsListParameterType),
    KwargsDict(KwargsDictParameterType),
}

impl ParameterType {
    pub fn type_(&self) -> Option<Type> {
        match self {
            ParameterType::Simple(type_) => type_.type_(),
            ParameterType::ArgsList(type_) => type_.type_(),
            ParameterType::KwargsDict(type_) => type_.type_(),
        }
    }
}

impl AstNode for ParameterType {
    type Language = StarlarkLanguage;

    fn can_cast(kind: <Self::Language as rowan::Language>::Kind) -> bool
    where
        Self: Sized,
    {
        matches!(
            kind,
            SIMPLE_PARAMETER_TYPE | ARGS_LIST_PARAMETER_TYPE | KWARGS_DICT_PARAMETER_TYPE
        )
    }

    fn cast(syntax: rowan::SyntaxNode<Self::Language>) -> Option<Self>
    where
        Self: Sized,
    {
        Some(match syntax.kind() {
            SIMPLE_PARAMETER_TYPE => Self::Simple(SimpleParameterType { syntax }),
            ARGS_LIST_PARAMETER_TYPE => Self::ArgsList(ArgsListParameterType { syntax }),
            KWARGS_DICT_PARAMETER_TYPE => Self::KwargsDict(KwargsDictParameterType { syntax }),
            _ => return None,
        })
    }

    fn syntax(&self) -> &rowan::SyntaxNode<Self::Language> {
        match self {
            ParameterType::Simple(type_) => type_.syntax(),
            ParameterType::ArgsList(type_) => type_.syntax(),
            ParameterType::KwargsDict(type_) => type_.syntax(),
        }
    }
}

ast_node! {
    SimpleParameterType => SIMPLE_PARAMETER_TYPE
    child type_ -> Type;
}

ast_node! {
    ArgsListParameterType => ARGS_LIST_PARAMETER_TYPE
    child type_ -> Type;
}

ast_node! {
    KwargsDictParameterType => KWARGS_DICT_PARAMETER_TYPE
    child type_ -> Type;
}
