//! Native factories supply declarations to Ty after its ordinary call checking.

use ruff_db::files::FilePath;
use ruff_db::files::FileRange;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::name::Name;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use rustc_hash::FxHashMap;
use starpls_bazel::attr::AttributeKind;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::ProgramFile;
use ty_python_semantic::provided::ProvidedBindingValue;
use ty_python_semantic::provided::ProvidedClass;
use ty_python_semantic::provided::ProvidedData;
use ty_python_semantic::provided::ProvidedField;
use ty_python_semantic::provided::ProvidedInstanceFields;
use ty_python_semantic::types::CallableTypeKind;
use ty_python_semantic::types::CheckedArgument;
use ty_python_semantic::types::CheckedCall;
use ty_python_semantic::types::KnownClass;
use ty_python_semantic::types::Parameter;
use ty_python_semantic::types::ParameterDefault;
use ty_python_semantic::types::Parameters;
use ty_python_semantic::types::Signature;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::UnionType;
use ty_python_semantic::ProgramEnvironment;

use crate::Database;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct Attribute {
    pub(super) kind: AttributeKind,
    mandatory: bool,
    default: Option<FileRange>,
    documentation: Option<Box<str>>,
}

impl get_size2::GetSize for Attribute {
    fn get_heap_size(&self) -> usize {
        self.documentation.as_ref().map_or(0, |doc| doc.len())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(crate) struct Documentation {
    pub(crate) text: Option<Box<str>>,
    pub(crate) parameters: Vec<(Name, Box<str>)>,
}

fn doc_string(db: &Database, call: &CheckedCall<'_, '_>) -> Option<Box<str>> {
    let CheckedArgument::Value { ty, expression: _ } = call.argument("doc") else {
        return None;
    };
    ty.string_literal_value(db).map(Into::into)
}

pub(super) enum Factory {
    Attribute(AttributeKind),
    Rule { repository: bool },
    Macro,
    Struct,
    Provider,
}

pub(super) fn declaration(db: &Database, declaration: Definition<'_>) -> Option<Factory> {
    let file = declaration.program_file(db);
    let FilePath::SystemVirtual(path) = file.file(db).path(db) else {
        return None;
    };
    if !path.as_str().starts_with("starpls-native:") {
        return None;
    }
    let DefinitionKind::Function(function) = declaration.kind(db) else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let function = function.node(&parsed);
    let attr_method = parsed.suite().iter().any(|statement| {
        let Stmt::ClassDef(namespace) = statement else {
            return false;
        };
        namespace.name.as_str() == "_starpls_types"
            && namespace.body.iter().any(|statement| {
                let Stmt::ClassDef(class) = statement else {
                    return false;
                };
                class.name.as_str() == "attr" && class.range().contains_range(function.range())
            })
    });
    if attr_method {
        return attribute_kind(function.name.as_str()).map(Factory::Attribute);
    }
    // A same-named method is not the global factory declaration.
    if !parsed.suite().iter().any(|statement| {
        let Stmt::ClassDef(namespace) = statement else {
            return false;
        };
        namespace.name.as_str().starts_with("_starpls_globals_")
            && namespace.body.iter().any(|statement| {
                let Stmt::FunctionDef(candidate) = statement else {
                    return false;
                };
                candidate.range() == function.range()
            })
    }) {
        return None;
    }
    match function.name.as_str() {
        "rule" => Some(Factory::Rule { repository: false }),
        "repository_rule" => Some(Factory::Rule { repository: true }),
        "macro" => Some(Factory::Macro),
        "struct" => Some(Factory::Struct),
        "provider" => Some(Factory::Provider),
        _ => None,
    }
}

pub(super) fn result<'db>(db: &'db Database, call: &CheckedCall<'_, 'db>) -> Option<Type<'db>> {
    match declaration(db, call.declaration()?)? {
        Factory::Attribute(kind) => attribute(db, call, kind),
        Factory::Rule { repository } => rule(
            db,
            call,
            if repository {
                RuleKind::Repository
            } else {
                RuleKind::Build
            },
        ),
        Factory::Macro => rule(db, call, RuleKind::Macro),
        Factory::Struct => structure(db, call),
        Factory::Provider => provider(db, call),
    }
}

