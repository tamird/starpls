use std::fmt::Write;

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
use starpls_common::parsed_module;
use starpls_hir::Semantics;
use starpls_syntax::source::expr_range;

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
    let sema = Semantics::new(db);
    let file = file_id;
    let parsed = parsed_module(db, file).load(db);
    let source = file.contents(db);
    let (expr, active_arg) = call_at_cursor(
        parsed.syntax(),
        parsed.tokens(),
        u32::from(pos).into(),
        TextSize::of(&*source),
    )?;
    let func = sema.resolve_call_expr(file, expr)?;
    let params = func.params();
    let param_labels: Vec<String> = params
        .iter()
        .map(|(param, ty)| {
            let mut s = String::new();
            if param.is_args_list() {
                s.push('*');
            } else if param.is_kwargs_dict() {
                s.push_str("**");
            }

            match param.name() {
                Some(name) if !name.is_missing() && !name.as_str().is_empty() => {
                    s.push_str(name.as_str());

                    let ty = if param.is_args_list() {
                        ty.variable_tuple_element_ty()
                    } else if param.is_kwargs_dict() {
                        ty.dict_value_ty()
                    } else {
                        ty.clone().into()
                    };

                    match ty {
                        Some(ty) if !ty.is_unknown() => {
                            let _ = write!(&mut s, ": {}", ty);
                        }
                        _ => {}
                    }

                    match param.default_value() {
                        Some(default_value) if !default_value.is_empty() => {
                            s.push_str(" = ");
                            s.push_str(&default_value);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }

            s
        })
        .collect();

    // Construct the labels for the function signature.
    // TODO(withered-magic): Some of this logic is duplicated from the `DisplayWithDb` implementation on `TyKind`.
    let mut label = String::new();
    label.push_str("def ");
    label.push_str(func.name().as_str());
    label.push('(');

    let is_rule_or_tag = func.is_rule() || func.is_tag() || func.is_macro();
    if is_rule_or_tag {
        label.push('*');
    }

    for (index, param_label) in param_labels.iter().enumerate() {
        if index > 0 || is_rule_or_tag {
            label.push_str(", ");
        }
        label.push_str(param_label);
    }

    label.push_str(") -> ");
    let _ = write!(&mut label, "{}", func.ret_ty());

    let active_parameter = sema
        .resolve_call_expr_active_param(file, expr, active_arg)
        .unwrap_or(DEFAULT_ACTIVE_PARAMETER_INDEX); // active_parameter defaults to 0, so we just add a crazy high value here to avoid a false positive

    Some(SignatureHelp {
        signatures: vec![SignatureInfo {
            label,
            documentation: func.doc().map(|doc| unindent_doc(&doc)),
            parameters: Some(
                params
                    .into_iter()
                    .zip(param_labels.into_iter())
                    .map(|((param, _), label)| ParameterInfo {
                        label,
                        documentation: param.doc().map(|doc| unindent_doc(&doc)),
                    })
                    .collect(),
            ),
            active_parameter: Some(active_parameter),
        }],
    })
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
                format!("r = rule(attrs = {{\"foo\": attr.string(default = {default})}})"),
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
            assert_eq!(parameter.label, format!("foo: string = {default}"));
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
        assert_eq!(signature.active_parameter, Some(2));
        let parameters = signature.parameters.as_ref().unwrap();
        let [_, _, keyword_only] = parameters.as_slice() else {
            panic!("{parameters:?}");
        };
        assert_eq!(keyword_only.label, "y");
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
