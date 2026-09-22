use ruff_text_size::Ranged;
use ty_ide::call_at_offset;
use ty_ide::Docstring;
use ty_ide::DocstringFragment;
use ty_ide::MarkupKind;
use ty_python_semantic::types::ide_support::call_signature_details;
use ty_python_semantic::types::ide_support::CallSignatureDetails;
use ty_python_semantic::types::ide_support::CallSignatureParameter;
use ty_python_semantic::types::Type;
use ty_python_semantic::HasType;
use ty_python_semantic::SemanticModel;

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
    let (expr, active_arg) = call_at_offset(&parsed, &source, u32::from(pos).into())?;
    let model = SemanticModel::new(db, program_file);
    model.scope(expr.into())?;
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
                .or(source_documentation.as_deref())
                .map(|doc| Docstring::new(doc.to_owned()));
            let parameter_documentation = documentation
                .as_ref()
                .map(Docstring::parameter_documentation)
                .unwrap_or_default();
            let name = if constructor || matches!(callee_type, Some(Type::NominalInstance(_))) {
                None
            } else {
                definition.and_then(|definition| definition.name(db))
            };
            let name = name.as_deref().unwrap_or(&source[expr.func.range()]);
            SignatureInfo {
                label: format!("def {name}{label}"),
                documentation: documentation.map(|doc| doc.render(MarkupKind::Markdown)),
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
                                        (parameter.as_str() == name).then(|| {
                                            Docstring::new(text.to_string())
                                                .render(MarkupKind::Markdown)
                                        })
                                    })
                                })
                                .or_else(|| {
                                    parameter_documentation.get(&name).map(|doc| {
                                        DocstringFragment::new(doc).render(MarkupKind::Markdown)
                                    })
                                });
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

#[cfg(test)]
mod tests {
    use crate::Analysis;
    use crate::FilePosition;