fn attribute_kind(name: &str) -> Option<AttributeKind> {
    Some(match name {
        "bool" => AttributeKind::Bool,
        "int" => AttributeKind::Int,
        "int_list" => AttributeKind::IntList,
        "label" => AttributeKind::Label,
        "label_keyed_string_dict" => AttributeKind::LabelKeyedStringDict,
        "label_list" => AttributeKind::LabelList,
        "output" => AttributeKind::Output,
        "output_list" => AttributeKind::OutputList,
        "string" => AttributeKind::String,
        "string_dict" => AttributeKind::StringDict,
        "string_list" => AttributeKind::StringList,
        "string_list_dict" => AttributeKind::StringListDict,
        "string_keyed_label_dict" => AttributeKind::StringKeyedLabelDict,
        _ => return None,
    })
}

fn attribute<'db>(
    db: &'db Database,
    call: &CheckedCall<'_, 'db>,
    kind: AttributeKind,
) -> Option<Type<'db>> {
    let mandatory = match call.argument("mandatory") {
        CheckedArgument::Omitted => false,
        CheckedArgument::Value { ty, expression: _ } => ty.as_bool_literal()?,
        CheckedArgument::Indeterminate => return None,
    };
    let default = match call.argument("default") {
        CheckedArgument::Omitted => None,
        CheckedArgument::Value { ty: _, expression } => {
            let expression = expression?;
            let file = call.file();
            let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
            Some(FileRange::new(
                file.file(db),
                starpls_syntax::source::expr_range(expression, call.call().into(), parsed.tokens()),
            ))
        }
        CheckedArgument::Indeterminate => return None,
    };
    let environment = ProgramEnvironment::from_file(call.file());
    let class = call.class_type(
        db,
        ProvidedClass {
            name: Name::new("Attribute"),
            bases: declared_base(db, call, "Attribute"),
            class_members: Box::default(),
            instance_fields: ProvidedInstanceFields {
                fields: Box::default(),
                has_dynamic_fields: false,
                data: Some(ProvidedData::new(Attribute {
                    kind,
                    mandatory,
                    default,
                    documentation: doc_string(db, call),
                })),
            },
        },
    );
    class.to_instance_approximation(db, &environment)
}

enum RuleKind {
    Build,
    Repository,
    Macro,
}

