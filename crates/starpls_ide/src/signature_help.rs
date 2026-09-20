use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::Tokens;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::ExprCall;
use ruff_python_ast::ModModule;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use ruff_text_size::TextSize;
use starpls_hir::Semantics;
use starpls_syntax::source::expr_range;
use ty_python_semantic::types::ide_support::call_signature_details;
use ty_python_semantic::types::ide_support::CallSignatureDetails;
use ty_python_semantic::types::ide_support::CallSignatureParameter;
use ty_python_semantic::types::Type;
use ty_python_semantic::HasType;
use ty_python_semantic::SemanticModel;

use crate::util::parameter_doc;
use crate::util::pick_source_token;
use crate::util::unindent_doc;
use crate::util::CursorToken;
use crate::Database;
use crate::FilePosition;

const DEFAULT_ACTIVE_PARAMETER_INDEX: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureHelp {
    pub signatures: Vec<SignatureInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureInfo {
    pub label: String,
    pub documentation: Option<String>,
    pub parameters: Option<Vec<ParameterInfo>>,
    pub active_parameter: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterInfo {
    pub label: String,
    pub documentation: Option<String>,
}

pub(crate) fn signature_help(
    db: &Database,
    FilePosition { file_id, pos }: FilePosition,
) -> Option<SignatureHelp> {
    let file = file_id;
    let program_file = db.starlark_program_file(file);
    let parsed = ruff_db::parsed::parsed_module(db, program_file.python_file(db)).load(db);
    let source = file.contents(db);
    let (expr, active_arg) = call_at_cursor(
        parsed.syntax(),
        parsed.tokens(),
        u32::from(pos).into(),
        TextSize::of(&*source),
    )?;
    if !Semantics::new(db).contains_expr(file, expr.into()) {
        return None;
    }
    let model = SemanticModel::new(db, program_file);
    let callee_type = expr.func.inferred_type(&model);
    let constructor = matches!(callee_type, Some(Type::ClassLiteral(_)));
    let provided_documentation = callee_type
        .and_then(|ty| ty.provided_data(db, &model.program_environment()))
        .and_then(|data| data.downcast_ref::<crate::ty::Documentation>());
    let signatures = call_signature_details(&model, expr)
        .into_iter()
        .map(|details| {
            let active_parameter = details.active_parameter(active_arg);
            let CallSignatureDetails {
                signature: _,
                label,
                parameters,
                definition,
                argument_to_parameter_mapping: _,
                argument_to_displayed_parameter_mapping: _,
            } = details;
            let source_documentation = definition.and_then(|definition| definition.docstring(db));
            let documentation = provided_documentation
                .and_then(|doc| doc.text.as_deref())
                .or(source_documentation.as_deref());
            let name = if constructor {
                None
            } else {
                definition.and_then(|definition| definition.name(db))
            };
            let name = name.as_deref().unwrap_or(&source[expr.func.range()]);
            SignatureInfo {
                label: format!("def {name}{label}"),
                documentation: documentation.map(unindent_doc),
                parameters: Some(
                    parameters
                        .into_iter()
                        .map(|parameter| {
                            let CallSignatureParameter {
                                label,
                                name,
                                ty: _,
                                is_positional_only: _,
                                is_variadic: _,
                                is_keyword_variadic: _,
                            } = parameter;
                            let documentation = provided_documentation
                                .and_then(|doc| {
                                    doc.parameters.iter().find_map(|(parameter, text)| {
                                        (parameter.as_str() == name).then(|| unindent_doc(text))
                                    })
                                })
                                .or_else(|| parameter_doc(documentation, &name).map(str::to_owned));
                            ParameterInfo {
                                label,
                                documentation,
                            }
                        })
                        .collect(),
                ),
                active_parameter: Some(active_parameter.unwrap_or(DEFAULT_ACTIVE_PARAMETER_INDEX)),
            }
        })
        .collect::<Vec<_>>();
    if signatures.is_empty() {
        return None;
    }
    Some(SignatureHelp { signatures })
}

/// Select the call and argument using concrete tokens, including trivia that
/// Ruff does not store in the AST. Commas inside an argument belong to it, even
/// when they are not bracketed (for example, lambda parameters).
fn call_at_cursor<'a>(
    module: &'a ModModule,
    tokens: &Tokens,
    offset: TextSize,
    source_len: TextSize,
) -> Option<(&'a ExprCall, usize)> {
    let selected = pick_source_token(tokens, offset, source_len, |token| {
        let CursorToken::Token(token) = token else {
            return 0;
        };
        match token.kind() {
            TokenKind::Lpar => 0,
            TokenKind::Rpar => 0,
            TokenKind::Comma => 0,
            TokenKind::Comment => 0,
            TokenKind::NonLogicalNewline => 0,
            TokenKind::Indent => 0,
            _ => 1,
        }
    })?;
    let node = covering_node(module.into(), selected.range());
    let enclosing_call = |node: AnyNodeRef<'a>| match node {
        AnyNodeRef::ExprCall(call) => Some(call),
        _ => None,
    };
    let call = if let Some(call) = node.ancestors().find_map(enclosing_call) {
        call
    } else {
        // A recovered call may end at its last argument, before trailing
        // whitespace. Only an actually unmatched call can own that gap.
        let CursorToken::Gap(gap) = selected else {
            return None;
        };
        let previous = tokens
            .before(gap.start())
            .iter()
            .rev()
            .find(|token| !token.range().is_empty())?;
        let node = covering_node(module.into(), previous.range());
        let call = node.ancestors().filter_map(enclosing_call).find(|call| {
            let mut depth = 0;
            for token in tokens.in_range(TextRange::new(call.arguments.start(), gap.start())) {
                match token.kind() {
                    TokenKind::Lpar => depth += 1,
                    TokenKind::Rpar => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    return false;
                }
            }
            depth > 0
        })?;
        call
    };
    if selected.start() < call.arguments.start() {
        return None;
    }
    let ranges = call
        .arguments
        .iter_source_order()
        .map(|arg| match arg {
            ArgOrKeyword::Arg(expr) => expr_range(expr, (&call.arguments).into(), tokens),
            ArgOrKeyword::Keyword(keyword) => TextRange::new(
                keyword.start(),
                expr_range(&keyword.value, keyword.into(), tokens).end(),
            ),
        })
        .collect::<Vec<_>>();
    if let Some(index) = ranges
        .iter()
        .position(|range| range.contains_range(selected.range()))
    {
        return Some((call, index));
    }
    let mut arguments = ranges.iter().peekable();
    let commas = tokens
        .in_range(call.arguments.range())
        .iter()
        .take_while(|token| token.start() <= selected.start())
        .filter(|token| token.kind() == TokenKind::Comma)
        .filter(|token| {
            while arguments
                .peek()
                .is_some_and(|range| range.end() <= token.start())
            {
                arguments.next();
            }
            !arguments
                .peek()
                .is_some_and(|range| range.contains_range(token.range()))
        })
        .count();
    Some((call, commas))
}

