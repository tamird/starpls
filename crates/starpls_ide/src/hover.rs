use std::fmt::Write;

use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use starpls_common::parsed_module;
use starpls_common::syntax_info;
use starpls_common::File;
use starpls_hir::Semantics;
use starpls_hir::Type;
use starpls_syntax::ast::AstNode;
use starpls_syntax::ast::{self};
use starpls_syntax::source::expr_range;
use starpls_syntax::source::string_value;
use starpls_syntax::TextRange;
use starpls_syntax::T;

use crate::selection::Selection;
use crate::util::navigation_token;
use crate::util::pick_best_token;
use crate::util::text_range;
use crate::util::unindent_doc;
use crate::util::CursorToken;
use crate::Database;
use crate::FilePosition;

mod docs;

pub struct Markup {
    pub value: String,
}
pub struct Hover {
    pub contents: Markup,
    pub range: Option<TextRange>,
}
impl From<String> for Hover {
    fn from(value: String) -> Self {
        Self {
            contents: Markup { value },
            range: None,
        }
    }
}

pub(crate) fn hover(
    db: &Database,
    FilePosition { file_id: file, pos }: FilePosition,
) -> Option<Hover> {
    let sema = Semantics::new(db);
    let source = file.contents(db);
    let parsed = parsed_module(db, file).load(db);
    let offset = u32::from(pos).into();
    let token = navigation_token(&source, parsed.tokens(), offset)?;
    let comments = syntax_info(db, file);
    if let Some(comment) =
        crate::selection::type_comment_at_cursor(comments, offset, token, &source)
    {
        return type_comment_hover(&sema, file, comment, offset);
    }
    if let CursorToken::Token(token) = token {
        if token.kind().is_non_soft_keyword()
            || (token.kind() == TokenKind::Identifier && &source[token.range()] == "load")
        {
            return keyword_hover(&source[token.range()]);
        }
    }
    let node = covering_node(parsed.syntax().into(), token.range());
    match crate::selection::classify(&node, token.range())? {
        Selection::Reference(expr) => {
            Some(format_for_name(expr.id.as_str(), &sema.type_of_expr(file, expr.into())?).into())
        }
        Selection::Attribute(expr) => {
            let ty = sema.type_of_expr(file, expr.value.as_ref().into())?;
            let (field, field_ty) = ty
                .fields()
                .into_iter()
                .find(|(field, _)| field.name().as_str() == expr.attr.as_str())?;
            let mut text = String::from("```python\n");
            if field_ty.is_function() {
                text.push_str("(method) ");
            } else {
                write!(text, "(field) {}: ", expr.attr).ok()?;
            }
            write!(text, "{}\n```\n", field_ty).ok()?;
            let doc = field.doc();
            if !doc.is_empty() {
                text.push_str(&unindent_doc(&doc));
                text.push('\n');
            }
            Some(text.into())
        }
        Selection::Definition(def) => {
            let func = sema.resolve_def_stmt(file, def)?;
            let mut text = format!("```python\n(function) {}\n```\n", func.ty());
            if let Some(doc) = func.doc() {
                text.push_str(&unindent_doc(&doc));
                text.push('\n');
            }
            Some(text.into())
        }
        Selection::Parameter(param) => {
            let (param, ty) = sema.resolve_param(file, param)?;
            let mut text = format!(
                "```python\n(parameter) {}: {}\n```\n",
                param.name().as_ref().map_or("", |name| name.as_str()),
                ty
            );
            if let Some(doc) = param.doc() {
                text.push_str(&unindent_doc(&doc));
                text.push('\n');
            }
            Some(text.into())
        }
        Selection::Keyword { keyword, call } => {
            let func = sema.resolve_call_expr(file, call)?;
            let name = keyword.arg.as_ref()?.as_str();
            let (param, ty) = func
                .params()
                .into_iter()
                .find(|(param, _)| param.name().is_some_and(|param| param.as_str() == name))?;
            let mut text = format!("```python\n(parameter) {name}: {ty}\n```\n");
            if let Some(doc) = param.doc() {
                if !doc.is_empty() {
                    text.push_str(&unindent_doc(&doc));
                    text.push('\n');
                }
            }
            Some(text.into())
        }
        Selection::LoadItem(item) => {
            let item = sema.resolve_load_item(file, item)?;
            let def = item.definition()?;
            Some(format_for_name(item.name().as_str(), &def.ty()).into())
        }
        Selection::LoadModule(call) => {
            let loaded = sema.resolve_load_stmt(file, call)?;
            let mut text = format!("```python\n(module) {}\n```\n", &source[token.range()]);
            if let Some(doc) = module_doc(&sema, loaded) {
                text.push_str(&unindent_doc(&doc));
                text.push('\n');
            }
            Some(text.into())
        }
        Selection::String(_) => None,
    }
}