    #[test]
    fn call_and_argument_cursor_boundaries() {
        for (call, expected) in [
            ("f($0)", Some(("f", 0))),
            ("f(0$0)", Some(("f", 0))),
            ("f(0,$0)", Some(("f", 1))),
            ("f(0,$0 )", Some(("f", 1))),
            ("f(0,,$0)", Some(("f", 100))),
            ("f(lambda x,$0 y: x, 0)", Some(("f", 0))),
            ("f((0,$0 1), 0)", Some(("f", 0))),
            ("f((0)$0, 1)", Some(("f", 0))),
            ("f((0)$0 , 1)", Some(("f", 0))),
            ("f((0),$0 1)", Some(("f", 1))),
            ("f(0, # type: in$0t\n 1)", Some(("f", 1))),
            ("f(x=(0,$0 1), y=0)", Some(("f", 0))),
            ("f(g() $0", Some(("f", 0))),
            ("f(0, g$0())", Some(("g", 100))),
            ("f(0, g()$0)", Some(("f", 1))),
            ("f$0()", Some(("f", 0))),
            ("f()$0", None),
            ("f()$0 ", None),
            ("f()\n$0", None),
            ("def nested():\n    f(0, $0)", Some(("f", 1))),
            ("while True:\n    f($0)", None),
        ] {
            let source = format!("def f(x, y): pass\ndef g(): pass\n{call}");
            let (analysis, fixture) = Analysis::from_single_file_fixture(&source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let help = analysis
                .snapshot()
                .signature_help(FilePosition { file_id, pos })
                .unwrap();
            let actual = help.as_ref().map(|help| {
                let [signature] = help.signatures.as_slice() else {
                    panic!("{call}: {help:?}");
                };
                let (name, _) = signature.label.split_once('(').unwrap();
                (
                    name.strip_prefix("def ").unwrap(),
                    signature.active_parameter,
                )
            });
            let expected = expected.map(|(name, active)| (name, Some(active)));
            assert_eq!(actual, expected, "{call}: {help:?}");
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
        let caller = "load(\"defs.bzl\", \"r\", \"fallback\")\nr(fo$0o = \"\")\nrules = struct(r=r)\nrules.r\nfallback()";
        fixture.add_file(&mut analysis.db, "main.bzl", caller);
        loader.add_files_from_fixture(&fixture);
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        for default in ["\"é\"", "(\"日本語\")", "\"é\""] {
            analysis.update_file(
                dependency,
                format!(
                    r#"r = rule(doc = """Rule documentation.

Args:
    foo: General parameter documentation.
""", attrs = {{"foo": attr.string(doc = """Attribute documentation.
    More detail.

    ```python
    if True:
        value = 1
    ```
""", default = {default})}})
def unknown(attrs):
    return rule(attrs=attrs)
fallback = unknown({{}})
"#
                ),
            );
            let rule_hover = analysis
                .snapshot()
                .hover(FilePosition {
                    file_id,
                    pos: (u32::from(pos) - 3).into(),
                })
                .unwrap()
                .unwrap();
            assert!(rule_hover.contents.value.contains("(function)"));
            assert!(rule_hover
                .contents
                .value
                .contains(&format!("foo: str = {default}")));
            assert!(rule_hover.contents.value.contains("Rule documentation."));
            let field_hover = analysis
                .snapshot()
                .hover(FilePosition {
                    file_id,
                    pos: u32::try_from(caller.replace("$0", "").rfind("rules.r").unwrap() + 6)
                        .unwrap()
                        .into(),
                })
                .unwrap()
                .unwrap();
            assert!(field_hover
                .contents
                .value
                .contains(&format!("foo: str = {default}")));
            let fallback_help = analysis
                .snapshot()
                .signature_help(FilePosition {
                    file_id,
                    pos: u32::try_from(caller.replace("$0", "").rfind("fallback(").unwrap() + 9)
                        .unwrap()
                        .into(),
                })
                .unwrap()
                .unwrap();
            assert!(fallback_help.signatures[0]
                .label
                .starts_with("def fallback("));
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
            assert!(signature
                .documentation
                .as_ref()
                .unwrap()
                .contains("General parameter documentation."));
            assert_eq!(
                parameter.documentation.as_deref(),
                Some("Attribute documentation.  \nMore detail.  \n  \n```python\nif True:\n    value = 1\n```")
            );
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert!(
                hover
                    .contents
                    .value
                    .contains(parameter.documentation.as_ref().unwrap()),
                "{}",
                hover.contents.value
            );
            assert!(
                !hover
                    .contents
                    .value
                    .contains("General parameter documentation."),
                "{}",
                hover.contents.value
            );
        }
    }

    #[test]
    fn inherited_macro_parameters_follow_loaded_declarations() {
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
        let caller = r#"load("defs.bzl", "base", "implementation")
child = macro(implementation=implementation, inherit_attrs=base, attrs={
    "overridden": attr.int(default=3, doc="Own documentation."),
    "removed": None,
})
child(name="ok", original$0="", overridden=3)
"#;
        fixture.add_file(&mut analysis.db, "main.bzl", caller);
        loader.add_files_from_fixture(&fixture);
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        for (kind, ty, default) in [("string", "str", "'parent'"), ("int", "int", "42")] {
            let source = format!(
                r#"# {kind}
def implementation(**kwargs):
    pass
base = rule(implementation=implementation, attrs={{
    "original": attr.{kind}(default={default}, doc="Inherited documentation."),
    "overridden": attr.string(),
    "removed": attr.string(),
}})
"#
            );
            analysis.update_file(dependency, source.clone());
            let snapshot = analysis.snapshot();
            let help = snapshot
                .signature_help(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            let [signature] = help.signatures.as_slice() else {
                panic!("{help:?}");
            };
            assert!(!signature.label.contains("removed"), "{signature:?}");
            assert!(!signature.label.contains("kwargs"), "{signature:?}");
            let parameters = signature.parameters.as_ref().unwrap();
            for (name, expected, doc) in [
                (
                    "original:",
                    format!("original: {ty} | None = None"),
                    "Inherited documentation.",
                ),
                (
                    "overridden:",
                    "overridden: int | None = 3".to_owned(),
                    "Own documentation.",
                ),
            ] {
                let parameter = parameters
                    .iter()
                    .find(|parameter| parameter.label.starts_with(name))
                    .unwrap();
                assert_eq!(parameter.label, expected);
                assert_eq!(parameter.documentation.as_deref(), Some(doc));
            }
            let locations = snapshot
                .goto_definition(FilePosition { file_id, pos }, false)
                .unwrap()
                .unwrap();
            let [crate::LocationLink::Local {
                target_file_id,
                target_selection_range,
                ..
            }] = locations.as_slice()
            else {
                panic!("{locations:?}");
            };
            assert_eq!(*target_file_id, dependency.source);
            assert_eq!(
                &source[usize::from(target_selection_range.start())
                    ..usize::from(target_selection_range.end())],
                "\"original\""
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
