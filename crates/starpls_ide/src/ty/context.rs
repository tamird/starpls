//! Bazel's optional implementation-parameter contract, supplied to Ty's body binding.

use ruff_db::files::FileRange;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor;
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprCall;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use starpls_bazel::attr::AttributeKind;
use starpls_common::Dialect;
use starpls_hir::Db as _;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::definition::ParameterDefinitionNodeKind;
use ty_python_core::ProgramFile;
use ty_python_semantic::provided::ProvidedClass;
use ty_python_semantic::provided::ProvidedField;
use ty_python_semantic::provided::ProvidedInstanceFields;
use ty_python_semantic::types::ide_support::resolved_call_signature;
use ty_python_semantic::types::ide_support::CallSignatureDetails;
use ty_python_semantic::types::DictionaryItem;
use ty_python_semantic::types::DictionaryItems;
use ty_python_semantic::types::KnownClass;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::UnionType;
use ty_python_semantic::HasDefinition;
use ty_python_semantic::HasType;
use ty_python_semantic::ProgramEnvironment;
use ty_python_semantic::SemanticModel;

use super::factory;
use super::factory::Attribute;
use super::factory::AttributeConfiguration;
use super::factory::AttributeUse;
use super::factory::Factory;
use crate::Database;

pub(super) fn parameter_type<'db>(
    db: &'db Database,
    definition: Definition<'db>,
) -> Option<Type<'db>> {
    if !db.environment().options(db).infer_ctx_attributes {
        return None;
    }
    let file = definition.program_file(db);
    if db.starlark_file(file)?.dialect != Dialect::Bazel {
        return None;
    }
    let DefinitionKind::Parameter(parameter) = definition.kind(db) else {
        return None;
    };
    let ParameterDefinitionNodeKind::Parameter(parameter) = parameter else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let parameter = parameter.node(&parsed);
    let function = covering_node(parsed.syntax().into(), parameter.range())
        .ancestors()
        .find_map(|node| match node {
            AnyNodeRef::StmtFunctionDef(function) => Some(function),
            _ => None,
        })?;
    if function.parameters.iter_non_variadic_params().count() != 1
        || function.parameters.vararg.is_some()
        || function.parameters.kwarg.is_some()
    {
        return None;
    }
    let model = SemanticModel::new(db, file);
    let implementation = function.definition(&model);
    let environment = model.program_environment();
    let mut calls = RegistrationCalls::default();
    for statement in parsed.suite() {
        calls.visit_stmt(statement);
    }
    let mut registration = None;
    for call in calls.calls {
        let Some(signature) = resolved_call_signature(&model, call) else {
            continue;
        };
        let Some(declaration) = signature.definition else {
            continue;
        };
        let Some(factory) = factory::declaration(db, declaration) else {
            continue;
        };
        let Factory::Rule { repository } = factory else {
            continue;
        };
        // An unknown expansion could register this callback a second time;
        // uncertainty here prevents a unique schema for the entire module.
        let Some(argument) = argument(call, &signature, "implementation").ok()? else {
            continue;
        };
        let ty = argument.inferred_type(&model)?;
        if !matches!(ty, Type::FunctionLiteral(_)) {
            return None;
        }
        let target = ty
            .definition(db, &environment)
            .and_then(|ty| ty.definition())?;
        if target != implementation {
            continue;
        }
        if registration.is_some() {
            // A callback used with more than one schema has no unique body contract.
            return None;
        }
        registration = Some((call, signature, declaration, repository));
    }
    let (call, signature, declaration, repository) = registration?;
    let declarations = declaration.program_file(db);
    let common = starpls_bazel::attr::make_common_attributes();
    let common = if repository {
        common.repository
    } else {
        common.build
    };
    let attribute_type = |kind| {
        factory::attribute_value_type(
            db,
            &environment,
            declarations,
            kind,
            if repository {
                AttributeUse::RepositoryContext
            } else {
                AttributeUse::BuildContext
            },
        )
    };
    let schema = match argument(call, &signature, "attrs").ok()? {
        Some(attrs) => model.dictionary_items(attrs).unwrap_or(DictionaryItems {
            items: Box::default(),
            is_complete: false,
        }),
        None => DictionaryItems {
            items: Box::default(),
            is_complete: true,
        },
    };
    let make_class = |name, base, fields, has_dynamic_fields| {
        let class = model.provided_class_at_call(
            call,
            ProvidedClass {
                name: Name::new(name),
                bases: vec![factory::native_class(db, declarations, base)?].into_boxed_slice(),
                class_members: Box::default(),
                instance_fields: ProvidedInstanceFields {
                    fields,
                    has_dynamic_fields,
                    data: None,
                },
            },
        )?;
        class.to_instance_approximation(db, &environment)
    };
    let mut context_fields = Vec::new();
    for view in [
        View::Attr,
        View::Files,
        View::File,
        View::Executable,
        View::Outputs,
        View::SplitAttr,
    ] {
        if repository && view != View::Attr {
            continue;
        }
        let mut complete = schema.is_complete;
        let mut fields = common
            .iter()
            .filter_map(|attribute| {
                let ty = match view {
                    View::Attr => attribute_type(&attribute.r#type),
                    View::Files => dependency(&attribute.r#type)
                        .then(|| native_file_list(db, &environment, declarations))
                        .flatten(),
                    View::File => None,
                    View::Executable => None,
                    View::Outputs => None,
                    View::SplitAttr => None,
                }?;
                Some(ProvidedField {
                    name: Name::new(&attribute.name),
                    ty,
                    source: None,
                })
            })
            .collect::<Vec<_>>();
        for DictionaryItem { name, ty, source } in &schema.items {
            let attribute = ty
                .provided_data(db, &environment)
                .and_then(|data| data.downcast_ref::<Attribute>());
            let ty = if !schema.is_complete || attribute.is_none() {
                complete = false;
                if view != View::Attr {
                    continue;
                }
                Type::unknown()
            } else if repository {
                attribute_type(&attribute?.kind)?
            } else {
                let Some(ty) = field_type(db, &environment, declarations, view, attribute?) else {
                    continue;
                };
                ty
            };
            insert_field(
                &mut fields,
                ProvidedField {
                    name: name.clone(),
                    ty,
                    source: Some(FileRange::new(file.file(db), *source)),
                },
            );
        }
        if view == View::Outputs {
            let output = factory::native_class(db, declarations, "File")?
                .to_instance_approximation(db, &environment)?;
            if let Some(outputs) = argument(call, &signature, "outputs").ok()? {
                match model.dictionary_items(outputs) {
                    Some(DictionaryItems { items, is_complete }) => {
                        complete &= is_complete;
                        for DictionaryItem {
                            name,
                            ty: _,
                            source,
                        } in items
                        {
                            insert_field(
                                &mut fields,
                                ProvidedField {
                                    name,
                                    ty: if is_complete { output } else { Type::unknown() },
                                    source: Some(FileRange::new(file.file(db), source)),
                                },
                            );
                        }
                    }
                    None => complete = false,
                }
            }
            for option in ["executable", "test"] {
                let Some(option) = argument(call, &signature, option).ok()? else {
                    continue;
                };
                match option.inferred_type(&model).and_then(Type::as_bool_literal) {
                    Some(true) => insert_field(
                        &mut fields,
                        ProvidedField {
                            name: Name::new("executable"),
                            ty: output,
                            source: None,
                        },
                    ),
                    Some(false) => {}
                    None => complete = false,
                }
            }
        }
        let ty = make_class(view.name(), "struct", fields.into_boxed_slice(), !complete)?;
        context_fields.push(ProvidedField {
            name: Name::new(view.name()),
            ty,
            source: None,
        });
    }
    let name = if repository { "repository_ctx" } else { "ctx" };
    make_class(name, name, context_fields.into_boxed_slice(), false)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Attr,
    Files,
    File,
    Executable,
    Outputs,
    SplitAttr,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            Self::Attr => "attr",
            Self::Files => "files",
            Self::File => "file",
            Self::Executable => "executable",
            Self::Outputs => "outputs",
            Self::SplitAttr => "split_attr",
        }
    }
}