fn keyword_hover(keyword: &str) -> Option<Hover> {
    let docs = match keyword {
        "break" => docs::BREAK_DOCS,
        "continue" => docs::CONTINUE_DOCS,
        "def" => docs::DEF_DOCS,
        "for" => docs::FOR_DOCS,
        "if" => docs::IF_DOCS,
        "load" => docs::LOAD_DOCS,
        "pass" => docs::PASS_DOCS,
        "return" => docs::RETURN_DOCS,
        _ => return None,
    };
    Some(docs.to_owned().into())
}

fn type_comment_hover(
    sema: &Semantics<'_>,
    file: File,
    comment: &starpls_syntax::TypeComment,
    offset: ruff_text_size::TextSize,
) -> Option<Hover> {
    let local = u32::from(offset - comment.range.start()).into();
    let tree = comment.parsed.syntax();
    let token = pick_best_token(tree.token_at_offset(local), |kind| match kind {
        T![ident] => 2,
        T!['('] | T![')'] | T!['['] | T![']'] | T!['{'] | T!['}'] => 0,
        kind if kind.is_trivia_token() => 0,
        _ => 1,
    })?;
    if token.kind().is_keyword() {
        return keyword_hover(token.text());
    }
    let segment = ast::PathSegment::cast(token.parent()?)?;
    let path = ast::PathType::cast(segment.syntax().parent()?)?;
    let ty = sema.resolve_path_type(file, text_range(comment.range), &path)?;
    let mut text = format!("```python\n(type) {ty}\n```\n");
    if let Some(doc) = ty.doc() {
        text.push_str(&unindent_doc(&doc));
        text.push('\n');
    }
    Some(text.into())
}

fn module_doc(sema: &Semantics<'_>, file: File) -> Option<Box<str>> {
    let source = file.contents(sema.db);
    let parsed = parsed_module(sema.db, file).load(sema.db);
    let first = parsed.syntax().body.first()?;
    if syntax_info(sema.db, file)
        .first()
        .is_some_and(|comment| comment.range.start() < first.start())
    {
        return None;
    }
    let Stmt::Expr(stmt) = first else {
        return None;
    };
    let Expr::StringLiteral(_) = stmt.value.as_ref() else {
        return None;
    };
    if !starpls_syntax::supports_expr(&stmt.value, parsed.tokens())
        || expr_range(&stmt.value, stmt.into(), parsed.tokens()) != stmt.value.range()
    {
        return None;
    }
    string_value(&source[stmt.value.range()]).map(|(doc, _)| doc)
}