fn rule<'db>(db: &'db Database, call: &CheckedCall<'_, 'db>, kind: RuleKind) -> Option<Type<'db>> {
    let environment = ProgramEnvironment::from_file(call.file());
    let common = starpls_bazel::attr::make_common_attributes();
    let common = match kind {
        RuleKind::Build => common.build,
        RuleKind::Repository => common.repository,
        RuleKind::Macro => Vec::new(),
    };
    let mut documentation = Documentation {
        text: doc_string(db, call),
        parameters: common
            .iter()
            .map(|attribute| {
                (
                    Name::new(&attribute.name),
                    attribute.doc.clone().into_boxed_str(),
                )
            })
            .collect(),
    };
    let mut parameters = common
        .into_iter()
        .map(|attribute| {
            let ty = attribute_type(db, call, &attribute.r#type)?;
            let name = Name::new(attribute.name);
            let parameter = Parameter::keyword_only(name.clone()).with_annotated_type(ty);
            let parameter = if attribute.is_mandatory {
                parameter
            } else {
                parameter.with_default_type(Type::unknown())
            };
            Some((name, parameter))
        })
        .collect::<Option<Vec<_>>>()?;
    match call.argument("attrs") {
        CheckedArgument::Omitted => {}
        CheckedArgument::Indeterminate => return None,
        CheckedArgument::Value { ty: _, expression } => {
            let Expr::Dict(dictionary) = expression? else {
                // Initializer provenance does not prove an alias's current mapping.
                return None;
            };
            let mut sources = FxHashMap::default();
            for item in &dictionary.items {
                let key = item.key.as_ref()?;
                let name = call.expression_type(key)?.string_literal_value(db)?;
                let source = *sources
                    .entry(name)
                    .or_insert_with(|| FileRange::new(call.file().file(db), key.range()));
                if name.starts_with('_') {
                    continue;
                }
                let value = call.expression_type(&item.value)?;
                let attribute = value
                    .provided_data(db, &environment)
                    .and_then(|data| data.downcast_ref::<Attribute>());
                let mut parameter =
                    Parameter::keyword_only(Name::new(name)).with_source_range(source);
                if matches!(kind, RuleKind::Macro) && value == Type::none(db, &environment) {
                    // A removed macro attribute can be omitted, but cannot accept a value.
                    parameter = parameter
                        .with_annotated_type(Type::Never)
                        .with_default_type(Type::unknown());
                } else if let Some(attribute) = attribute {
                    let ty = attribute_type(db, call, &attribute.kind)?;
                    parameter = parameter.with_annotated_type(ty);
                    if let Some(doc) = &attribute.documentation {
                        documentation
                            .parameters
                            .retain(|(existing, _)| existing.as_str() != name);
                        documentation
                            .parameters
                            .push((Name::new(name), doc.clone()));
                    }
                    if !attribute.mandatory {
                        parameter = match attribute.default {
                            Some(source) => {
                                parameter.with_default(ParameterDefault::Source { ty, source })
                            }
                            None => parameter.with_default_type(Type::unknown()),
                        };
                    }
                } else {
                    // Keep a known attribute name navigable during recovery. An
                    // invalid descriptor supplies no type or requiredness contract.
                    parameter = parameter.with_default_type(Type::unknown());
                }
                if let Some(existing) = parameters
                    .iter()
                    .position(|(existing, _)| existing.as_str() == name)
                {
                    parameters[existing].1 = parameter;
                } else {
                    parameters.push((Name::new(name), parameter));
                }
            }
        }
    }
    let mut parameters: Vec<_> = parameters
        .into_iter()
        .map(|(_, parameter)| parameter)
        .collect();
    if matches!(kind, RuleKind::Macro) {
        // Macros may forward inherited attributes whose names are not declared locally.
        parameters.push(Parameter::keyword_variadic(Name::new("kwargs")));
    }
    let callable = Type::single_callable(
        db,
        Signature::new(
            Parameters::standard(parameters),
            Type::none(db, &environment),
        ),
    );
    callable.with_callable_data(db, ProvidedData::new(documentation))
}

fn attribute_type<'db>(
    db: &'db Database,
    call: &CheckedCall<'_, 'db>,
    kind: &AttributeKind,
) -> Option<Type<'db>> {
    let environment = ProgramEnvironment::from_file(call.file());
    let declaration = call.declaration()?;
    attribute_value_type(
        db,
        &environment,
        declaration.program_file(db),
        kind,
        AttributeUse::Input,
    )
}

pub(super) enum AttributeUse {
    Input,
    BuildContext,
    RepositoryContext,
}