fn insert_field<'db>(fields: &mut Vec<ProvidedField<'db>>, field: ProvidedField<'db>) {
    if let Some(existing) = fields
        .iter_mut()
        .find(|existing| existing.name == field.name)
    {
        *existing = field;
    } else {
        fields.push(field);
    }
}

fn dependency(kind: &AttributeKind) -> bool {
    matches!(
        kind,
        AttributeKind::Label
            | AttributeKind::LabelList
            | AttributeKind::LabelKeyedStringDict
            | AttributeKind::StringKeyedLabelDict
    )
}

fn native_file_list<'db>(
    db: &'db Database,
    environment: &ProgramEnvironment<'db>,
    declarations: ProgramFile<'db>,
) -> Option<Type<'db>> {
    let file = factory::native_class(db, declarations, "File")?
        .to_instance_approximation(db, environment)?;
    Some(KnownClass::List.to_specialized_instance(db, environment, &[file]))
}

fn field_type<'db>(
    db: &'db Database,
    environment: &ProgramEnvironment<'db>,
    declarations: ProgramFile<'db>,
    view: View,
    attribute: &Attribute,
) -> Option<Type<'db>> {
    let native = |name| {
        factory::native_class(db, declarations, name)?.to_instance_approximation(db, environment)
    };
    let list = |ty| KnownClass::List.to_specialized_instance(db, environment, &[ty]);
    let dict =
        |key, value| KnownClass::Dict.to_specialized_instance(db, environment, &[key, value]);
    let optional =
        |ty| UnionType::from_elements(db, environment, [ty, Type::none(db, environment)]);
    let optional_file = |enabled| {
        if attribute.kind != AttributeKind::Label {
            return None;
        }
        match enabled {
            Some(true) => Some(optional(native("File")?)),
            Some(false) => None,
            None => Some(Type::unknown()),
        }
    };
    match view {
        View::Attr => {
            if attribute.kind == AttributeKind::Label {
                match attribute.configuration {
                    AttributeConfiguration::Starlark => return Some(list(native("Target")?)),
                    AttributeConfiguration::Unknown => return Some(Type::unknown()),
                    AttributeConfiguration::Ordinary => {}
                }
            }
            factory::attribute_value_type(
                db,
                environment,
                declarations,
                &attribute.kind,
                AttributeUse::BuildContext,
            )
        }
        View::Files => dependency(&attribute.kind)
            .then(|| native_file_list(db, environment, declarations))
            .flatten(),
        View::File => optional_file(attribute.single_file),
        View::Executable => optional_file(attribute.executable),
        View::Outputs => match attribute.kind {
            AttributeKind::Output => Some(optional(native("File")?)),
            AttributeKind::OutputList => Some(list(native("File")?)),
            _ => None,
        },
        View::SplitAttr => {
            if !dependency(&attribute.kind) {
                return None;
            }
            match attribute.configuration {
                AttributeConfiguration::Ordinary => return None,
                AttributeConfiguration::Unknown => return Some(Type::unknown()),
                AttributeConfiguration::Starlark => {}
            }
            let string = KnownClass::Str.to_instance(db, environment);
            let target = native("Target")?;
            let value = match attribute.kind {
                AttributeKind::Label => target,
                AttributeKind::LabelList => list(target),
                AttributeKind::LabelKeyedStringDict => list(target),
                AttributeKind::StringKeyedLabelDict => dict(string, target),
                _ => unreachable!("split attributes are dependencies"),
            };
            Some(dict(optional(string), value))
        }
    }
}