fn format_for_name(name: &str, ty: &Type<'_>) -> String {
    let mut text = String::from("```python\n");

    // Handle special `def` formatting for function types.
    if ty.is_function() {
        text.push_str("(function) ");
    } else {
        text.push_str("(variable) ");
        text.push_str(name);
        text.push_str(": ");
    }

    write!(&mut text, "{}", ty).unwrap();
    text.push_str("\n```\n");

    if let Some(doc) = ty.doc() {
        text.push_str(&unindent_doc(&doc));
        text.push('\n');
    }

    text
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use expect_test::Expect;

    use crate::Analysis;
    use crate::FilePosition;

    fn check_hover(fixture: &str, expect: Expect) {
        let (analysis, fixture) = Analysis::from_single_file_fixture(fixture);
        let hover = analysis
            .snapshot()
            .hover(
                fixture
                    .cursor_pos
                    .map(|(file_id, pos)| FilePosition { file_id, pos })
                    .unwrap(),
            )
            .unwrap()
            .unwrap();

        expect.assert_eq(&hover.contents.value);
    }

    #[test]
    fn load_cursor_roles() {
        for (input, hover, navigates) in [
            ("load(\"defs.bzl\", al$0ias = \"value\")", None, false),
            (
                "load(\"defs.bzl\", alias = \"val$0ue\")",
                Some("(variable) value: Literal[1]"),
                true,
            ),
            ("load(\"defs.bzl\", \"value\"$0,)", None, false),
            (
                "load(\"defs.bzl\", \"value\"$0)",
                Some("(variable) value: Literal[1]"),
                true,
            ),
            (
                "load(\"defs.bzl\", alias = # type: int$0\n \"value\")",
                Some("(type) int"),
                false,
            ),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
            fixture.add_file(&mut analysis.db, "defs.bzl", "value = 1\n");
            fixture.add_file(&mut analysis.db, "main.bzl", input);
            loader.add_files_from_fixture(&fixture);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let snapshot = analysis.snapshot();
            let position = FilePosition { file_id, pos };
            let actual = snapshot.hover(position.clone()).unwrap();
            match hover {
                Some(expected) => {
                    assert!(actual.unwrap().contents.value.contains(expected), "{input}")
                }
                None => assert!(actual.is_none(), "{input}"),
            }
            assert_eq!(
                snapshot.goto_definition(position, false).unwrap().is_some(),
                navigates,
                "{input}"
            );
        }
    }

    #[test]
    fn module_documentation_uses_first_direct_child() {
        for (source, documented) in [
            ("# ordinary comment\n\n\"module documentation\"\n", true),
            ("# type: ignore\n\"module documentation\"\n", false),
            ("(\"module documentation\")\n", false),
            ("value = 1\n\"module documentation\"\n", false),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
            fixture.add_file(&mut analysis.db, "defs.bzl", source);
            fixture.add_file(
                &mut analysis.db,
                "main.bzl",
                "load(\"defs$0.bzl\", \"value\")",
            );
            loader.add_files_from_fixture(&fixture);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert_eq!(
                hover.contents.value.contains("module documentation"),
                documented,
                "{source}"
            );
        }
    }

    #[test]
    fn recovery_and_comment_boundaries() {
        for (source, expected) in [
            ("def f(name): pass\nf(na$0me=)", "(parameter) name: Unknown"),
            ("case = 1\ncase$0# type: int", "(variable) case: Literal[1]"),
            ("value = 1 # type: int$0\n", "(type) int"),
        ] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert!(
                hover.contents.value.contains(expected),
                "{source}: {}",
                hover.contents.value
            );
        }
        let (analysis, fixture) = Analysis::from_single_file_fixture("# type: de$0f");
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let hover = analysis
            .snapshot()
            .hover(FilePosition { file_id, pos })
            .unwrap()
            .unwrap();
        assert_eq!(hover.contents.value, super::docs::DEF_DOCS);
    }

    #[test]
    fn type_comments_resolve_in_their_declaration_context() {
        for (body, expected) in [
            ("def f(x, # type: P$0\n): pass", "P"),
            ("def f():\n    if True:\n        x = P() # type: P$0", "P"),
            ("def f():\n    Q = provider()\n    x = Q() # type: Q$0", "Q"),
            ("def f(): # type: () -> P$0\n    pass", "P"),
            ("x = 0; # type: P$0", "Unknown"),
            (
                "def f():\n    \"doc\"\n    # type: () -> P$0\n    pass",
                "Unknown",
            ),
            ("def f(x=(\n    1 # type: P$0\n)): pass", "Unknown"),
        ] {
            let input = format!("P = provider()\n{body}\n");
            let (analysis, fixture) = Analysis::from_single_file_fixture(&input);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert_eq!(
                hover.contents.value,
                format!("```python\n(type) {expected}\n```\n"),
                "{input}"
            );
        }
    }

    #[test]
    fn check_variable() {
        check_hover(
            r#"
a$0bc = 123
"#,
            expect![[r#"
                ```python
                (variable) abc: Literal[123]
                ```
            "#]],
        );
    }

    #[test]
    fn parenthesized_singleton_loop_target() {
        check_hover(
            "for (x,) in [(1,)]:\n    x$0\n",
            expect![[r#"
                ```python
                (variable) x: Literal[1]
                ```
            "#]],
        );
    }

    #[test]
    fn check_def_stmt() {
        check_hover(
            r#"
def f$0oo(x, y):
    """Doc string"""
    pass
"#,
            expect![[r#"
                ```python
                (function) def foo(x, y) -> Unknown
                ```
                Doc string  
            "#]],
        );
    }

    #[test]
    fn check_call_expr() {
        check_hover(
            r#"
def foo(x, y):
    """Doc string"""
    pass

f$0oo(1, 2)
"#,
            expect![[r#"
                ```python
                (function) def foo(x, y) -> Unknown
                ```
                Doc string  
            "#]],
        );
    }

    #[test]
    fn check_type() {
        check_hover(
            r#"
x = 1 # type: i$0nt
"#,
            expect![[r#"
                ```python
                (type) int
                ```
            "#]],
        );
    }

    #[test]
    fn check_param() {
        check_hover(
            r#"
def foo(a$0bc):
    """
    Args:
        abc: Easy as 123!
    """
    pass
"#,
            expect![[r#"
                ```python
                (parameter) abc: Unknown
                ```
                Easy as 123!  
            "#]],
        );
    }

    #[test]
    fn check_arg() {
        check_hover(
            r#"
def foo(abc):
    """
    Args:
        abc: Easy as 123!
    """
    pass

foo(a$0bc = 123)
"#,
            expect![[r#"
                ```python
                (parameter) abc: Unknown
                ```
                Easy as 123!  
            "#]],
        );
    }

    #[test]
    fn check_field() {
        check_hover(
            r#"
foo = struct(bar = 123)
foo.b$0ar
"#,
            expect![[r#"
                ```python
                (field) bar: Literal[123]
                ```
            "#]],
        );
    }

    #[test]
    fn check_method() {
        check_hover(
            r#"
def bar():
    pass

foo = struct(bar = bar)
foo.b$0ar
"#,
            expect![[r#"
                ```python
                (method) def bar() -> Unknown
                ```
            "#]],
        );
    }

    #[test]
    fn check_provider_doc() {
        check_hover(
            r#"
Foo$0Info = provider(doc = "The foo provider")
"#,
            expect![[r#"
                ```python
                (variable) FooInfo: Provider[FooInfo]
                ```
                The foo provider  
            "#]],
        );
    }

    #[test]
    fn check_provider_field_doc() {
        check_hover(
            r#"
FooInfo = provider(
    doc = "The foo provider",
    fields = {
        "bar": "The bar field",
    },
)

foo = FooInfo(bar = "bar")
foo.b$0ar
"#,
            expect![[r#"
                ```python
                (field) bar: Unknown
                ```
                The bar field  
            "#]],
        );
    }

    #[test]
    fn check_rule_attr() {
        check_hover(
            r#"
foo = rule(
    attrs = {
        "bar": attr.string(doc = "The bar attr"),
    },
)

foo(
    name = "foo",
    b$0ar = "bar",
)
"#,
            expect![[r#"
                ```python
                (parameter) bar: string
                ```
                The bar attr  
            "#]],
        );
    }
}
