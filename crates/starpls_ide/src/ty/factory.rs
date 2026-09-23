//! Native factories supply declarations to Ty after its ordinary call checking.

use ruff_db::files::FilePath;
use ruff_db::files::FileRange;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::name::Name;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::Expr;
use ruff_python_ast::HasNodeIndex;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use starpls_bazel::attr::AttributeKind;
use starpls_hir::Db as _;
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
use ty_python_semantic::types::DictionaryItem;
use ty_python_semantic::types::DictionaryItems;
use ty_python_semantic::types::KnownClass;
use ty_python_semantic::types::Parameter;
use ty_python_semantic::types::ParameterDefault;
use ty_python_semantic::types::ParameterKind;
use ty_python_semantic::types::Parameters;
use ty_python_semantic::types::Signature;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::UnionType;
use ty_python_semantic::ProgramEnvironment;

use crate::Database;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Attribute {
    pub(super) kind: AttributeKind,
    pub(super) single_file: Option<bool>,
    pub(super) executable: Option<bool>,
    pub(super) configuration: AttributeConfiguration,
    configurability: Configurability,
    mandatory: bool,
    default: Option<FileRange>,
    default_value: DefaultValue,
    documentation: Option<Box<str>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DefaultValue {
    None,
    NonNone,
    Unknown,
}

#[derive(Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) struct RuleData {
    documentation: Documentation,
    attributes: Box<[(Name, Attribute)]>,
}

impl RuleData {
    pub(super) fn parameter_type<'db>(
        &self,
        db: &'db Database,
        environment: &ProgramEnvironment<'db>,
        declarations: ProgramFile<'db>,
        name: &str,
    ) -> Option<Type<'db>> {
        let (_, attribute) = self.attributes.iter().find(|(key, _)| key == name)?;
        let none = Type::none(db, environment);
        if name.starts_with('_') && attribute.default_value == DefaultValue::None {
            return Some(none);
        }
        let value = attribute_value_type(
            db,
            environment,
            declarations,
            &attribute.kind,
            AttributeUse::MacroContext,
        )?;
        let selected = || {
            // A selected None branch survives conversion into a macro body.
            let alternatives = if name.starts_with('_') {
                value
            } else {
                UnionType::from_elements(db, environment, [value, none])
            };
            specialized_native_instance(db, environment, declarations, "select", alternatives)
        };
        let value = match attribute.configurable() {
            Some(configurable) => {
                if configurable {
                    selected()?
                } else {
                    value
                }
            }
            None => {
                let selected = selected()?;
                UnionType::from_elements(db, environment, [value, selected])
            }
        };
        Some(if !attribute.has_non_none_value() {
            UnionType::from_elements(db, environment, [value, none])
        } else {
            value
        })
    }
}