/// Select a definite source argument using Ty's existing binding. Expanded
/// arguments can supply or overwrite either field, so they leave this policy gradual.
fn argument<'a>(
    call: &'a ExprCall,
    signature: &CallSignatureDetails<'_>,
    name: &str,
) -> Result<Option<&'a Expr>, ()> {
    let parameter = signature
        .parameters
        .iter()
        .position(|parameter| parameter.name == name)
        .ok_or(())?;
    let mut selected = None;
    for (index, argument) in call.arguments.iter_source_order().enumerate() {
        let expression = match argument {
            ArgOrKeyword::Arg(expression) => {
                if matches!(expression, Expr::Starred(_)) {
                    return Err(());
                }
                expression
            }
            ArgOrKeyword::Keyword(keyword) => {
                keyword.arg.as_ref().ok_or(())?;
                &keyword.value
            }
        };
        if signature.argument_to_displayed_parameter_mapping.get(index) == Some(&Some(parameter)) {
            if selected.is_some() {
                return Err(());
            }
            selected = Some(expression);
        }
    }
    Ok(selected)
}

#[derive(Default)]
struct RegistrationCalls<'a> {
    calls: Vec<&'a ExprCall>,
}

impl<'a> Visitor<'a> for RegistrationCalls<'a> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        // Only module-executed registrations establish this opt-in contract.
        // Inferring deferred bodies here could re-enter the parameter being supplied.
        if !matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
            visitor::walk_stmt(self, statement);
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Lambda(_) => {}
            Expr::Call(call) => {
                // Skip calls that cannot supply an implementation argument.
                // The shared matcher still decides every candidate's meaning.
                if !call.arguments.args.is_empty()
                    || call.arguments.keywords.iter().any(|keyword| {
                        keyword
                            .arg
                            .as_ref()
                            .is_none_or(|name| name.as_str() == "implementation")
                    })
                {
                    self.calls.push(call);
                }
                visitor::walk_expr(self, expression);
            }
            _ => visitor::walk_expr(self, expression),
        }
    }
}

