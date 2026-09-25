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
use ty_python_semantic::provided::ProvidedFieldImplication;
use ty_python_semantic::provided::ProvidedInstanceFields;
use ty_python_semantic::types::ide_support::resolved_call_signature;
use ty_python_semantic::types::ide_support::CallSignatureDetails;
use ty_python_semantic::types::DictionaryExtraItems;
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
use super::factory::RuleAttributeData;
use crate::Database;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContextKind {
    Build,
    Repository,
    Aspect,
}

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
        if !matches!(
            factory,
            Factory::Rule { repository: _ } | Factory::Aspect | Factory::Macro
        ) {
            continue;
        }
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
        registration = Some((call, signature, declaration, factory));
    }
    let (call, signature, declaration, factory) = registration?;
    let declarations = declaration.program_file(db);
    let context_kind = match factory {
        Factory::Macro => {
            let result = call.inferred_type(&model)?;
            let data = result.provided_data(db, &environment)?;
            let data = data.downcast_ref::<factory::RuleData>()?;
            return data.parameter_type(db, &environment, declarations, parameter.name().as_str());
        }
        Factory::Rule { repository } => {
            if repository {
                ContextKind::Repository
            } else {
                ContextKind::Build
            }
        }
        Factory::Aspect => ContextKind::Aspect,
        Factory::Attribute(_) => return None,
        Factory::BuildSetting(_) => return None,
        Factory::Struct => return None,
        Factory::Provider => return None,
        Factory::Transition => return None,
    };
    let parameter_count = if context_kind == ContextKind::Aspect {
        2
    } else {
        1
    };
    if function.parameters.iter_non_variadic_params().count() != parameter_count
        || function.parameters.vararg.is_some()
        || function.parameters.kwarg.is_some()
    {
        return None;
    }
    if context_kind == ContextKind::Aspect {
        if !function.parameters.kwonlyargs.is_empty() {
            return None;
        }
        if function.parameters.index(parameter.name().as_str()) == Some(0) {
            let target = factory::native_class(db, declarations, "Target")?;
            return target.to_instance_approximation(db, &environment);
        }
    }
    let result = call.inferred_type(&model)?;
    let rule = result
        .provided_data(db, &environment)
        .and_then(|data| data.downcast_ref::<factory::RuleData>());
    let aspect_attributes;
    let (attributes, complete) = if context_kind == ContextKind::Aspect {
        let mapping = match argument(call, &signature, "attrs").ok()? {
            Some(attrs) => model.dictionary_items(attrs).unwrap_or(DictionaryItems {
                items: Box::default(),
                extra_items: DictionaryExtraItems::Value(Type::unknown()),
            }),
            None => DictionaryItems {
                items: Box::default(),
                extra_items: DictionaryExtraItems::Closed,
            },
        };
        let is_complete = mapping.is_complete();
        let DictionaryItems {
            items,
            extra_items: _,
        } = mapping;
        let is_complete = is_complete && items.iter().all(DictionaryItem::is_required);
        aspect_attributes = items
            .iter()
            .filter(|item| item.is_required())
            .map(
                |DictionaryItem {
                     name,
                     ty,
                     source,
                     kind: _,
                 }| RuleAttributeData {
                    name: name.clone(),
                    descriptor: if is_complete {
                        ty.provided_data(db, &environment)
                            .and_then(|data| data.downcast_ref::<Attribute>())
                            .cloned()
                    } else {
                        None
                    },
                    source: Some(FileRange::new(file.file(db), *source)),
                },
            )
            .collect::<Vec<_>>();
        (aspect_attributes.as_slice(), is_complete)
    } else {
        let rule = rule?;
        (rule.attributes.as_ref(), rule.complete)
    };
    let make_class = |name, base, fields, has_dynamic_fields, implications| {
        let class = model.provided_class_at_call(
            call,
            ProvidedClass {
                name: Name::new(name),
                bases: vec![base].into_boxed_slice(),
                class_members: Box::default(),
                instance_fields: ProvidedInstanceFields {
                    fields,
                    has_dynamic_fields,
                    implications,
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
        if context_kind == ContextKind::Repository && view != View::Attr {
            continue;
        }
        if context_kind == ContextKind::Aspect && matches!(view, View::Outputs | View::SplitAttr) {
            continue;
        }
        let mut complete = complete;
        let mut fields = Vec::new();
        for RuleAttributeData {
            name,
            descriptor,
            source,
        } in attributes
        {
            // Bazel accepts hints at rule call sites but hides them from ctx.attr.
            if context_kind == ContextKind::Build && name == "aspect_hints" {
                continue;
            }
            let ty = if let Some(attribute) = descriptor {
                let finite = if view == View::Attr {
                    attribute.finite_value_type(db, &environment)
                } else {
                    None
                };
                if let Some(ty) = finite {
                    ty
                } else if context_kind == ContextKind::Repository {
                    if attribute.kind == AttributeKind::Label && attribute.has_non_none_value() {
                        factory::native_class(db, declarations, "Label")?
                            .to_instance_approximation(db, &environment)?
                    } else {
                        factory::attribute_value_type(
                            db,
                            &environment,
                            declarations,
                            &attribute.kind,
                            AttributeUse::RepositoryContext,
                        )?
                    }
                } else {
                    let Some(ty) = field_type(db, &environment, declarations, view, attribute)
                    else {
                        continue;
                    };
                    ty
                }
            } else {
                if view != View::Attr {
                    continue;
                }
                Type::unknown()
            };
            fields.push(ProvidedField {
                name: name.clone(),
                ty,
                source: *source,
            });
        }
        if view == View::Outputs {
            let output = factory::native_class(db, declarations, "File")?
                .to_instance_approximation(db, &environment)?;
            if let Some(outputs) = argument(call, &signature, "outputs").ok()? {
                match model.dictionary_items(outputs) {
                    Some(mapping) => {
                        let is_complete = mapping.is_complete();
                        let DictionaryItems {
                            items,
                            extra_items: _,
                        } = mapping;
                        let is_complete =
                            is_complete && items.iter().all(DictionaryItem::is_required);
                        complete &= is_complete;
                        for DictionaryItem {
                            name,
                            ty: _,
                            source,
                            kind: _,
                        } in IntoIterator::into_iter(items).filter(DictionaryItem::is_required)
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
            match rule?.executable {
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
        let ty = make_class(
            view.name(),
            factory::native_class(db, declarations, "struct")?,
            fields.into_boxed_slice(),
            !complete,
            Box::default(),
        )?;
        context_fields.push(ProvidedField {
            name: Name::new(view.name()),
            ty,
            source: None,
        });
    }
    let name = if context_kind == ContextKind::Repository {
        "repository_ctx"
    } else {
        "ctx"
    };
    let base = match rule.and_then(|rule| rule.build_setting) {
        Some(setting_kind) => factory::specialized_native_class(
            db,
            declarations,
            name,
            setting_kind.value_type(db, &environment),
        )?,
        None => factory::native_class(db, declarations, name)?,
    };
    let mut implications = Vec::new();
    // Bazel validates single-file and executable prerequisites before invoking the rule.
    // A present target therefore has a File in each enabled view of that attribute.
    if context_kind != ContextKind::Repository {
        for RuleAttributeData {
            name,
            descriptor,
            source: _,
        } in attributes
        {
            let Some(attribute) = descriptor else {
                continue;
            };
            if attribute.kind != AttributeKind::Label
                || attribute.configuration != AttributeConfiguration::Ordinary
                || attribute.has_non_none_value()
            {
                continue;
            }
            for (view, enabled) in [
                (View::File, attribute.single_file),
                (View::Executable, attribute.executable),
            ] {
                if enabled != Some(true) {
                    continue;
                }
                let file_class = factory::native_class(db, declarations, "File")?;
                let ty = file_class.to_instance_approximation(db, &environment)?;
                implications.push(ProvidedFieldImplication {
                    guard: Box::from([Name::new("attr"), name.clone()]),
                    target: Box::from([Name::new(view.name()), name.clone()]),
                    ty,
                });
            }
        }
    }
    make_class(
        name,
        base,
        context_fields.into_boxed_slice(),
        false,
        implications.into_boxed_slice(),
    )
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
    let present = |ty| {
        if attribute.has_non_none_value() {
            ty
        } else {
            optional(ty)
        }
    };
    let optional_file = |enabled| {
        if attribute.kind != AttributeKind::Label {
            return None;
        }
        match enabled {
            Some(true) => Some(present(native("File")?)),
            Some(false) => None,
            None => Some(Type::unknown()),
        }
    };
    let target = || {
        // Successful executable/single-artifact prerequisites exclude targets
        // without FilesToRunProvider. Other dependencies can be environment groups.
        let provider = if attribute.executable == Some(true) {
            factory::specialized_native_instance(
                db,
                environment,
                declarations,
                "FilesToRunProvider",
                native("File")?,
            )?
        } else {
            native("FilesToRunProvider")?
        };
        let target = factory::specialized_native_instance(
            db,
            environment,
            declarations,
            "Target",
            provider,
        )?;
        if attribute.executable == Some(true) || attribute.single_file == Some(true) {
            Some(target)
        } else {
            let group = factory::specialized_native_instance(
                db,
                environment,
                declarations,
                "Target",
                Type::none(db, environment),
            )?;
            Some(UnionType::from_elements(db, environment, [target, group]))
        }
    };
    match view {
        View::Attr => {
            if attribute.kind == AttributeKind::Label {
                match attribute.configuration {
                    AttributeConfiguration::Starlark => return Some(list(target()?)),
                    AttributeConfiguration::Unknown => return Some(Type::unknown()),
                    AttributeConfiguration::Ordinary => return Some(present(target()?)),
                }
            }
            if attribute.kind == AttributeKind::Output {
                return Some(present(native("Label")?));
            }
            factory::attribute_value_type(
                db,
                environment,
                declarations,
                &attribute.kind,
                AttributeUse::BuildContext(target()?),
            )
        }
        View::Files => dependency(&attribute.kind)
            .then(|| native_file_list(db, environment, declarations))
            .flatten(),
        View::File => optional_file(attribute.single_file),
        View::Executable => optional_file(attribute.executable),
        View::Outputs => match attribute.kind {
            AttributeKind::Output => Some(present(native("File")?)),
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
            let target = target()?;
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
    use ruff_python_ast::Stmt;
    use salsa::Setter;
    use starpls_hir::Db as _;
    use ty_python_semantic::HasType;
    use ty_python_semantic::SemanticModel;

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

    #[test]
    fn successful_provider_lookup_establishes_files_to_run() {
        let source = r#"Info = provider(fields=["message"])
def implementation(ctx):
    target = ctx.attr.dep
    info = target[Info]
    print(info)
    executable = target[DefaultInfo].files_to_run.executable # type: File | None
    if executable != None:
        print(executable.path)
    for dependency in ctx.attr.deps:
        for artifact in dependency[OutputGroupInfo]["exe"].to_list():
            print(artifact.path)
        manifest = dependency[DefaultInfo].files_to_run.runfiles_manifest # type: File | None
        if manifest != None:
            print(manifest.path)
    return []
example = rule(implementation=implementation, attrs={
    "dep": attr.label(mandatory=True),
    "deps": attr.label_list(),
})
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let file = fixture.main_file();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        for (original, replacement, expression) in [
            (
                "    if executable != None:\n        print(executable.path)",
                "    print(executable.path)",
                "executable.path",
            ),
            (
                "        if manifest != None:\n            print(manifest.path)",
                "        print(manifest.path)",
                "manifest.path",
            ),
            (
                "info = target[Info]",
                "info = target[DefaultInfo]",
                "target[DefaultInfo].files_to_run.executable",
            ),
        ] {
            let source = source.replace(original, replacement);
            analysis.update_file(file, source.clone());
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            let [diagnostic] = diagnostics.as_slice() else {
                panic!("{expression}: {diagnostics:?}");
            };
            assert_eq!(diagnostic.id().as_str(), "unresolved-attribute");
            assert!(diagnostic.concise_message().to_string().contains("None"));
            let range = diagnostic.range().unwrap();
            assert_eq!(
                &source[range.start().to_usize()..range.end().to_usize()],
                expression,
            );
        }
    }

    #[test]
    fn attribute_guards_refine_matching_file_views() {
        let source = r#"def implementation(ctx):
    if ctx.attr.jar:
        print(ctx.file.jar.path)
        if ctx.attr.tool:
            print(ctx.file.jar.path, ctx.executable.tool.path)
    selected = ctx.file.jar if ctx.attr.jar else ctx.files.jars[0]
    print(selected.path)
    if ctx.attr.tool:
        print(ctx.executable.tool.path)
    return []
example = rule(implementation=implementation, attrs={
    "jar": attr.label(allow_single_file=True),
    "jars": attr.label_list(allow_files=True),
    "other": attr.label(allow_single_file=True),
    "tool": attr.label(executable=True, cfg="exec"),
})
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let file = fixture.main_file();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        for (expression, key) in [
            ("ctx.file.jar", "\"jar\""),
            ("ctx.executable.tool", "\"tool\""),
        ] {
            check_field(&analysis.snapshot(), file, source, expression, "File", key);
        }

        for (statement, expression) in [
            ("    print(ctx.file.jar.path)", "ctx.file.jar.path"),
            (
                "    if not ctx.attr.jar:\n        print(ctx.file.jar.path)",
                "ctx.file.jar.path",
            ),
            (
                "    if ctx.attr.jar:\n        print(ctx.file.other.path)",
                "ctx.file.other.path",
            ),
            (
                "    if ctx.attr.jar:\n        print(ctx.executable.tool.path)",
                "ctx.executable.tool.path",
            ),
            (
                "    if ctx.attr.tool:\n        print(ctx.file.jar.path)",
                "ctx.file.jar.path",
            ),
        ] {
            let changed = source.replace("    return []", &format!("{statement}\n    return []"));
            analysis.update_file(file, changed.clone());
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            let [diagnostic] = diagnostics.as_slice() else {
                panic!("{statement}: {diagnostics:?}");
            };
            assert_eq!(diagnostic.id().as_str(), "unresolved-attribute");
            assert!(diagnostic.concise_message().to_string().contains("None"));
            assert_eq!(&changed[diagnostic.range().unwrap()], expression);
        }
        analysis.update_file(
            file,
            source.replace("allow_single_file=True", "allow_single_file=False"),
        );
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for (option, replacement, expression, key) in [
            (
                "allow_single_file=True",
                "allow_single_file=None",
                "ctx.file.jar",
                "\"jar\"",
            ),
            (
                "executable=True",
                "executable=False",
                "ctx.executable.tool",
                "\"tool\"",
            ),
        ] {
            let changed = source.replace(option, replacement);
            analysis.update_file(file, changed.clone());
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{option}: {diagnostics:?}");
            // Disabled views use the native struct's generic field fallback.
            check_field(
                &analysis.snapshot(),
                file,
                &changed,
                expression,
                "Unknown",
                "",
            );
            analysis.update_file(file, source.to_owned());
            check_field(&analysis.snapshot(), file, source, expression, "File", key);
        }
    }

    #[test]
    fn provider_field_mappings_preserve_list_inference() {
        let providers = r#"FIELDS = dict(value='Value documentation')
SecondInfo = provider(fields=FIELDS)
ThirdInfo = provider(fields=FIELDS)
"#;
        let source = r#"load('providers.bzl', 'SecondInfo', 'ThirdInfo')
FirstInfo = provider(fields=['value'])
def implementation(ctx):
    image = ctx.attr.image
    result = [image[DefaultInfo], FirstInfo(value=1)]
    for provider_type in [SecondInfo, ThirdInfo, OutputGroupInfo]:
        if provider_type in image:
            result.append(image[provider_type])
    _ = result[0]
    return result
example = rule(implementation=implementation, attrs={'image': attr.label(mandatory=True)})
"#;
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "providers.bzl", providers);
        let file = fixture.add_file(&mut analysis.db, "defs.bzl", source);
        loader.add_files_from_fixture(&fixture);
        enable_context(&mut analysis);
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let db = &snapshot.db;
        let program = db.starlark_program_file(file);
        let parsed = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
        let model = SemanticModel::new(db, program);
        let [_load, _provider, implementation, _rule] = parsed.suite().as_slice() else {
            panic!("expected provider fixture declarations");
        };
        let Stmt::FunctionDef(function) = implementation else {
            panic!("expected implementation function");
        };
        let [_image, _initial, _loop, observation, _return] = function.body.as_slice() else {
            panic!("expected list construction, appends, and observation");
        };
        let Stmt::Assign(assignment) = observation else {
            panic!("expected list element observation");
        };
        let ty = assignment.value.inferred_type(&model).unwrap();
        let mut elements: Vec<_> = ty
            .as_union()
            .expect("expected a union of provider instances")
            .elements(db)
            .iter()
            .map(|ty| ty.display(db, &model.program_environment()).to_string())
            .collect();
        elements.sort();
        assert_eq!(
            elements,
            [
                "DefaultInfo[depset[File], FilesToRunProvider[File | None]]",
                "DefaultInfo[depset[File], None]",
                "FirstInfo",
                "OutputGroupInfo",
                "SecondInfo",
                "ThirdInfo",
            ]
        );
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
    fn scalar_attribute_values_include_permitted_defaults() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        enable_context(&mut analysis);
        let file = fixture.main_file();
        for (descriptor, expected) in [
            (
                "attr.string(values=['fast', 'slow'], default='fast')",
                "Literal[\"fast\", \"slow\"]",
            ),
            (
                "attr.string(values=('fast', 'slow'), mandatory=True)",
                "Literal[\"fast\", \"slow\"]",
            ),
            (
                "make(values=['fast', 'fast'], mandatory=True)",
                "Literal[\"fast\"]",
            ),
            ("attr.string(values=['fast'])", "Literal[\"fast\", \"\"]"),
            (
                "attr.string(values=['fast'], default='other')",
                "Literal[\"fast\", \"other\"]",
            ),
            ("attr.string(values=['fast'], default=dynamic())", "str"),
            (
                "attr.string(values=['fast'], default=dynamic(), mandatory=True)",
                "Literal[\"fast\"]",
            ),
            ("attr.string(values=['fast', dynamic()])", "str"),
            ("attr.string(values=allowed)", "str"),
            ("attr.string(values=[])", "str"),
            ("attr.string(values=())", "str"),
            ("attr.string()", "str"),
            ("attr.string(values=unknown())", "str"),
            ("attr.int(values=[-1, 1, 1], default=-1)", "Literal[-1, 1]"),
            (
                "attr.int(values=(-2147483648, 2147483647), mandatory=True)",
                "Literal[-2147483648, 2147483647]",
            ),
            ("attr.int(values=[1])", "Literal[1, 0]"),
            ("attr.int(values=[1], default=2)", "Literal[1, 2]"),
            ("attr.int(values=[1], default=number())", "int"),
            ("attr.int(values=[1, number()])", "int"),
            ("attr.int(values=[2147483648], mandatory=True)", "int"),
            ("attr.int(values=[1], default=-2147483649)", "int"),
            ("attr.int(values=[])", "int"),
            ("attr.int()", "int"),
            (
                "attr.string(values=['fast']) if flag() else attr.string(values=['slow'])",
                "Unknown",
            ),
            ("fake(values=['fast'], mandatory=True)", "Unknown"),
        ] {
            let source = format!(
                "def dynamic() -> str: return 'other'\ndef number() -> int: return 2\ndef flag() -> bool: return True\ndef unknown(): pass\ndef fake(**kwargs): return None\nallowed = ['fast']\nmake = attr.string\ndef implementation(ctx):\n    ctx.attr.mode\nexample = rule(implementation=implementation, attrs={{'mode': {descriptor}}})\n"
            );
            analysis.update_file(file, source.clone());
            let snapshot = analysis.snapshot();
            check_field(
                &snapshot,
                file,
                &source,
                "ctx.attr.mode",
                expected,
                "'mode'",
            );
            let diagnostics = snapshot.diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{descriptor}: {diagnostics:?}");
        }
        let source = "def implementation(ctx):\n    ctx.attr.mode\nexample = rule(implementation=implementation, attrs={'mode': attr.int(values=[True], mandatory=True)})\n";
        analysis.update_file(file, source.to_owned());
        let snapshot = analysis.snapshot();
        check_field(&snapshot, file, source, "ctx.attr.mode", "int", "'mode'");
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.id().as_str() == "invalid-argument-type"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn finite_attribute_guards_follow_descriptor_edits() {
        let source = r#"def dynamic() -> str: return "zstd"
def implementation(ctx):
    if ctx.attr.mode == "zstd":
        pass
    elif ctx.attr.mode == "gzip":
        pass
    else:
        print(ctx.attrs)
    print(ctx.missing)
    return []
example = rule(implementation=implementation, attrs={
    "mode": attr.string(values=["zstd", "gzip"], default="zstd"),
})
example(name="dynamic", mode=dynamic())
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let file = fixture.main_file();
        for (edited, errors, redundant_conditions) in [
            (source.to_owned(), vec!["ctx.missing"], 1),
            (
                source.replace("values=[\"zstd\", \"gzip\"]", "values=[]"),
                vec!["ctx.attrs", "ctx.missing"],
                0,
            ),
            (
                source.replace("default=\"zstd\"", "default=dynamic()"),
                vec!["ctx.attrs", "ctx.missing"],
                0,
            ),
            (source.to_owned(), vec!["ctx.missing"], 1),
        ] {
            analysis.update_file(file, edited.clone());
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(
                diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.id().as_str() == "redundant-condition")
                    .count(),
                redundant_conditions,
                "{diagnostics:?}"
            );
            let errors_found: Vec<_> = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.id().as_str() != "redundant-condition")
                .collect();
            assert_eq!(errors_found.len(), errors.len(), "{diagnostics:?}");
            for (diagnostic, expression) in errors_found.into_iter().zip(errors) {
                assert_eq!(diagnostic.id().as_str(), "unresolved-attribute");
                assert_eq!(
                    usize::from(diagnostic.range().unwrap().start()),
                    edited.find(expression).unwrap(),
                    "{diagnostic:?}"
                );
            }
        }
    }

    #[test]
    fn inherited_rule_attributes_preserve_contracts_and_origins() {
        let parent = r#"def implementation(ctx): return []
base_test = rule(implementation=implementation, test=True, extendable=True, attrs={
    "deps": attr.label_list(mandatory=True),
    "tool": attr.label(default="//:tool", executable=True, cfg="exec"),
    "single": attr.label(default="//:input", allow_single_file=True),
    "_private": attr.string(default="private", values=["private", "other"]),
})
"#;
        let source = r#"load("parent.bzl", imported="base_test")
alias = imported
def implementation(ctx):
    ctx.attr.deps
    ctx.attr.tool
    ctx.attr._private
    ctx.attr.timeout
    ctx.executable.tool
    ctx.file.single
    ctx.outputs.executable
example_test = rule(parent=alias, implementation=implementation, attrs={
    "own": attr.string(mandatory=True),
    "deps": attr.label_list(),
    "tool": attr.label(default="//:replacement"),
})
example_test(name="ok", own="value", deps=["//:dep"], timeout="short")
example_test(name="selected", own="value", deps=select({"//conditions:default": []}))
"#;
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let parent_file = fixture.add_file(&mut analysis.db, "parent.bzl", parent);
        let file = fixture.add_file(&mut analysis.db, "defs.bzl", source);
        loader.add_files_from_fixture(&fixture);
        enable_context(&mut analysis);
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for (expression, expected, key) in [
            (
                "ctx.attr.deps",
                "list[Target[FilesToRunProvider[File | None]] | Target[None]]",
                "\"deps\"",
            ),
            (
                "ctx.attr.tool",
                "Target[FilesToRunProvider[File]]",
                "\"tool\"",
            ),
            ("ctx.attr.timeout", "str", ""),
            ("ctx.executable.tool", "File", "\"tool\""),
            ("ctx.file.single", "File", ""),
            ("ctx.outputs.executable", "File", ""),
            ("ctx.attr._private", "Literal[\"private\", \"other\"]", ""),
        ] {
            check_field(&snapshot, file, source, expression, expected, key);
        }
        let position = FilePosition {
            file_id: file,
            pos: (source.find("ctx.attr._private").unwrap() as u32 + 16).into(),
        };
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
        assert_eq!(*target_file_id, parent_file.source);
        assert_eq!(&parent[*target_selection_range], "\"_private\"");
        drop(snapshot);
        for invalid in [
            "example_test(name='bad', own='value')",
            "example_test(name='bad', own='value', deps=[1])",
            "example_test(name='bad', deps=[])",
            "example_test(name='bad', own='value', deps=[], _private='value')",
        ] {
            analysis
                .open_document(
                    std::path::Path::new("/defs.bzl"),
                    starpls_common::Dialect::Bazel,
                    None,
                    format!("{source}{invalid}\n"),
                    2,
                )
                .unwrap();
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            let [diagnostic] = diagnostics.as_slice() else {
                panic!("{invalid}: {diagnostics:?}");
            };
            assert!(usize::from(diagnostic.range().unwrap().start()) >= source.len());
        }
        let edited = source.replace("    \"deps\": attr.label_list(),\n", "");
        analysis
            .open_document(
                std::path::Path::new("/defs.bzl"),
                starpls_common::Dialect::Bazel,
                None,
                edited.clone(),
                3,
            )
            .unwrap();
        analysis
            .open_document(
                std::path::Path::new("/parent.bzl"),
                starpls_common::Dialect::Bazel,
                None,
                parent.replace(
                    "attr.label_list(mandatory=True)",
                    "attr.string(mandatory=True)",
                ),
                2,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        check_field(&snapshot, file, &edited, "ctx.attr.deps", "str", "");
        assert_eq!(snapshot.diagnostics(file).unwrap().len(), 2);
    }

    #[test]
    fn inherited_label_defaults_account_for_computed_overrides() {
        let parent = "def implementation(ctx): return []\nbase = rule(implementation=implementation, extendable=True, attrs={'dep': attr.label(default='//:input', allow_single_file=True)})\n";
        for (attrs, expected) in [
            ("{'dep': attr.label(default=None)}", "File"),
            ("{'dep': attr.label(default=computed)}", "File | None"),
            (
                "{'dep': opaque(attr.label(default=computed))}",
                "File | None",
            ),
            (
                "opaque({'dep': attr.label(default=computed)})",
                "File | None",
            ),
        ] {
            let source = format!(
                r#"load("parent.bzl", "base")
def opaque(value): return value
def computed(): return None
def implementation(ctx):
    ctx.file.dep
example = rule(parent=base, implementation=implementation, attrs={attrs})
example(name="test", dep="//:explicit")
"#
            );
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
            fixture.add_file(&mut analysis.db, "parent.bzl", parent);
            let file = fixture.add_file(&mut analysis.db, "main.bzl", &source);
            loader.add_files_from_fixture(&fixture);
            enable_context(&mut analysis);
            let snapshot = analysis.snapshot();
            let diagnostics = snapshot.diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{attrs}: {diagnostics:?}");
            check_field(&snapshot, file, &source, "ctx.file.dep", expected, "");
            if expected == "File" {
                check_field(&snapshot, file, &source, "ctx.file.dep", expected, "'dep'");
                let position = FilePosition {
                    file_id: file,
                    pos: (source.rfind("dep=").unwrap() as u32).into(),
                };
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
                assert_eq!(&source[*target_selection_range], "'dep'");
            }
        }
    }

    #[test]
    fn unknown_rule_parents_preserve_only_proven_own_contracts() {
        let parent = "def implementation(ctx): return []\nbase = rule(implementation=implementation, extendable=True, attrs={'dep': attr.label(), 'unseen': attr.bool()})\n";
        let source = r#"load("parent.bzl", "base")
def opaque(value): return value
def implementation(ctx):
    ctx.attr.dep
    ctx.attr.own
    ctx.executable.dep
example = rule(parent=opaque(base), implementation=implementation, attrs={
    "dep": attr.label(default="//:dep"),
    "own": attr.string(mandatory=True),
})
example(name="ok", own="value", unseen=True)
"#;
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "parent.bzl", parent);
        let file = fixture.add_file(&mut analysis.db, "main.bzl", source);
        loader.add_files_from_fixture(&fixture);
        enable_context(&mut analysis);
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        check_field(
            &snapshot,
            file,
            source,
            "ctx.attr.dep",
            "Unknown",
            "\"dep\"",
        );
        check_field(
            &snapshot,
            file,
            source,
            "ctx.executable.dep",
            "Unknown",
            "\"dep\"",
        );
        check_field(&snapshot, file, source, "ctx.attr.own", "str", "\"own\"");
        drop(snapshot);
        for invalid in [
            "example(name='bad', own=1)",
            "example(name='bad')",
            "example(name='bad', own='value', dep=1)",
        ] {
            analysis
                .open_document(
                    std::path::Path::new("/main.bzl"),
                    starpls_common::Dialect::Bazel,
                    None,
                    format!("{source}{invalid}\n"),
                    2,
                )
                .unwrap();
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(diagnostics.len(), 1, "{invalid}: {diagnostics:?}");
        }
    }

    #[test]
    fn test_rule_attributes_share_the_call_and_context_schema() {
        for option in [
            "test=True",
            "analysis_test=True",
            "test=False, analysis_test=True",
        ] {
            let source = format!(
                r#"def implementation(ctx):
    timeout: str = ctx.attr.timeout
    size: str = ctx.attr.size
    flaky: bool = ctx.attr.flaky
    shards: int = ctx.attr.shard_count
    local: bool = ctx.attr.local
    args: list[str] = ctx.attr.args
    ctx.outputs.executable.basename
    (timeout, size, flaky, shards, local, args)
example_test = rule(implementation=implementation, {option})
example_test(name="test", timeout="short", size="small", flaky=True, shard_count=2, local=True, args=["--flag"], aspect_hints=["//:hint"])
"#
            );
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(&source);
            enable_context(&mut analysis);
            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            assert!(diagnostics.is_empty(), "{option}: {diagnostics:?}");
            for (expression, expected) in
                [("ctx.attr.timeout", "str"), ("ctx.attr.args", "list[str]")]
            {
                check_field(
                    &analysis.snapshot(),
                    fixture.main_file(),
                    &source,
                    expression,
                    expected,
                    "",
                );
            }
            for invalid in [
                "timeout=1",
                "timeout=select({'//conditions:default': 'short'})",
                "aspect_hints=[1]",
            ] {
                analysis
                    .open_document(
                        std::path::Path::new("/main.bzl"),
                        starpls_common::Dialect::Bazel,
                        None,
                        format!("{source}example_test(name='bad', {invalid})\n"),
                        2,
                    )
                    .unwrap();
                let diagnostics = analysis
                    .snapshot()
                    .diagnostics(fixture.main_file())
                    .unwrap();
                let [diagnostic] = diagnostics.as_slice() else {
                    panic!("{invalid}: {diagnostics:?}");
                };
                assert!(usize::from(diagnostic.range().unwrap().start()) >= source.len());
            }
        }
    }

    #[test]
    fn generated_rule_attributes_have_bounded_visibility() {
        for (options, name, field, argument, errors) in [
            ("", "example", "timeout", "timeout='short'", 1),
            ("test=False", "example", "timeout", "timeout='short'", 1),
            (
                "test=True",
                "example_test",
                "aspect_hints",
                "timeout='short'",
                0,
            ),
            (
                "test=unknown()",
                "example_test",
                "timeout",
                "timeout='short'",
                0,
            ),
            (
                "analysis_test=unknown()",
                "example_test",
                "timeout",
                "timeout='short'",
                0,
            ),
            (
                "test=unknown()",
                "example_test",
                "missing",
                "nonexistent=True",
                1,
            ),
        ] {
            let source = format!("def unknown() -> bool: return True\ndef implementation(ctx):\n    ctx.attr.{field}\n{name} = rule(implementation=implementation, {options})\n{name}(name='test', {argument})\n");
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(&source);
            enable_context(&mut analysis);
            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            assert_eq!(
                diagnostics.len(),
                errors,
                "{options}, {field}: {diagnostics:?}"
            );
            check_field(
                &analysis.snapshot(),
                fixture.main_file(),
                &source,
                &format!("ctx.attr.{field}"),
                "Unknown",
                "",
            );
        }
    }

    #[test]
    fn aspect_callbacks_use_target_and_own_attributes() {
        let source = r#"def implementation(subject, context):
    subject.files.to_list()
    context.attr.mode
    context.attr._tool.label
    context.files._tool
    context.file._tool
    context.executable._tool
    context.rule.attr.unselected
    _wrong_target: str = subject.files.to_list()
    _wrong_mode: int = context.attr.mode
    print(_wrong_target, _wrong_mode)
make_aspect = aspect
example = make_aspect(implementation=implementation, attrs={
    "mode": attr.string(default="fast", values=["fast", "slow"]),
    "_tool": attr.label(default="//:tool", allow_single_file=True, executable=True, cfg="exec"),
})
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let file = fixture.main_file();
        let snapshot = analysis.snapshot();
        check_field(
            &snapshot,
            file,
            source,
            "    subject.files",
            "depset[File]",
            "",
        );
        check_field(
            &snapshot,
            file,
            source,
            "    context.attr.mode",
            "Literal[\"fast\", \"slow\"]",
            "\"mode\"",
        );
        check_field(
            &snapshot,
            file,
            source,
            "    context.files._tool",
            "list[File]",
            "\"_tool\"",
        );
        check_field(
            &snapshot,
            file,
            source,
            "    context.file._tool",
            "File",
            "\"_tool\"",
        );
        check_field(
            &snapshot,
            file,
            source,
            "    context.executable._tool",
            "File",
            "\"_tool\"",
        );
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.id().as_str() == "invalid-assignment"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn aspect_parameter_contract_requires_a_unique_registration() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        enable_context(&mut analysis);
        let file = fixture.main_file();
        for (parameters, extra, registration, expected) in [
            ("item, env", "", "aspect(implementation=implementation, attrs={'mode': attr.string(default='fast', values=['fast'])})", "Literal[\"fast\"]"),
            ("item, env", "", "aspect(implementation=implementation, attrs={'mode': attr.int(default=1, values=[1])})", "Literal[1]"),
            ("item, env", "", "aspect(implementation=implementation, attrs={'mode': attr.string(default='fast', values=['fast'])})", "Literal[\"fast\"]"),
            ("item, env", "other = aspect(implementation=implementation)", "aspect(implementation=implementation)", "Unknown"),
            ("item, *, env", "", "aspect(implementation=implementation)", "Unknown"),
            ("item, env, *rest", "", "aspect(implementation=implementation)", "Unknown"),
            ("item, env", "def aspect(implementation): pass", "aspect(implementation=implementation)", "Unknown"),
        ] {
            let source = format!("def implementation({parameters}):\n    env.attr.mode\n{extra}\nexample = {registration}\n");
            analysis.update_file(file, source.clone());
            check_field(&analysis.snapshot(), file, &source, "    env.attr.mode", expected, "");
        }
        let source = "def implementation(item: str, env):\n    item.upper()\nexample = aspect(implementation=implementation)\n";
        analysis.update_file(file, source.to_owned());
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn loaded_build_setting_values_follow_descriptor_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        enable_context(&mut analysis);
        let settings = fixture.add_file(&mut analysis.db, "//:settings.bzl", "");
        let source = r#"load("//:settings.bzl", "setting")
def implementation(context):
    context.build_setting_value
    context.attr.build_setting_default
    context.attr.help
    _wrong_value: None = context.build_setting_value
    _wrong_default: None = context.attr.build_setting_default
    _wrong_help: None = context.attr.help
    print(_wrong_value, _wrong_default, _wrong_help)
example = rule(implementation=implementation, build_setting=setting)
"#;
        let file = fixture.add_file(&mut analysis.db, "//:defs.bzl", source);
        loader.add_files_from_fixture(&fixture);
        for (descriptor, value, default) in [
            ("config.bool()", "bool", "bool"),
            ("config.int()", "int", "int"),
            ("config.string()", "str", "str"),
            ("config.string(allow_multiple=True)", "list[str]", "str"),
            ("config.string(allow_multiple=False)", "str", "str"),
            (
                "config.string(allow_multiple=option())",
                "str | list[str]",
                "str",
            ),
            ("config.string_list()", "list[str]", "list[str]"),
            (
                "config.string_list(flag=True, repeatable=True)",
                "list[str]",
                "list[str]",
            ),
            ("config.bool()", "bool", "bool"),
        ] {
            analysis.update_file(
                settings,
                format!("def option() -> bool: return True\nsetting = {descriptor}\n"),
            );
            let snapshot = analysis.snapshot();
            let setting_diagnostics = snapshot.diagnostics(settings).unwrap();
            assert!(
                setting_diagnostics.is_empty(),
                "{descriptor}: {setting_diagnostics:?}"
            );
            check_field(
                &snapshot,
                file,
                source,
                "    context.build_setting_value",
                value,
                "",
            );
            check_field(
                &snapshot,
                file,
                source,
                "    context.attr.build_setting_default",
                default,
                "",
            );
            check_field(&snapshot, file, source, "    context.attr.help", "str", "");
            let diagnostics = snapshot.diagnostics(file).unwrap();
            assert_eq!(diagnostics.len(), 3, "{descriptor}: {diagnostics:?}");
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.id().as_str() == "invalid-assignment"),
                "{diagnostics:?}"
            );
        }
        let position = FilePosition {
            file_id: file,
            pos: ((source.find("build_setting_value").unwrap() + "build_setting_value".len() - 1)
                as u32)
                .into(),
        };
        {
            let snapshot = analysis.snapshot();
            let hover = snapshot.hover(position).unwrap().unwrap();
            assert!(
                hover
                    .contents
                    .value
                    .contains("Value of the build setting represented"),
                "{}",
                hover.contents.value
            );
        }
        analysis.update_file(
            file,
            source.replace(
                "    print(",
                "    context.build_setting_value = True\n    print(",
            ),
        );
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert_eq!(diagnostics.len(), 4, "{diagnostics:?}");
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .concise_message()
                .to_string()
                .contains("read-only")),
            "{diagnostics:?}"
        );
        analysis.update_file(file, source.to_owned());
        analysis.update_file(
            settings,
            "def choose(value): return value\nsetting = choose(None)\n".to_owned(),
        );
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let source = source.replace("build_setting=setting)", "build_setting=setting, attrs={\"build_setting_default\": attr.int(), \"help\": attr.bool()})");
        analysis.update_file(file, source.clone());
        for (descriptor, default, help, errors) in [
            ("choose(None)", "int", "bool", 2),
            ("config.string()", "str", "str", 3),
        ] {
            analysis.update_file(
                settings,
                format!("def choose(value): return value\nsetting = {descriptor}\n"),
            );
            let snapshot = analysis.snapshot();
            check_field(
                &snapshot,
                file,
                &source,
                "    context.attr.build_setting_default",
                default,
                "",
            );
            check_field(&snapshot, file, &source, "    context.attr.help", help, "");
            let diagnostics = snapshot.diagnostics(file).unwrap();
            assert_eq!(diagnostics.len(), errors, "{descriptor}: {diagnostics:?}");
        }
    }

    #[test]
    fn macro_defaults_and_registration_follow_edits() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        enable_context(&mut analysis);
        let file = fixture.main_file();
        // The default expression's range and the public callable are unchanged.
        // Only raw metadata for the private callback parameter changes.
        for (default, expected, errors) in [
            ("None         ", "None", 1),
            ("Label('//:x')", "Label", 0),
            ("None         ", "None", 1),
        ] {
            let source = format!(
                r#"DEFAULT = {default}
def implementation(name, visibility, _value):
    _value.name
example = macro(implementation=implementation, attrs={{
    "_value": attr.label(default=DEFAULT, configurable=False),
}})
"#
            );
            analysis.update_file(file, source.clone());
            for _ in 0..2 {
                let snapshot = analysis.snapshot();
                check_field(&snapshot, file, &source, "    _value", expected, "");
                let diagnostics = snapshot.diagnostics(file).unwrap();
                assert_eq!(diagnostics.len(), errors, "{diagnostics:?}");
                if let Some(diagnostic) = diagnostics.first() {
                    assert_eq!(diagnostic.id().as_str(), "unresolved-attribute");
                }
            }
        }
        for (parameter, annotation, registrations, expected) in [
            ("value", "", "example = macro(implementation=implementation, attrs={'value': attr.string(configurable=False, values=['fast'], default='fast')})", "str"),
            ("value", "", "example = macro(implementation=implementation, attrs={'value': attr.int(configurable=False, values=[1], default=1)})", "int"),
            ("value", "", "example = macro(implementation=implementation, attrs={'value': attr.string(configurable=False)})", "str"),
            ("value", "", "example = macro(implementation=implementation, attrs={'value': attr.int(configurable=False)})", "int"),
            ("value", "", "example = macro(implementation=implementation, attrs={'value': attr.string(configurable=False)})", "str"),
            ("value", "", "first = macro(implementation=implementation, attrs={'value': attr.string()})\nsecond = macro(implementation=implementation, attrs={'value': attr.int()})", "Unknown"),
            ("value", "    # type: (str, list[Label], Unknown) -> None\n", "example = macro(implementation=implementation, attrs={'value': attr.string()})", "Unknown"),
            ("value=42", "", "example = macro(implementation=implementation, attrs={'value': attr.string()})", "Unknown | Literal[42]"),
        ] {
            let source = format!("def implementation(name, visibility, {parameter}):\n{annotation}    value\n{registrations}\n");
            analysis.update_file(file, source.clone());
            check_field(&analysis.snapshot(), file, &source, "    value", expected, "");
        }
        // Invalid non-None macro returns still need stable body inference during
        // editing. Resolving this return re-enters the registration expression.
        let source = "def implementation(name, visibility, value):\n    return value\nexample = macro(implementation=implementation, attrs={'value': attr.string(configurable=False)})\n";
        analysis.update_file(file, source.to_owned());
        for _ in 0..2 {
            check_field(
                &analysis.snapshot(),
                file,
                source,
                "return value",
                "str",
                "",
            );
        }
    }

    #[test]
    fn dependency_contracts_preserve_provider_presence() {
        let source = r#"def transition_impl(settings, attr):
    return [{}]
split = transition(implementation=transition_impl, inputs=[], outputs=[])
def implementation(ctx):
    files_to_run = ctx.attr.single[DefaultInfo].files_to_run
    optional_executable = files_to_run.executable
    if optional_executable != None:
        print(optional_executable.path)
    ctx.attr.tool[DefaultInfo].files_to_run.executable.path
    ctx.attr.single[DefaultInfo].files_to_run.runfiles_manifest
    ctx.attr.split[0][DefaultInfo].files_to_run.executable.path
    for target in ctx.split_attr.split.values():
        print(target[DefaultInfo].files_to_run.executable.path)
    ctx.actions.run(executable=files_to_run, outputs=[])
    ctx.actions.run(executable=ctx.attr.tool[DefaultInfo].files_to_run, outputs=[])
example = rule(implementation=implementation, attrs={
    "tool": attr.label(mandatory=True, executable=True, cfg="exec"),
    "single": attr.label(mandatory=True, allow_single_file=True),
    "split": attr.label(mandatory=True, executable=True, cfg=split),
})
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let file = fixture.main_file();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for expression in [
            "ctx.attr.single[DefaultInfo].files_to_run.executable.path",
            "ctx.attr.tool[DefaultInfo].files_to_run.runfiles_manifest.path",
        ] {
            let source = source.replacen(
                "    files_to_run =",
                &format!("    {expression}\n    files_to_run ="),
                1,
            );
            analysis.update_file(file, source.clone());
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            let [diagnostic] = diagnostics.as_slice() else {
                panic!("{expression}: {diagnostics:?}");
            };
            assert_eq!(diagnostic.id().as_str(), "unresolved-attribute");
            assert_eq!(
                &source[diagnostic.range().unwrap()],
                expression,
                "{diagnostic:?}"
            );
        }
    }

    #[test]
    fn provider_presence_tracks_descriptor_edits() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        enable_context(&mut analysis);
        let file = fixture.main_file();
        for (options, errors) in [
            ("", 1),
            (", executable=True, cfg='exec'", 0),
            ("", 1),
            (", executable=unknown(), cfg='exec'", 1),
            (", allow_single_file=True", 0),
            (", allow_single_file=unknown()", 1),
            (", providers=[PackageSpecificationInfo]", 1),
        ] {
            let source = format!("def unknown():\n    pass\ndef implementation(ctx):\n    ctx.attr.dep[DefaultInfo].files_to_run.executable\nexample = rule(implementation=implementation, attrs={{'dep': attr.label(mandatory=True{options})}})\n");
            analysis.update_file(file, source.clone());
            for _ in 0..2 {
                let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
                assert_eq!(diagnostics.len(), errors, "{options}: {diagnostics:?}");
                if let Some(diagnostic) = diagnostics.first() {
                    assert_eq!(diagnostic.id().as_str(), "unresolved-attribute");
                    assert!(diagnostic.concise_message().to_string().contains("None"));
                }
            }
        }
    }

    #[test]
    fn required_and_defaulted_context_values_are_present() {
        let source = r#"def computed(name):
    return None
def implementation(ctx):
    ctx.attr.required[DefaultInfo]
    ctx.attr.defaulted[DefaultInfo]
    ctx.file.required.basename
    ctx.file.defaulted.basename
    ctx.executable.tool.path
    ctx.attr.output.name
    ctx.outputs.output.path
    ctx.actions.run(executable=ctx.executable.tool, outputs=[])
    ctx.attr.optional[DefaultInfo]
    ctx.file.optional.basename
    ctx.executable.optional_tool.path
    ctx.attr.computed[DefaultInfo]
example = rule(implementation=implementation, attrs={
    "required": attr.label(mandatory=True, allow_single_file=True),
    "defaulted": attr.label(default="//:input", allow_single_file=True),
    "tool": attr.label(default=Label("//:tool"), executable=True, cfg="exec"),
    "output": attr.output(mandatory=True),
    "optional": attr.label(allow_single_file=True),
    "optional_tool": attr.label(executable=True, cfg="exec"),
    "computed": attr.label(default=computed),
})
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let file = fixture.main_file();
        let snapshot = analysis.snapshot();
        for (expression, expected, key) in [
            (
                "ctx.attr.required",
                "Target[FilesToRunProvider[File | None]]",
                "\"required\"",
            ),
            (
                "ctx.attr.defaulted",
                "Target[FilesToRunProvider[File | None]]",
                "\"defaulted\"",
            ),
            ("ctx.file.required", "File", "\"required\""),
            ("ctx.file.defaulted", "File", "\"defaulted\""),
            ("ctx.executable.tool", "File", "\"tool\""),
            ("ctx.attr.output", "Label", "\"output\""),
            ("ctx.outputs.output", "File", "\"output\""),
            (
                "ctx.attr.optional",
                "Target[FilesToRunProvider[File | None]] | None",
                "\"optional\"",
            ),
            ("ctx.file.optional", "File | None", "\"optional\""),
            (
                "ctx.executable.optional_tool",
                "File | None",
                "\"optional_tool\"",
            ),
            (
                "ctx.attr.computed",
                "Target[FilesToRunProvider[File | None]] | Target[None] | None",
                "\"computed\"",
            ),
        ] {
            check_field(&snapshot, file, source, expression, expected, key);
        }
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert_eq!(diagnostics.len(), 4, "{diagnostics:?}");
        for diagnostic in diagnostics {
            let range = diagnostic.range().unwrap();
            assert!(
                usize::from(range.start()) >= source.find("    ctx.attr.optional").unwrap(),
                "{diagnostic:?}"
            );
            assert!(
                usize::from(range.end()) < source.find("example =").unwrap(),
                "{diagnostic:?}"
            );
        }
    }

    #[test]
    fn repository_labels_preserve_required_and_optional_values() {
        let source = r#"def implementation(ctx):
    ctx.read(ctx.attr.required)
    ctx.attr.required.name
    ctx.read(ctx.attr.defaulted)
    ctx.attr.defaulted.name
    ctx.attr.label_default.name
    if ctx.attr.optional != None:
        ctx.read(ctx.attr.optional)
    ctx.attr.optional.name
    ctx.attr.mode
example = repository_rule(implementation=implementation, attrs={
    "required": attr.label(mandatory=True),
    "defaulted": attr.label(default="//:input"),
    "label_default": attr.label(default=Label("//:input")),
    "optional": attr.label(),
    "mode": attr.string(values=["fast"], default="other"),
})
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let file = fixture.main_file();
        let snapshot = analysis.snapshot();
        for (name, expected) in [
            ("required", "Label"),
            ("defaulted", "Label"),
            ("label_default", "Label"),
            ("optional", "Label | None"),
            ("mode", "Literal[\"fast\", \"other\"]"),
        ] {
            check_field(
                &snapshot,
                file,
                source,
                &format!("ctx.attr.{name}"),
                expected,
                &format!("\"{name}\""),
            );
        }
        let diagnostics = snapshot.diagnostics(file).unwrap();
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("{diagnostics:?}");
        };
        assert_eq!(diagnostic.id().as_str(), "unresolved-attribute");
        assert_eq!(
            usize::from(diagnostic.range().unwrap().start()),
            source.find("ctx.attr.optional.name").unwrap()
        );
    }

    #[test]
    fn context_presence_tracks_default_only_edits() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        enable_context(&mut analysis);
        let file = fixture.main_file();
        for (factory, access, present) in [
            (
                "rule",
                "[DefaultInfo]",
                "Target[FilesToRunProvider[File | None]] | Target[None]",
            ),
            ("repository_rule", ".name", "Label"),
        ] {
            for (default, optional) in [("None   ", true), ("'//:x' ", false), ("None   ", true)] {
                let source = format!("DEFAULT = {default}\ndef implementation(ctx):\n    ctx.attr.dep{access}\nexample = {factory}(implementation=implementation, attrs={{'dep': attr.label(default=DEFAULT)}})\n");
                analysis.update_file(file, source.clone());
                let snapshot = analysis.snapshot();
                let expected = if optional {
                    format!("{present} | None")
                } else {
                    present.to_owned()
                };
                check_field(&snapshot, file, &source, "ctx.attr.dep", &expected, "'dep'");
                let diagnostics = snapshot.diagnostics(file).unwrap();
                assert_eq!(
                    diagnostics.len(),
                    usize::from(optional),
                    "{factory}: {diagnostics:?}"
                );
            }
        }
    }

    #[test]
    fn macro_callbacks_inherit_native_attribute_metadata() {
        use starpls_bazel::build::attribute::Discriminator;
        let source = r#"def implementation(name, visibility, srcs, optional, **kwargs):
    srcs
    optional
    native.example(srcs=srcs, optional=optional)
example = macro(implementation=implementation, inherit_attrs=native.example)
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        enable_context(&mut analysis);
        let builtins = analysis
            .db
            .get_builtin_defs(&starpls_common::Dialect::Bazel)
            .builtins(&analysis.db)
            .clone();
        analysis
            .set_builtin_defs(
                builtins,
                starpls_bazel::build::BuildLanguage {
                    rule: vec![starpls_bazel::build::RuleDefinition {
                        name: "example".to_owned(),
                        attribute: [
                            ("srcs", Discriminator::LabelList, true, false),
                            ("optional", Discriminator::Boolean, false, true),
                        ]
                        .into_iter()
                        .map(|(name, kind, mandatory, configurable)| {
                            starpls_bazel::build::AttributeDefinition {
                                name: name.to_owned(),
                                r#type: kind as i32,
                                mandatory: Some(mandatory),
                                configurable: Some(configurable),
                                ..Default::default()
                            }
                        })
                        .collect(),
                        ..Default::default()
                    }],
                },
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(fixture.main_file()).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        check_field(
            &snapshot,
            fixture.main_file(),
            source,
            "    srcs",
            "list[Label]",
            "",
        );
        check_field(
            &snapshot,
            fixture.main_file(),
            source,
            "    optional",
            "select[bool | None] | None",
            "",
        );
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
            (
                "ctx.attr.dep",
                "Target[FilesToRunProvider[File | None]] | Target[None] | None",
                "\"dep\"",
            ),
            ("ctx.attr.out", "Label | None", "\"out\""),
            ("ctx.attr.outs", "list[Label]", "\"outs\""),
            (
                "ctx.attr.split",
                "list[Target[FilesToRunProvider[File | None]] | Target[None]]",
                "\"split\"",
            ),
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
                "dict[str | None, Target[FilesToRunProvider[File | None]] | Target[None]]",
                "\"split\"",
            ),
            (
                "ctx.split_attr.split_list",
                "dict[str | None, list[Target[FilesToRunProvider[File | None]] | Target[None]]]",
                "\"split_list\"",
            ),
            (
                "ctx.split_attr.split_keyed",
                "dict[str | None, list[Target[FilesToRunProvider[File | None]] | Target[None]]]",
                "\"split_keyed\"",
            ),
            (
                "ctx.split_attr.split_named",
                "dict[str | None, dict[str, Target[FilesToRunProvider[File | None]] | Target[None]]]",
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
