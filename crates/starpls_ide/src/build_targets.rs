//! The syntactic BUILD target convention shared by symbols and label navigation.

use ruff_python_ast::token::Tokens;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprCall;
use ruff_python_ast::Keyword;
use ruff_python_ast::ModModule;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use starpls_syntax::source::expr_range;
use starpls_syntax::source::string_value;

pub(crate) fn calls<'a>(
    module: &'a ModModule,
    tokens: &'a Tokens,
) -> impl Iterator<Item = &'a ExprCall> {
    module.body.iter().filter_map(|stmt| {
        let Stmt::Expr(stmt) = stmt else {
            return None;
        };
        let Expr::Call(call) = stmt.value.as_ref() else {
            return None;
        };
        if expr_range(&stmt.value, stmt.into(), tokens) != call.range() {
            return None;
        }
        if let Expr::Name(name) = call.func.as_ref() {
            if name.id == "load" {
                return None;
            }
        }
        Some(call)
    })
}

/// Keep all valid `name` keywords: symbols select the first, while navigation
/// accepts any matching name in recovered duplicate-keyword syntax.
pub(crate) fn names<'a>(
    call: &'a ExprCall,
    source: &'a str,
    tokens: &'a Tokens,
) -> impl Iterator<Item = Box<str>> + 'a {
    call.arguments.keywords.iter().filter_map(|keyword| {
        let Keyword {
            node_index: _,
            range: _,
            arg,
            value,
        } = keyword;
        if arg.as_ref()?.as_str() != "name" {
            return None;
        }
        let Expr::StringLiteral(_) = value else {
            return None;
        };
        if !starpls_syntax::supports_expr(value.into(), tokens)
            || expr_range(value, (&call.arguments).into(), tokens) != value.range()
        {
            return None;
        }
        string_value(&source[value.range()]).map(|(value, _)| value)
    })
}

#[cfg(test)]
mod tests {
    use ruff_text_size::Ranged;

    #[test]
    fn retains_literal_names_and_top_level_call_policy() {
        let source = r#"
rule(name=1, name="first", name="second")
(rule)(name="\x61")
rule(name="first")
rule(name="")
(rule(name="parenthesized call"))
rule(name=("parenthesized name"))
rule(name="concatenated" "name")
rule(name=b"bytes")
load(":defs.bzl", name="loaded")
(load)(":defs.bzl", name="loaded")
assigned = rule(name="assigned")
if True:
    rule(name="nested")
"#;
        let (analysis, fixture) = crate::Analysis::from_single_file_fixture(source);
        let parsed =
            starpls_common::parsed_module(&analysis.db, fixture.main_file()).load(&analysis.db);
        let targets = super::calls(parsed.syntax(), parsed.tokens())
            .filter_map(|call| {
                let names = super::names(call, source, parsed.tokens()).collect::<Vec<_>>();
                (!names.is_empty()).then(|| (&source[call.range()], names))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            [
                (
                    r#"rule(name=1, name="first", name="second")"#,
                    vec!["first".into(), "second".into()]
                ),
                (r#"(rule)(name="\x61")"#, vec!["a".into()]),
                (r#"rule(name="first")"#, vec!["first".into()]),
                (r#"rule(name="")"#, vec!["".into()]),
            ]
        );
    }
}