#[cfg(test)]
mod tests {
    use salsa::Setter;
    use starpls_hir::Db as _;

    use crate::Analysis;
    use crate::FilePosition;
    use crate::LocationLink;

    fn enable_context(analysis: &mut Analysis) {
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        analysis
            .db
            .environment()
            .set_options(&mut analysis.db)
            .to(crate::InferenceOptions {
                infer_ctx_attributes: true,
                ..Default::default()
            });
    }

    fn check_field(
        snapshot: &crate::AnalysisSnapshot,
        file: starpls_common::File,
        source: &str,
        expression: &str,
        expected: &str,
        key: &str,
    ) {
        let pos = (source.find(expression).unwrap() + expression.len() - 1) as u32;
        let position = FilePosition {
            file_id: file,
            pos: pos.into(),
        };
        let hover = snapshot.hover(position.clone()).unwrap().unwrap();
        assert!(
            hover.contents.value.contains(&format!(": {expected}\n")),
            "{expression}: {}",
            hover.contents.value
        );
        if !key.is_empty() {
            let locations = snapshot.goto_definition(position, false).unwrap().unwrap();
            let [LocationLink::Local {
                target_file_id,
                target_selection_range,
                origin_selection_range: _,
                target_range: _,
            }] = locations.as_slice()
            else {
                panic!("{locations:?}");
            };
            assert_eq!(*target_file_id, file.source);
            assert_eq!(&source[*target_selection_range], key);
        }
    }

    #[test]
    fn context_views_share_attribute_origins_and_transition_types() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "transitions.bzl", "def implementation(settings, attrs): return {}\nfactory = transition\nchange = factory(implementation=implementation, inputs=[], outputs=[])\n");
        let source = r#"load("transitions.bzl", imported="change")
def implementation(ctx):
    ctx.attr.dep
    ctx.attr.out
    ctx.attr.outs
    ctx.attr.split
    ctx.files.srcs
    ctx.files.keyed
    ctx.files.named
    ctx.file.empty
    ctx.file.extension
    ctx.file.false_value
    ctx.executable.tool
    ctx.outputs.out
    ctx.outputs.outs
    ctx.outputs.implicit
    ctx.outputs.executable
    ctx.split_attr.split
    ctx.split_attr.split_list
    ctx.split_attr.split_keyed
    ctx.split_attr.split_named
    ctx.actions.run