pub(super) fn attribute_value_type<'db>(
    db: &'db Database,
    environment: &ProgramEnvironment<'db>,
    declarations: ProgramFile<'db>,
    kind: &AttributeKind,
    usage: AttributeUse,
) -> Option<Type<'db>> {
    let string = KnownClass::Str.to_instance(db, environment);
    let int = KnownClass::Int.to_instance(db, environment);
    let list = |element| KnownClass::List.to_specialized_instance(db, environment, &[element]);
    let dict =
        |key, value| KnownClass::Dict.to_specialized_instance(db, environment, &[key, value]);
    let label = || {
        let name = match usage {
            AttributeUse::Input => "Label",
            AttributeUse::BuildContext => "Target",
            AttributeUse::RepositoryContext => "Label",
        };
        let declaration = native_class(db, declarations, name)?;
        let label = declaration.to_instance_approximation(db, environment)?;
        Some(match usage {
            AttributeUse::Input => UnionType::from_elements(db, environment, [label, string]),
            AttributeUse::BuildContext => label,
            AttributeUse::RepositoryContext => label,
        })
    };
    let output = || match usage {
        AttributeUse::Input => Some(string),
        AttributeUse::BuildContext => {
            let class = native_class(db, declarations, "File")?;
            class.to_instance_approximation(db, environment)
        }
        AttributeUse::RepositoryContext => Some(Type::unknown()),
    };
    Some(match kind {
        AttributeKind::Bool => {
            let boolean = KnownClass::Bool.to_instance(db, environment);
            match usage {
                // Bazel converts only integer 0 and 1 at attribute inputs.
                AttributeUse::Input => UnionType::from_elements(
                    db,
                    environment,
                    [boolean, Type::int_literal(0), Type::int_literal(1)],
                ),
                AttributeUse::BuildContext => boolean,
                AttributeUse::RepositoryContext => boolean,
            }
        }
        AttributeKind::Int => int,
        AttributeKind::IntList => list(int),
        AttributeKind::String => string,
        AttributeKind::StringList => list(string),
        AttributeKind::StringDict => dict(string, string),
        AttributeKind::StringListDict => dict(string, list(string)),
        AttributeKind::Label => label()?,
        AttributeKind::LabelList => list(label()?),
        AttributeKind::LabelKeyedStringDict => dict(label()?, string),
        AttributeKind::StringKeyedLabelDict => dict(string, label()?),
        AttributeKind::Output => output()?,
        AttributeKind::OutputList => list(output()?),
    })
}

fn structure<'db>(db: &'db Database, call: &CheckedCall<'_, 'db>) -> Option<Type<'db>> {
    let environment = ProgramEnvironment::from_file(call.file());
    let mut fields = Vec::new();
    let mut has_dynamic_fields = false;
    for keyword in &call.call().arguments.keywords {
        match &keyword.arg {
            Some(name) => fields.push(ProvidedField {
                name: name.id.clone(),
                ty: call.expression_type(&keyword.value)?,
                source: Some(FileRange::new(call.file().file(db), name.range())),
            }),
            None => has_dynamic_fields = true,
        }
    }
    let class = call.class_type(
        db,
        ProvidedClass {
            name: Name::new("struct"),
            bases: declared_base(db, call, "struct"),
            class_members: Box::default(),
            instance_fields: ProvidedInstanceFields {
                fields: fields.into_boxed_slice(),
                has_dynamic_fields,
                data: None,
            },
        },
    );
    class.to_instance_approximation(db, &environment)
}