struct RuleAttribute<'db> {
    name: Name,
    parameter: Option<Parameter<'db>>,
    descriptor: Option<Attribute>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum AttributeConfiguration {
    Ordinary,
    Starlark,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Configurability {
    Default,
    Explicit(bool),
    Unknown,
}

impl Attribute {
    pub(super) fn has_non_none_value(&self) -> bool {
        self.mandatory || self.default_value == DefaultValue::NonNone
    }

    fn configurable(&self) -> Option<bool> {
        if matches!(self.kind, AttributeKind::Output | AttributeKind::OutputList) {
            return Some(false);
        }
        match self.configurability {
            Configurability::Default => Some(true),
            Configurability::Explicit(value) => Some(value),
            Configurability::Unknown => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
struct StarlarkTransition;

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

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) struct ProviderData {
    pub(super) documentation: Documentation,
    /// None denotes an unrestricted field namespace.
    pub(super) fields: Option<Box<[Name]>>,
    pub(super) initializer: ProviderInitializer,
    pub(super) origin: FileRange,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) enum ProviderInitializer {
    None,
    Expression(FileRange),
    Unknown,
}

impl Documentation {
    pub(crate) fn from_data(data: &ProvidedData) -> Option<&Self> {
        data.downcast_ref::<Self>()
            .or_else(|| {
                data.downcast_ref::<RuleData>()
                    .map(|rule| &rule.documentation)
            })
            .or_else(|| {
                data.downcast_ref::<ProviderData>()
                    .map(|provider| &provider.documentation)
            })
    }
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
    Transition,
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
        let Stmt::FunctionDef(candidate) = statement else {
            return false;
        };
        candidate.range() == function.range()
    }) {
        return None;
    }
    match function.name.as_str() {
        "rule" => Some(Factory::Rule { repository: false }),
        "repository_rule" => Some(Factory::Rule { repository: true }),
        "macro" => Some(Factory::Macro),
        "struct" => Some(Factory::Struct),
        "provider" => Some(Factory::Provider),
        "transition" => Some(Factory::Transition),
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
        Factory::Transition => {
            let environment = ProgramEnvironment::from_file(call.file());
            call.class_type(
                db,
                ProvidedClass {
                    name: Name::new("transition"),
                    bases: declared_base(db, call, "transition"),
                    class_members: Box::default(),
                    instance_fields: ProvidedInstanceFields {
                        fields: Box::default(),
                        has_dynamic_fields: false,
                        data: Some(ProvidedData::new(StarlarkTransition)),
                    },
                },
            )
            .to_instance_approximation(db, &environment)
        }
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
    let environment = ProgramEnvironment::from_file(call.file());
    let mandatory = match call.argument("mandatory") {
        CheckedArgument::Omitted => false,
        CheckedArgument::Value { ty, expression: _ } => ty.as_bool_literal()?,
        CheckedArgument::Indeterminate => return None,
    };
    // Output descriptors expose no default parameter in Bazel's API.
    let default = match kind {
        AttributeKind::Output => CheckedArgument::Omitted,
        AttributeKind::OutputList => CheckedArgument::Omitted,
        _ => call.argument("default"),
    };
    let (default, default_value) = match default {
        CheckedArgument::Omitted => (
            None,
            if matches!(kind, AttributeKind::Label | AttributeKind::Output) {
                DefaultValue::None
            } else {
                DefaultValue::NonNone
            },
        ),
        CheckedArgument::Value { ty, expression } => {
            let expression = expression?;
            let file = call.file();
            let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
            let source = FileRange::new(
                file.file(db),
                starpls_syntax::source::expr_range(expression, call.call().into(), parsed.tokens()),
            );
            let declaration = call.declaration()?;
            let input = attribute_value_type(
                db,
                &environment,
                declaration.program_file(db),
                &kind,
                AttributeUse::Input,
            )?;
            let default_value = if ty.is_none(db) {
                DefaultValue::None
            } else if Type::none(db, &environment).is_assignable_to(db, &environment, ty) {
                DefaultValue::Unknown
            } else if ty.is_assignable_to(db, &environment, input) {
                DefaultValue::NonNone
            } else {
                // Computed and late-bound defaults do not expose their value here.
                DefaultValue::Unknown
            };
            (Some(source), default_value)
        }
        CheckedArgument::Indeterminate => return None,
    };
    let flag = |name| match call.argument(name) {
        CheckedArgument::Omitted => Some(false),
        CheckedArgument::Value { ty, expression: _ } => ty.as_bool_literal(),
        CheckedArgument::Indeterminate => None,
    };
    let single_file = match call.argument("allow_single_file") {
        CheckedArgument::Value { ty, expression: _ } => {
            // Bazel sets SINGLE_ARTIFACT for every non-None value, including False.
            let list =
                KnownClass::List.to_specialized_instance(db, &environment, &[Type::unknown()]);
            let boolean = KnownClass::Bool.to_instance(db, &environment);
            if ty.is_none(db) {
                Some(false)
            } else if ty.as_bool_literal().is_some()
                || (matches!(ty, Type::NominalInstance(_))
                    && (ty.is_assignable_to(db, &environment, list)
                        || ty.is_assignable_to(db, &environment, boolean)))
            {
                Some(true)
            } else {
                None
            }
        }
        CheckedArgument::Omitted => Some(false),
        CheckedArgument::Indeterminate => None,
    };
    let configuration = match call.argument("cfg") {
        CheckedArgument::Omitted => AttributeConfiguration::Ordinary,
        CheckedArgument::Value { ty, expression: _ } => {
            if ty == Type::none(db, &environment)
                || matches!(ty.string_literal_value(db), Some("target" | "exec"))
            {
                AttributeConfiguration::Ordinary
            } else if ty
                .provided_data(db, &environment)
                .is_some_and(|data| data.downcast_ref::<StarlarkTransition>().is_some())
            {
                AttributeConfiguration::Starlark
            } else {
                AttributeConfiguration::Unknown
            }
        }
        CheckedArgument::Indeterminate => AttributeConfiguration::Unknown,
    };
    let configurability = match call.argument("configurable") {
        CheckedArgument::Omitted => Configurability::Default,
        CheckedArgument::Value { ty, expression: _ } => match ty.as_bool_literal() {
            Some(value) => Configurability::Explicit(value),
            None => Configurability::Unknown,
        },
        CheckedArgument::Indeterminate => Configurability::Unknown,
    };
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
                    single_file,
                    executable: flag("executable"),
                    configuration,
                    configurability,
                    mandatory,
                    default,
                    default_value,
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
    let inherit_common = matches!(call.argument("inherit_attrs"), CheckedArgument::Value { ty, expression: _ }
        if ty.string_literal_value(db) == Some("common"));
    let common = match kind {
        RuleKind::Build => common.build,
        RuleKind::Repository => common.repository,
        RuleKind::Macro => common
            .build
            .into_iter()
            .filter(|attribute| {
                inherit_common || matches!(attribute.name.as_str(), "name" | "visibility")
            })
            .collect(),
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
    let mut attributes = common
        .into_iter()
        .map(|attribute| {
            let ty = attribute_type(db, call, &attribute.r#type)?;
            let ty = if attribute.configurable && !matches!(kind, RuleKind::Repository) {
                configurable_input(db, call, ty)?
            } else {
                ty
            };
            let name = Name::new(&attribute.name);
            let parameter = Parameter::keyword_only(name.clone()).with_annotated_type(ty);
            let parameter = if attribute.is_mandatory {
                parameter
            } else {
                let parameter = optional_attribute(db, &environment, parameter)
                    .with_default_type(Type::unknown());
                if matches!(kind, RuleKind::Macro) {
                    parameter.with_default_type(Type::none(db, &environment))
                } else {
                    parameter
                }
            };
            Some(RuleAttribute {
                name,
                parameter: Some(parameter),
                descriptor: Some(Attribute {
                    kind: attribute.r#type,
                    single_file: Some(false),
                    executable: Some(false),
                    configuration: AttributeConfiguration::Ordinary,
                    configurability: Configurability::Explicit(attribute.configurable),
                    mandatory: attribute.is_mandatory,
                    default: None,
                    default_value: if matches!(kind, RuleKind::Macro)
                        && !attribute.is_mandatory
                        && attribute.name != "visibility"
                    {
                        DefaultValue::None
                    } else {
                        DefaultValue::NonNone
                    },
                    documentation: Some(attribute.doc.into_boxed_str()),
                }),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let mut complete = !matches!(kind, RuleKind::Macro)
        || inherit_common
        || inherit_macro_attributes(db, call, &mut attributes, &mut documentation);
    let own_attributes = match call.argument("attrs") {
        CheckedArgument::Omitted => Some(DictionaryItems {
            items: Box::default(),
            is_complete: true,
        }),
        CheckedArgument::Value {
            ty: _,
            expression: _,
        } => call.dictionary_argument("attrs"),
        CheckedArgument::Indeterminate => None,
    };
    let DictionaryItems { items, is_complete } = own_attributes.unwrap_or(DictionaryItems {
        items: Box::default(),
        is_complete: false,
    });
    complete &= is_complete;
    if matches!(kind, RuleKind::Macro) && !is_complete {
        // Unknown own attributes can override or remove any inherited contract.
        for RuleAttribute {
            name,
            parameter,
            descriptor,
        } in &mut attributes
        {
            if !matches!(name.as_str(), "name" | "visibility") {
                *parameter = parameter.clone().map(|parameter| {
                    parameter
                        .with_annotated_type(Type::unknown())
                        .with_default_type(Type::unknown())
                });
                *descriptor = None;
            }
        }
    }
    for DictionaryItem {
        name,
        ty: value,
        source,
    } in items
    {
        let source = FileRange::new(call.file().file(db), source);
        let name = name.as_str();
        if matches!(kind, RuleKind::Macro) && matches!(name, "name" | "visibility") {
            continue;
        }
        documentation
            .parameters
            .retain(|(existing, _)| existing.as_str() != name);
        if complete && matches!(kind, RuleKind::Macro) && value.is_none(db) {
            attributes.retain(|attribute| attribute.name.as_str() != name);
            continue;
        }
        let attribute = value
            .provided_data(db, &environment)
            .and_then(|data| data.downcast_ref::<Attribute>());
        let mut parameter = Parameter::keyword_only(Name::new(name)).with_source_range(source);
        if !is_complete {
            parameter = parameter
                .with_annotated_type(Type::unknown())
                .with_default_type(Type::unknown());
        } else if matches!(kind, RuleKind::Macro) && value == Type::none(db, &environment) {
            // A removed macro attribute can be omitted, but cannot accept a value.
            parameter = parameter
                .with_annotated_type(Type::Never)
                .with_default_type(Type::unknown());
        } else if let Some(attribute) = attribute {
            let ty = attribute_type(db, call, &attribute.kind)?;
            let ty = if !matches!(kind, RuleKind::Repository)
                && attribute.configurable() != Some(false)
            {
                configurable_input(db, call, ty)?
            } else {
                ty
            };
            parameter = parameter.with_annotated_type(ty);
            if let Some(doc) = &attribute.documentation {
                documentation
                    .parameters
                    .push((Name::new(name), doc.clone()));
            }
            if !attribute.mandatory {
                parameter = match attribute.default {
                    Some(source) => parameter.with_default(ParameterDefault::Source { ty, source }),
                    None => parameter.with_default_type(Type::unknown()),
                };
                parameter = optional_attribute(db, &environment, parameter);
            }
        } else {
            // Keep a known attribute name navigable during recovery. An
            // invalid descriptor supplies no type or requiredness contract.
            parameter = parameter.with_default_type(Type::unknown());
        }
        let parameter = if name.starts_with('_') {
            // An own private macro keyword may explicitly omit its value.
            matches!(kind, RuleKind::Macro).then(|| {
                parameter
                    .with_annotated_type(Type::none(db, &environment))
                    .with_default_type(Type::none(db, &environment))
            })
        } else {
            Some(parameter)
        };
        let attribute = RuleAttribute {
            name: Name::new(name),
            parameter,
            descriptor: if is_complete {
                attribute.cloned()
            } else {
                None
            },
        };
        if let Some(existing) = attributes
            .iter_mut()
            .find(|attribute| attribute.name.as_str() == name)
        {
            *existing = attribute;
        } else {
            attributes.push(attribute);
        }
    }
    let mut parameters = Vec::new();
    let mut descriptors = Vec::new();
    for RuleAttribute {
        name,
        parameter,
        descriptor,
    } in attributes
    {
        parameters.extend(parameter);
        if let Some(descriptor) = descriptor {
            descriptors.push((name, descriptor));
        }
    }
    if !complete {
        // Partial mappings and unknown parents can contain additional names.
        parameters.push(Parameter::keyword_variadic(Name::new("kwargs")));
    }
    let callable = Type::function_like_callable(
        db,
        Signature::new(
            Parameters::standard(
                std::iter::once(Parameter::positional_only(Some(Name::new("self"))))
                    .chain(parameters),
            ),
            Type::none(db, &environment),
        ),
    );
    let name = match kind {
        RuleKind::Build => "rule",
        RuleKind::Repository => "repository_rule",
        RuleKind::Macro => "macro",
    };
    call.class_type(
        db,
        ProvidedClass {
            name: Name::new(name),
            bases: declared_base(db, call, name),
            class_members: Box::from([(Name::new("__call__"), callable)]),
            instance_fields: ProvidedInstanceFields {
                fields: Box::default(),
                has_dynamic_fields: false,
                data: Some(ProvidedData::new(RuleData {
                    documentation,
                    attributes: descriptors.into_boxed_slice(),
                })),
            },
        },
    )
    .to_instance_approximation(db, &environment)
}

/// Compose the parent's public attributes from its existing callable contract.
fn inherit_macro_attributes<'db>(
    db: &'db Database,
    call: &CheckedCall<'_, 'db>,
    attributes: &mut Vec<RuleAttribute<'db>>,
    documentation: &mut Documentation,
) -> bool {
    let parent = match call.argument("inherit_attrs") {
        CheckedArgument::Omitted => return true,
        CheckedArgument::Indeterminate => return false,
        CheckedArgument::Value { ty, expression: _ } => {
            if ty.is_none(db) {
                return true;
            }
            ty
        }
    };
    let environment = ProgramEnvironment::from_file(call.file());
    if !["rule", "macro"].into_iter().any(|name| {
        declared_class(db, call, name)
            .and_then(|class| class.to_instance_approximation(db, &environment))
            .is_some_and(|base| parent.is_subtype_of(db, &environment, base))
    }) {
        return false;
    }
    let mut signatures = Vec::new();
    parent.map_callable_signatures(db, &environment, CallableTypeKind::Regular, |signature| {
        signatures.push(signature.clone());
        signature
    });
    let [signature] = signatures.as_slice() else {
        return false;
    };
    let parent_documentation = parent
        .provided_data(db, &environment)
        .and_then(Documentation::from_data);
    let parent_data = parent
        .provided_data(db, &environment)
        .and_then(|data| data.downcast_ref::<RuleData>());
    let definition = parent
        .definition(db, &environment)
        .and_then(|definition| definition.definition());
    let native = definition.is_some_and(|definition| {
        matches!(definition.program_file(db).file(db).path(db), FilePath::SystemVirtual(path) if path.as_str().starts_with("starpls-native:"))
    });
    let docstring = definition
        .and_then(|definition| definition.docstring(db))
        .map(ty_ide::Docstring::new);
    let parameter_docs = docstring
        .as_ref()
        .map(ty_ide::Docstring::parameter_documentation)
        .unwrap_or_default();
    let mut complete = true;
    for parameter in signature.parameters().iter() {
        let ParameterKind::KeywordOnly { name, default_type } = parameter.kind() else {
            // Native fallback signatures retain nominal identity but have no schema.
            complete = false;
            continue;
        };
        if name.starts_with('_') || matches!(name.as_str(), "name" | "visibility") {
            continue;
        }
        // Bazel's build-language metadata omits is_documented. These three
        // native bookkeeping attributes are explicitly undocumented in Bazel.
        if native
            && matches!(
                name.as_str(),
                "generator_name" | "generator_function" | "generator_location"
            )
        {
            continue;
        }
        let parameter = if default_type.is_some() {
            optional_attribute(db, &environment, parameter.clone())
                .with_default_type(Type::none(db, &environment))
        } else {
            parameter.clone()
        };
        let doc = parent_documentation
            .and_then(|doc| {
                doc.parameters
                    .iter()
                    .find_map(|(key, doc)| (key == name).then(|| doc.clone()))
            })
            .or_else(|| {
                parameter_docs.get(name.as_str()).map(|doc| {
                    ty_ide::DocstringFragment::new(doc)
                        .render(ty_ide::MarkupKind::PlainText)
                        .into_boxed_str()
                })
            });
        if let Some(doc) = doc {
            documentation.parameters.push((name.clone(), doc));
        }
        let mut descriptor = parent_data
            .and_then(|data| data.attributes.iter().find(|(key, _)| key == name))
            .map(|(_, attribute)| attribute.clone())
            .or_else(|| inherited_native_attribute(db, definition?, name.as_str()));
        if let Some(descriptor) = &mut descriptor {
            if !descriptor.mandatory {
                descriptor.default = None;
                descriptor.default_value = DefaultValue::None;
            }
        }
        attributes.push(RuleAttribute {
            name: name.clone(),
            parameter: Some(parameter),
            descriptor,
        });
    }
    complete
}

fn optional_attribute<'db>(
    db: &'db Database,
    environment: &ProgramEnvironment<'db>,
    parameter: Parameter<'db>,
) -> Parameter<'db> {
    let ty = parameter.annotated_type();
    // An incomplete parent may retain a removed name to reject it through **kwargs.
    if ty == Type::Never {
        return parameter;
    }
    parameter.with_annotated_type(UnionType::from_elements(
        db,
        environment,
        [ty, Type::none(db, environment)],
    ))
}

fn inherited_native_attribute(
    db: &Database,
    definition: Definition<'_>,
    name: &str,
) -> Option<Attribute> {
    let file = definition.program_file(db);
    let FilePath::SystemVirtual(path) = file.file(db).path(db) else {
        return None;
    };
    if !path.as_str().starts_with("starpls-native:") {
        return None;
    }
    let DefinitionKind::Class(class) = definition.kind(db) else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let class = class.node(&parsed);
    // Native rule classes are generated at module scope; the similarly named
    // nested rule base is only a fallback with no attribute schema.
    if !parsed.suite().iter().any(|statement| matches!(statement, Stmt::ClassDef(candidate) if candidate.range() == class.range())) { return None; }
    let rules = db
        .get_builtin_defs(&starpls_common::Dialect::Bazel)
        .rules(db);
    let rule = rules
        .rule
        .iter()
        .find(|rule| rule.name == class.name.as_str())?;
    let attribute = rule
        .attribute
        .iter()
        .find(|attribute| attribute.name == name)?;
    use starpls_bazel::build::attribute::Discriminator;
    let kind = match attribute.r#type() {
        Discriminator::Boolean => AttributeKind::Bool,
        Discriminator::Integer => AttributeKind::Int,
        Discriminator::IntegerList => AttributeKind::IntList,
        Discriminator::String => AttributeKind::String,
        Discriminator::StringList => AttributeKind::StringList,
        Discriminator::StringDict => AttributeKind::StringDict,
        Discriminator::StringListDict => AttributeKind::StringListDict,
        Discriminator::Label => AttributeKind::Label,
        Discriminator::LabelList => AttributeKind::LabelList,
        Discriminator::LabelKeyedStringDict => AttributeKind::LabelKeyedStringDict,
        Discriminator::Output => AttributeKind::Output,
        Discriminator::OutputList => AttributeKind::OutputList,
        _ => return None,
    };
    Some(Attribute {
        kind,
        single_file: None,
        executable: None,
        configuration: AttributeConfiguration::Unknown,
        configurability: attribute
            .configurable
            .map_or(Configurability::Unknown, Configurability::Explicit),
        mandatory: attribute.mandatory(),
        default: None,
        default_value: DefaultValue::Unknown,
        documentation: attribute.documentation.as_deref().map(Into::into),
    })
}

fn configurable_input<'db>(
    db: &'db Database,
    call: &CheckedCall<'_, 'db>,
    value: Type<'db>,
) -> Option<Type<'db>> {
    let environment = ProgramEnvironment::from_file(call.file());
    let declaration = call.declaration()?;
    let alternatives =
        UnionType::from_elements(db, &environment, [value, Type::none(db, &environment)]);
    let selected = specialized_native_instance(
        db,
        &environment,
        declaration.program_file(db),
        "select",
        alternatives,
    )?;
    Some(UnionType::from_elements(
        db,
        &environment,
        [value, selected],
    ))
}

pub(super) fn specialized_native_instance<'db>(
    db: &'db Database,
    environment: &ProgramEnvironment<'db>,
    declarations: ProgramFile<'db>,
    name: &str,
    value: Type<'db>,
) -> Option<Type<'db>> {
    // Callers select generated declarations with exactly one type parameter.
    let class = native_class(db, declarations, name)?;
    let class = class.as_class_literal()?;
    let specialized = class.apply_specialization(db, |context| context.specialize(db, vec![value]));
    Type::from(specialized).to_instance_approximation(db, environment)
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

pub(super) enum AttributeUse<'db> {
    Input,
    BuildContext(Type<'db>),
    RepositoryContext,
    MacroContext,
}

pub(super) fn attribute_value_type<'db>(
    db: &'db Database,
    environment: &ProgramEnvironment<'db>,
    declarations: ProgramFile<'db>,
    kind: &AttributeKind,
    usage: AttributeUse<'db>,
) -> Option<Type<'db>> {
    let string = KnownClass::Str.to_instance(db, environment);
    let int = KnownClass::Int.to_instance(db, environment);
    // Bazel copies and converts list attribute inputs; context values are lists.
    let list = |element| {
        let class = match usage {
            AttributeUse::Input => KnownClass::Iterable,
            AttributeUse::BuildContext(_) => KnownClass::List,
            AttributeUse::RepositoryContext => KnownClass::List,
            AttributeUse::MacroContext => KnownClass::List,
        };
        class.to_specialized_instance(db, environment, &[element])
    };
    let dict = |key, value| {
        let class = match usage {
            AttributeUse::Input => KnownClass::Mapping,
            AttributeUse::BuildContext(_) => KnownClass::Dict,
            AttributeUse::RepositoryContext => KnownClass::Dict,
            AttributeUse::MacroContext => KnownClass::Dict,
        };
        class.to_specialized_instance(db, environment, &[key, value])
    };
    let label = || {
        if let AttributeUse::BuildContext(target) = usage {
            return Some(target);
        }
        let declaration = native_class(db, declarations, "Label")?;
        let label = declaration.to_instance_approximation(db, environment)?;
        Some(match usage {
            AttributeUse::Input => UnionType::from_elements(db, environment, [label, string]),
            AttributeUse::BuildContext(_) => label,
            AttributeUse::RepositoryContext => label,
            AttributeUse::MacroContext => label,
        })
    };
    let output = || match usage {
        AttributeUse::Input => label(),
        AttributeUse::BuildContext(_) => {
            let class = native_class(db, declarations, "Label")?;
            class.to_instance_approximation(db, environment)
        }
        AttributeUse::RepositoryContext => Some(Type::unknown()),
        AttributeUse::MacroContext => label(),
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
                AttributeUse::BuildContext(_) => boolean,
                AttributeUse::RepositoryContext => boolean,
                AttributeUse::MacroContext => boolean,
            }
        }
        AttributeKind::Int => int,
        AttributeKind::IntList => list(int),
        AttributeKind::String => string,
        AttributeKind::StringList => list(string),
        AttributeKind::StringDict => dict(string, string),
        AttributeKind::StringListDict => dict(string, list(string)),
        AttributeKind::Label => {
            let label = label()?;
            match usage {
                AttributeUse::BuildContext(_) => {
                    UnionType::from_elements(db, environment, [label, Type::none(db, environment)])
                }
                AttributeUse::Input => label,
                AttributeUse::MacroContext => label,
                AttributeUse::RepositoryContext => {
                    UnionType::from_elements(db, environment, [label, Type::none(db, environment)])
                }
            }
        }
        AttributeKind::LabelList => list(label()?),
        AttributeKind::LabelKeyedStringDict => {
            let keys = label()?;
            match usage {
                AttributeUse::Input => {
                    let label = native_class(db, declarations, "Label")?
                        .to_instance_approximation(db, environment)?;
                    // Mapping keys are invariant; inputs can use either or both kinds.
                    UnionType::from_elements(
                        db,
                        environment,
                        [
                            dict(label, string),
                            dict(string, string),
                            dict(keys, string),
                        ],
                    )
                }
                AttributeUse::BuildContext(_) => dict(keys, string),
                AttributeUse::RepositoryContext => dict(keys, string),
                AttributeUse::MacroContext => dict(keys, string),
            }
        }
        AttributeKind::StringKeyedLabelDict => dict(string, label()?),
        AttributeKind::Output => {
            let output = output()?;
            match usage {
                AttributeUse::BuildContext(_) => {
                    UnionType::from_elements(db, environment, [output, Type::none(db, environment)])
                }
                AttributeUse::Input => output,
                AttributeUse::RepositoryContext => output,
                AttributeUse::MacroContext => output,
            }
        }
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
    let initializer = match call.argument("init") {
        CheckedArgument::Omitted => None,
        CheckedArgument::Indeterminate => return None,
        CheckedArgument::Value { ty, expression: _ } => (!ty.is_none(db)).then_some(ty),
    };
    let name = provider_name(db, call, initializer.is_some());
    if let Some(class) = db.provider_interface(call.file(), call.call().node_index().load()) {
        if initializer.is_none() {
            return Some(class);
        }
        let fields = super::interface::provider_fields(db, class, &environment)?;
        let parameters = fields
            .into_iter()
            .map(|ProvidedField { name, ty, source }| {
                let parameter = Parameter::keyword_only(name).with_annotated_type(ty);
                match source {
                    Some(source) => parameter.with_source_range(source),
                    None => parameter,
                }
            });
        let instance = class.to_instance_approximation(db, &environment)?;
        let raw = Type::single_callable(
            db,
            Signature::new(Parameters::standard(parameters), instance),
        );
        return Some(Type::heterogeneous_tuple(db, &environment, [class, raw]));
    }
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
    let data = ProviderData {
        origin: FileRange::new(call.file().file(db), call.call().range()),
        documentation: documentation.clone(),
        fields: (!open).then(|| fields.iter().map(|field| field.name.clone()).collect()),
        initializer: match call.argument("init") {
            CheckedArgument::Value { ty, expression } => {
                if ty.is_none(db) {
                    ProviderInitializer::None
                } else {
                    expression.map_or(ProviderInitializer::Unknown, |expression| {
                        ProviderInitializer::Expression(FileRange::new(
                            call.file().file(db),
                            expression.range(),
                        ))
                    })
                }
            }
            CheckedArgument::Omitted => ProviderInitializer::None,
            CheckedArgument::Indeterminate => return None,
        },
    };
    let class = call.class_type(
        db,
        ProvidedClass {
            name: name.unwrap_or_else(|| Name::new("provider")),
            bases: Box::default(),
            class_members: Box::from([(Name::new("__init__"), init)]),
            instance_fields: ProvidedInstanceFields {
                fields: fields.into_boxed_slice(),
                has_dynamic_fields: open,
                data: Some(ProvidedData::new(data.clone())),
            },
        },
    );
    if initializer.is_some() {
        let instance = class.to_instance_approximation(db, &environment)?;
        let raw = Type::single_callable(
            db,
            Signature::new(Parameters::standard(parameters), instance),
        )
        .with_callable_data(db, ProvidedData::new(data))?;
        Some(Type::heterogeneous_tuple(db, &environment, [class, raw]))
    } else {
        Some(class)
    }
}