example = rule(implementation=implementation, executable=True, outputs={"implicit": "%{name}.txt"}, attrs={
    "dep": attr.label(),
    "out": attr.output(),
    "outs": attr.output_list(),
    "srcs": attr.label_list(),
    "keyed": attr.label_keyed_string_dict(),
    "named": attr.string_keyed_label_dict(),
    "empty": attr.label(allow_single_file=[]),
    "extension": attr.label(allow_single_file=[".txt"]),
    "false_value": attr.label(allow_single_file=False),
    "tool": attr.label(executable=True, cfg="exec"),
    "split": attr.label(cfg=imported),
    "split_list": attr.label_list(cfg=imported),
    "split_keyed": attr.label_keyed_string_dict(cfg=imported),
    "split_named": attr.string_keyed_label_dict(cfg=imported),
})
"#;
        let file = fixture.add_file(&mut analysis.db, "defs.bzl", source);
        loader.add_files_from_fixture(&fixture);
        enable_context(&mut analysis);
        let snapshot = analysis.snapshot();
        for (expression, expected, key) in [
            ("ctx.attr.dep", "Target | None", "\"dep\""),
            ("ctx.attr.out", "Label | None", "\"out\""),
            ("ctx.attr.outs", "list[Label]", "\"outs\""),
            ("ctx.attr.split", "list[Target]", "\"split\""),
            ("ctx.files.srcs", "list[File]", "\"srcs\""),
            ("ctx.files.keyed", "list[File]", "\"keyed\""),
            ("ctx.files.named", "list[File]", "\"named\""),
            ("ctx.file.empty", "File | None", "\"empty\""),
            ("ctx.file.extension", "File | None", "\"extension\""),
            ("ctx.file.false_value", "File | None", "\"false_value\""),
            ("ctx.executable.tool", "File | None", "\"tool\""),
            ("ctx.outputs.out", "File | None", "\"out\""),
            ("ctx.outputs.outs", "list[File]", "\"outs\""),
            ("ctx.outputs.implicit", "File", "\"implicit\""),
            ("ctx.outputs.executable", "File", ""),
            (
                "ctx.split_attr.split",
                "dict[str | None, Target]",
                "\"split\"",
            ),
            (
                "ctx.split_attr.split_list",
                "dict[str | None, list[Target]]",
                "\"split_list\"",
            ),
            (
                "ctx.split_attr.split_keyed",
                "dict[str | None, list[Target]]",
                "\"split_keyed\"",
            ),
            (
                "ctx.split_attr.split_named",
                "dict[str | None, dict[str, Target]]",
                "\"split_named\"",
            ),
        ] {
            check_field(&snapshot, file, source, expression, expected, key);
        }
        let pos = (source.find("ctx.actions.run").unwrap() + "ctx.actions.".len()) as u32;
        let hover = snapshot
            .hover(FilePosition {
                file_id: file,
                pos: pos.into(),
            })
            .unwrap()
            .unwrap();
        assert!(
            hover.contents.value.contains("(method)"),
            "{}",
            hover.contents.value
        );
    }

    #[test]
    fn uncertain_flags_keep_unrelated_context_fields_precise() {
        let source = r#"def unknown(): pass
def implementation(ctx):
    ctx.attr.count
    ctx.attr.dep
    ctx.files.dep
    ctx.file.dep
    ctx.executable.dep
    ctx.split_attr.dep
    ctx.attr.native
    ctx.attr.none_transition
    ctx.outputs.dynamic
    ctx.file.
example = rule(implementation=implementation, outputs=unknown(), executable=unknown(), attrs={
    "count": attr.int(),
    "dep": attr.label(allow_single_file=unknown(), executable=unknown(), cfg=unknown()),
    "native": attr.label(cfg=config.target()),
    "none_transition": attr.label(cfg=config.none()),
    "absent": attr.label(allow_single_file=None, executable=False),
})
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let file = fixture.main_file();
        let snapshot = analysis.snapshot();
        for (expression, expected) in [
            ("ctx.attr.count", "int"),
            ("ctx.attr.dep", "Unknown"),
            ("ctx.files.dep", "list[File]"),
            ("ctx.file.dep", "Unknown"),
            ("ctx.executable.dep", "Unknown"),
            ("ctx.split_attr.dep", "Unknown"),
            ("ctx.attr.native", "Unknown"),
            ("ctx.attr.none_transition", "Unknown"),
            ("ctx.outputs.dynamic", "Unknown"),
        ] {
            check_field(&snapshot, file, source, expression, expected, "");
        }
        let pos = (source.rfind("ctx.file.").unwrap() + "ctx.file.".len()) as u32;
        let completions = snapshot
            .completions(
                FilePosition {
                    file_id: file,
                    pos: pos.into(),
                },
                None,
            )
            .unwrap()
            .unwrap();
        assert!(completions
            .iter()
            .any(|completion| completion.label == "dep"));
        assert!(!completions
            .iter()
            .any(|completion| completion.label == "absent"));
    }

    #[test]
    fn repository_attributes_keep_labels_and_native_methods() {
        let source = r#"def implementation(ctx):
    ctx.attr.dep
    ctx.download
example = repository_rule(implementation=implementation, attrs={"dep": attr.label()})
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let file = fixture.main_file();
        let snapshot = analysis.snapshot();
        check_field(
            &snapshot,
            file,
            source,
            "ctx.attr.dep",
            "Label | None",
            "\"dep\"",
        );
        let hover = snapshot
            .hover(FilePosition {
                file_id: file,
                pos: (source.find("download").unwrap() as u32).into(),
            })
            .unwrap()
            .unwrap();
        assert!(hover.contents.value.contains("(method)"));
    }
}