fn provider<'db>(db: &'db Database, call: &CheckedCall<'_, 'db>) -> Option<Type<'db>> {
    let environment = ProgramEnvironment::from_file(call.file());
    let mut fields: Vec<ProvidedField<'_>> = Vec::new();
    let mut open = false;
    let mut documentation = Documentation {
        text: doc_string(db, call),
        parameters: Vec::new(),
    };
    match call.argument("fields") {
        CheckedArgument::Omitted => open = true,
        CheckedArgument::Indeterminate => return None,
        CheckedArgument::Value { ty, expression } => {
            if ty.is_none(db) {
                open = true;
            } else {
                let names: Vec<_> = match expression? {
                    Expr::List(list) => list.elts.iter().collect(),
                    Expr::Tuple(tuple) => tuple.elts.iter().collect(),
                    Expr::Dict(dict) => {
                        for item in &dict.items {
                            let name = call
                                .expression_type(item.key.as_ref()?)?
                                .string_literal_value(db)?;
                            if let Some(doc) =
                                call.expression_type(&item.value)?.string_literal_value(db)
                            {
                                documentation.parameters.push((Name::new(name), doc.into()));
                            }
                        }
                        dict.items
                            .iter()
                            .map(|item| item.key.as_ref())
                            .collect::<Option<_>>()?
                    }
                    _ => return None,
                };
                for expression in names {
                    let ty = call.expression_type(expression)?;
                    let name = Name::new(ty.string_literal_value(db)?);
                    if !fields.iter().any(|field| field.name == name) {
                        fields.push(ProvidedField {
                            name,
                            ty: Type::unknown(),
                            source: Some(FileRange::new(call.file().file(db), expression.range())),
                        });
                    }
                }
            }
        }
    }
    let mut parameters: Vec<_> = fields
        .iter()
        .map(|ProvidedField { name, ty, source }| {
            let parameter = Parameter::keyword_only(name.clone())
                .with_annotated_type(*ty)
                .with_default_type(Type::unknown());
            match source {
                Some(source) => parameter.with_source_range(*source),
                None => parameter,
            }
        })
        .collect();
    if open {
        parameters.push(Parameter::keyword_variadic(Name::new("kwargs")));
    }
    let initializer = match call.argument("init") {
        CheckedArgument::Omitted => None,
        CheckedArgument::Indeterminate => return None,
        CheckedArgument::Value { ty, expression: _ } => (!ty.is_none(db)).then_some(ty),
    };
    let self_parameter = || Parameter::positional_only(Some(Name::new("self")));
    let init = if let Some(initializer) = initializer {
        initializer.map_callable_signatures(
            db,
            &environment,
            CallableTypeKind::FunctionLike,
            |signature| {
                let parameters =
                    std::iter::once(self_parameter()).chain(signature.parameters().iter().cloned());
                let parameters = Parameters::from_annotation(db, parameters);
                signature
                    .with_parameters(parameters)
                    .with_return_type(Type::none(db, &environment))
            },
        )?
    } else {
        Type::function_like_callable(
            db,
            Signature::new(
                Parameters::standard(std::iter::once(self_parameter()).chain(parameters.clone())),
                Type::none(db, &environment),
            ),
        )
    };
    let parsed = ruff_db::parsed::parsed_module(db, call.file().python_file(db)).load(db);
    let node = covering_node(parsed.syntax().into(), call.call().range());
    let name = node.parent().and_then(|parent| {
        let AnyNodeRef::StmtAssign(assignment) = parent else {
            return None;
        };
        let [target] = assignment.targets.as_slice() else {
            return None;
        };
        let target = match target {
            Expr::Tuple(tuple) => {
                initializer?;
                tuple.elts.first()?
            }
            _ => target,
        };
        let Expr::Name(name) = target else {
            return None;
        };
        Some(name.id.clone())
    });
    let class = call.class_type(
        db,
        ProvidedClass {
            name: name.unwrap_or_else(|| Name::new("provider")),
            bases: Box::default(),
            class_members: Box::from([(Name::new("__init__"), init)]),
            instance_fields: ProvidedInstanceFields {
                fields: fields.into_boxed_slice(),
                has_dynamic_fields: open,
                data: Some(ProvidedData::new(documentation.clone())),
            },
        },
    );
    if initializer.is_some() {
        let instance = class.to_instance_approximation(db, &environment)?;
        let raw = Type::single_callable(
            db,
            Signature::new(Parameters::standard(parameters), instance),
        )
        .with_callable_data(db, ProvidedData::new(documentation))?;
        Some(Type::heterogeneous_tuple(db, &environment, [class, raw]))
    } else {
        Some(class)
    }
}

fn declared_base<'db>(
    db: &'db Database,
    call: &CheckedCall<'_, 'db>,
    name: &str,
) -> Box<[Type<'db>]> {
    declared_class(db, call, name).into_iter().collect()
}

fn declared_class<'db>(
    db: &'db Database,
    call: &CheckedCall<'_, 'db>,
    name: &str,
) -> Option<Type<'db>> {
    let declaration = call.declaration()?;
    native_class(db, declaration.program_file(db), name)
}

pub(super) fn native_class<'db>(
    db: &'db Database,
    file: ProgramFile<'db>,
    name: &str,
) -> Option<Type<'db>> {
    ProvidedBindingValue::Export {
        file,
        name: Name::new(format!("_starpls_annotation_{name}")),
    }
    .resolve_type(db)
}