fn provider_name(db: &Database, call: &CheckedCall<'_, '_>, initialized: bool) -> Option<Name> {
    let parsed = ruff_db::parsed::parsed_module(db, call.file().python_file(db)).load(db);
    let node = covering_node(parsed.syntax().into(), call.call().range());
    node.parent().and_then(|parent| {
        let AnyNodeRef::StmtAssign(assignment) = parent else {
            return None;
        };
        let [target] = assignment.targets.as_slice() else {
            return None;
        };
        let target = match target {
            Expr::Tuple(tuple) => {
                if !initialized {
                    return None;
                }
                tuple.elts.first()?
            }
            _ => target,
        };
        let Expr::Name(name) = target else {
            return None;
        };
        Some(name.id.clone())
    })
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
                Default::default(),
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
                Default::default(),
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
            Some("Provider documentation")
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
    fn observed_rule_attributes_are_optional_and_gradual() {
        let (mut analysis, _) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            r#"
def implementation(ctx): pass
attrs = {"before": attr.string()}
attrs["after"] = attr.int(mandatory=True)
attrs.update({"before": attr.int()})
target = rule(implementation, attrs=attrs)
target(name="empty")
target(name="changed", before=1, after="unknown", extra=True)
target($0)
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
        assert!(signature.label.contains("before: Unknown ="), "{help:?}");
        assert!(signature.label.contains("after: Unknown ="), "{help:?}");
        assert!(signature.label.contains("**kwargs"), "{help:?}");
        let diagnostics = ty_python_semantic::check_file_unwrap(
            &snapshot.db,
            snapshot.db.starlark_program_file(file_id),
        );
        // The final incomplete call still requires the common name attribute.
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.id().to_string())
            .collect();
        assert_eq!(ids, ["missing-argument"], "{diagnostics:?}");
    }
}