#[cfg(test)]
mod tests {
    use crate::Analysis;
    use crate::FilePosition;

    #[test]
    fn call_and_argument_cursor_boundaries() {
        for (call, active) in [
            ("f($0)", Some(0)),
            ("f(0$0)", Some(0)),
            ("f(0,$0)", Some(1)),
            ("f(0,$0 )", Some(1)),
            ("f(0,,$0)", Some(100)),
            ("f(lambda x,$0 y: x, 0)", Some(0)),
            ("f((0,$0 1), 0)", Some(0)),
            ("f((0)$0, 1)", Some(1)),
            ("f((0)$0 , 1)", Some(0)),
            ("f(0, # type: in$0t\n 1)", Some(1)),
            ("f(x=(0,$0 1), y=0)", Some(0)),
            ("f(g() $0", Some(0)),
            ("f(0, g$0())", None),
            ("f$0()", None),
            ("f()$0", Some(0)),
            ("f()$0 ", None),
            ("f()\n$0", None),
        ] {
            let source = format!("def f(x, y): pass\ndef g(): pass\n{call}");
            let (analysis, fixture) = Analysis::from_single_file_fixture(&source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let help = analysis
                .snapshot()
                .signature_help(FilePosition { file_id, pos })
                .unwrap();
            let actual = help
                .as_ref()
                .and_then(|help| help.signatures[0].active_parameter);
            assert_eq!(actual, active, "{call}: {help:?}");
        }
    }

    #[test]
    fn imported_rule_default_follows_source_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                starpls_bazel::Builtins::default(),
            )
            .unwrap();
        let dependency = fixture.add_file(&mut analysis.db, "defs.bzl", "");
        fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "load(\"defs.bzl\", \"r\")\nr(fo$0o = \"\")",
        );
        loader.add_files_from_fixture(&fixture);
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        for default in ["\"é\"", "(\"日本語\")", "\"é\""] {
            analysis.update_file(
                dependency,
                format!("r = rule(doc = \"Rule documentation\", attrs = {{\"foo\": attr.string(doc = \"Attribute documentation\", default = {default})}})"),
            );
            let help = analysis
                .snapshot()
                .signature_help(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            let [signature] = help.signatures.as_slice() else {
                panic!("{help:?}");
            };
            let parameters = signature.parameters.as_ref().unwrap();
            let parameter = parameters
                .iter()
                .find(|param| param.label.starts_with("foo:"))
                .unwrap();
            assert_eq!(parameter.label, format!("foo: str = {default}"));
            assert_eq!(
                signature.documentation.as_deref(),
                Some("Rule documentation  ")
            );
            assert_eq!(
                parameter.documentation.as_deref(),
                Some("Attribute documentation  ")
            );
        }
    }

    #[test]
    fn keyword_only_after_multiplication_default() {
        let (analysis, fixture) =
            Analysis::from_single_file_fixture("def f(x=1*2, *, y=0): pass\nf(1, y=2$0)");
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let help = analysis
            .snapshot()
            .signature_help(FilePosition { file_id, pos })
            .unwrap()
            .unwrap();
        let [signature] = help.signatures.as_slice() else {
            panic!("{help:?}");
        };
        assert_eq!(signature.active_parameter, Some(1));
        let parameters = signature.parameters.as_ref().unwrap();
        let [_, keyword_only] = parameters.as_slice() else {
            panic!("{parameters:?}");
        };
        assert_eq!(keyword_only.label, "y=0");
    }

    #[test]
    fn shared_binding_for_expanded_arguments() {
        let mut mismatches = Vec::new();
        for (call, active) in [
            ("f(x=1, *[2$0])", Some(0)),
            ("f(y=1, *[2$0])", Some(0)),
            ("f(**{\"y\": 2$0})", Some(1)),
            ("f(x=1$0, 2)", Some(0)),
            ("f(x=1, 2$0)", Some(100)),
            ("f(1, y=2$0)", Some(1)),
        ] {
            let source = format!("def f(x, *, y): pass\n{call}");
            let (analysis, fixture) = Analysis::from_single_file_fixture(&source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let help = analysis
                .snapshot()
                .signature_help(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            let [signature] = help.signatures.as_slice() else {
                panic!("{help:?}");
            };
            if signature.active_parameter != active {
                mismatches.push((source, signature.active_parameter, active));
            }
        }
        assert!(mismatches.is_empty(), "{mismatches:#?}");
    }

    #[test]
    fn incomplete_call_after_comma() {
        let (analysis, fixture) = Analysis::from_single_file_fixture("def f(x, y): pass\nf(1, $0");
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let help = analysis
            .snapshot()
            .signature_help(FilePosition { file_id, pos })
            .unwrap()
            .unwrap();
        let [signature] = help.signatures.as_slice() else {
            panic!("{help:?}");
        };
        assert_eq!(signature.active_parameter, Some(1));
    }
}