#[cfg(test)]
mod tests {
    use ty_python_semantic::HasType;
    use ty_python_semantic::SemanticModel;

    use super::*;
    use crate::Analysis;
    use crate::FilePosition;

    #[test]
    fn factories_keep_identity_and_callable_fields() {
        let (mut analysis, _) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                starpls_bazel::Builtins::default(),
            )
            .unwrap();
        let file = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            r#"
def increment(value):
    # type: (int) -> int
    return value + 1
make = struct
record = make(callback=increment)
result = record.callback(1)
record.callback("bad")
def consume_record(value):
    # type: (struct) -> None
    pass
def consume_attribute(value):
    # type: (Attribute) -> None
    pass
consume_record(record)
consume_attribute(attr.string())
factory = provider
First = factory(fields=["value"])
Second = factory(fields=["value"])
def consume(value):
    # type: (First) -> None
    pass
consume(First(value=1))
consume(Second(value=1))
def shadowed():
    # type: () -> int
    def struct(value):
        # type: (int) -> int
        return value
    return struct(1)
shadow = shadowed()
"#,
        );
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let file = db.starlark_program_file(file);
        let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
        let model = SemanticModel::new(db, file);
        for statement in parsed.suite() {
            let Stmt::Assign(assignment) = statement else {
                continue;
            };
            if assignment.targets.iter().any(|target| match target {
                Expr::Name(name) => name.id == "result" || name.id == "shadow",
                _ => false,
            }) {
                let ty = assignment.value.inferred_type(&model).unwrap();
                assert_eq!(
                    ty.display(db, &model.program_environment()).to_string(),
                    "int"
                );
            }
        }
        let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.id().to_string())
            .collect();
        assert_eq!(
            ids,
            ["invalid-argument-type", "invalid-argument-type"],
            "{diagnostics:?}"
        );
    }

    #[test]
    fn provider_initializer_keeps_its_first_argument() {
        let (mut analysis, _) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                starpls_bazel::Builtins::default(),
            )
            .unwrap();
        fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            r#"
def initialize(value, optional=0):
    # type: (int, int) -> dict
    return {"field": value}
Info, raw = provider(doc="Provider documentation", fields={"field": "Field documentation"}, init=initialize)
Info(1$0)
raw(field=1)
"#,
        );
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let snapshot = analysis.snapshot();
        let help = snapshot
            .signature_help(FilePosition { file_id, pos })
            .unwrap()
            .unwrap();
        let [signature] = help.signatures.as_slice() else {
            panic!("{help:?}");
        };
        assert_eq!(signature.active_parameter, Some(0));
        assert!(signature.label.starts_with("def Info("), "{help:?}");
        assert_eq!(
            signature.documentation.as_deref(),
            Some("Provider documentation  ")
        );
        let labels: Vec<_> = signature
            .parameters
            .as_ref()
            .unwrap()
            .iter()
            .map(|parameter| parameter.label.as_str())
            .collect();
        assert_eq!(labels, ["value: int", "optional: int = 0"], "{help:?}");
        let diagnostics = ty_python_semantic::check_file_unwrap(
            &snapshot.db,
            snapshot.db.starlark_program_file(file_id),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn alias_mapping_is_not_claimed_to_be_a_closed_rule_schema() {
        let (mut analysis, _) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                starpls_bazel::Builtins::default(),
            )
            .unwrap();
        fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            r#"
def implementation(ctx): pass
attrs = {"before": attr.string()}
attrs["after"] = attr.int()
target = rule(implementation, attrs=attrs)
target($0)
"#,
        );
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let help = analysis
            .snapshot()
            .signature_help(FilePosition { file_id, pos })
            .unwrap();
        if let Some(help) = help {
            for signature in &help.signatures {
                assert!(!signature.label.contains("before"), "{help:?}");
                assert!(!signature.label.contains("after"), "{help:?}");
            }
        }
    }
}
