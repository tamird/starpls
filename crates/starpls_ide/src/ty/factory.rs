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
use ty_python_core::scope::NodeWithScopeKind;
use ty_python_core::ProgramFile;
use ty_python_semantic::provided::ProvidedBindingValue;
use ty_python_semantic::provided::ProvidedClass;
use ty_python_semantic::provided::ProvidedData;
use ty_python_semantic::provided::ProvidedField;
use ty_python_semantic::provided::ProvidedInstanceFields;
use ty_python_semantic::types::CallableTypeKind;
use ty_python_semantic::types::CheckedArgument;
use ty_python_semantic::types::CheckedCall;
use ty_python_semantic::types::DictionaryExtraItems;
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
    values: Option<ScalarValues>,
    documentation: Option<Box<str>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
enum ScalarValues {
    String(Box<[Box<str>]>),
    Int(Box<[i32]>),
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
    pub(super) attributes: Box<[RuleAttributeData]>,
    pub(super) complete: bool,
    pub(super) executable: Option<bool>,
    pub(super) build_setting: Option<BuildSetting>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) struct RuleAttributeData {
    pub(super) name: Name,
    pub(super) descriptor: Option<Attribute>,
    pub(super) source: Option<FileRange>,
}

impl RuleData {
    pub(super) fn parameter_type<'db>(
        &self,
        db: &'db Database,
        environment: &ProgramEnvironment<'db>,
        declarations: ProgramFile<'db>,
        name: &str,
    ) -> Option<Type<'db>> {
        let attribute = self.attributes.iter().find(|entry| entry.name == name)?;
        let attribute = attribute.descriptor.as_ref()?;
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
    source: Option<FileRange>,
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
    pub(super) fn finite_value_type<'db>(
        &self,
        db: &'db Database,
        environment: &ProgramEnvironment<'db>,
    ) -> Option<Type<'db>> {
        let Self {
            kind: _,
            single_file: _,
            executable: _,
            configuration: _,
            configurability: _,
            mandatory: _,
            default: _,
            default_value: _,
            values,
            documentation: _,
        } = self;
        let values = values.as_ref()?;
        Some(match values {
            ScalarValues::String(values) => UnionType::from_elements(
                db,
                environment,
                values
                    .iter()
                    .map(|value| Type::string_literal(db, value.as_ref())),
            ),
            ScalarValues::Int(values) => UnionType::from_elements(
                db,
                environment,
                values
                    .iter()
                    .map(|value| Type::int_literal(i64::from(*value))),
            ),
        })
    }

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(super) enum BuildSetting {
    Bool,
    Int,
    String { allow_multiple: Option<bool> },
    StringList,
}

impl BuildSetting {
    pub(super) const ATTRIBUTE_NAMES: [&str; 2] = ["build_setting_default", "help"];

    fn attribute_kind(self) -> AttributeKind {
        match self {
            Self::Bool => AttributeKind::Bool,
            Self::Int => AttributeKind::Int,
            Self::String { allow_multiple: _ } => AttributeKind::String,
            Self::StringList => AttributeKind::StringList,
        }
    }

    pub(super) fn attributes(self) -> [starpls_bazel::attr::Attribute; 2] {
        let [default, help] = Self::ATTRIBUTE_NAMES;
        [
            (
                default,
                self.attribute_kind(),
                "Default value of this build setting.",
                true,
            ),
            (
                help,
                AttributeKind::String,
                "Help text for this build setting.",
                false,
            ),
        ]
        .map(
            |(name, kind, doc, mandatory)| starpls_bazel::attr::Attribute {
                name: name.to_owned(),
                r#type: kind,
                doc: doc.to_owned(),
                default_value: String::new(),
                is_mandatory: mandatory,
                configurable: false,
            },
        )
    }

    pub(super) fn value_type<'db>(
        self,
        db: &'db Database,
        environment: &ProgramEnvironment<'db>,
    ) -> Type<'db> {
        let string = KnownClass::Str.to_instance(db, environment);
        let strings = || KnownClass::List.to_specialized_instance(db, environment, &[string]);
        match self {
            Self::Bool => KnownClass::Bool.to_instance(db, environment),
            Self::Int => KnownClass::Int.to_instance(db, environment),
            Self::String { allow_multiple } => match allow_multiple {
                Some(true) => strings(),
                Some(false) => string,
                None => UnionType::from_elements(db, environment, [string, strings()]),
            },
            Self::StringList => strings(),
        }
    }
}

impl get_size2::GetSize for Attribute {
    fn get_heap_size(&self) -> usize {
        let Self {
            kind: _,
            single_file: _,
            executable: _,
            configuration: _,
            configurability: _,
            mandatory: _,
            default: _,
            default_value: _,
            values,
            documentation,
        } = self;
        documentation.as_ref().map_or(0, |doc| doc.len())
            + get_size2::GetSize::get_heap_size(values)
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BuiltinFunction {
    Attribute(AttributeKind),
    BuildSetting(BuildSetting),
    Rule { repository: bool },
    Aspect,
    Macro,
    Struct,
    StructGetattr,
    Provider,
    Transition,
}

// Keep parse and lexical-owner dependencies behind a small, backdated declaration identity.
#[salsa::tracked(returns(clone))]
pub(super) fn declaration<'db>(
    db: &'db dyn ty_python_core::Db,
    definition: Definition<'db>,
) -> Option<BuiltinFunction> {
    let file = definition.program_file(db);
    let FilePath::SystemVirtual(path) = file.file(db).path(db) else {
        return None;
    };
    if !path.as_str().starts_with("starpls-native:") {
        return None;
    }
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let function = function.node(&parsed);
    let scope = definition.scope(db);
    let owner = scope.node(db);
    if let NodeWithScopeKind::Class(class) = owner {
        let parent = scope.scope(db).parent()?.to_scope_id(db, file);
        let NodeWithScopeKind::Class(namespace) = parent.node(db) else {
            return None;
        };
        if namespace.node(&parsed).name.as_str() != "_starpls_types" {
            return None;
        }
        return match class.node(&parsed).name.as_str() {
            "struct" => {
                (function.name.as_str() == "__getattr__").then_some(BuiltinFunction::StructGetattr)
            }
            "attr" => Some(BuiltinFunction::Attribute(attribute_kind(
                function.name.as_str(),
            )?)),
            "config" => Some(BuiltinFunction::BuildSetting(
                match function.name.as_str() {
                    "bool" => BuildSetting::Bool,
                    "int" => BuildSetting::Int,
                    "string" => BuildSetting::String {
                        allow_multiple: Some(false),
                    },
                    "string_list" => BuildSetting::StringList,
                    _ => return None,
                },
            )),
            _ => None,
        };
    }
    if !matches!(owner, NodeWithScopeKind::Module) {
        return None;
    }
    Some(match function.name.as_str() {
        "rule" => BuiltinFunction::Rule { repository: false },
        "repository_rule" => BuiltinFunction::Rule { repository: true },
        "aspect" => BuiltinFunction::Aspect,
        "macro" => BuiltinFunction::Macro,
        "struct" => BuiltinFunction::Struct,
        "provider" => BuiltinFunction::Provider,
        "transition" => BuiltinFunction::Transition,
        _ => return None,
    })
}

pub(super) fn result<'db>(db: &'db Database, call: &CheckedCall<'_, 'db>) -> Option<Type<'db>> {
    match declaration(db, call.declaration()?)? {
        BuiltinFunction::Attribute(kind) => attribute(db, call, kind),
        BuiltinFunction::BuildSetting(mut kind) => {
            if let BuildSetting::String { allow_multiple } = &mut kind {
                *allow_multiple = match call.argument("allow_multiple") {
                    CheckedArgument::Omitted => Some(false),
                    CheckedArgument::Value { ty, expression: _ } => ty.as_bool_literal(),
                    CheckedArgument::Indeterminate => None,
                };
            }
            descriptor(db, call, "BuildSetting", ProvidedData::new(kind))
        }
        BuiltinFunction::Rule { repository } => rule(
            db,
            call,
            if repository {
                RuleKind::Repository
            } else {
                RuleKind::Build
            },
        ),
        BuiltinFunction::Macro => rule(db, call, RuleKind::Macro),
        BuiltinFunction::Aspect => None,
        BuiltinFunction::Struct => structure(db, call),
        BuiltinFunction::StructGetattr => None,
        BuiltinFunction::Provider => provider(db, call),
        BuiltinFunction::Transition => descriptor(
            db,
            call,
            "transition",
            ProvidedData::new(StarlarkTransition),
        ),
    }
}

fn descriptor<'db>(
    db: &'db Database,
    call: &CheckedCall<'_, 'db>,
    name: &str,
    data: ProvidedData,
) -> Option<Type<'db>> {
    let environment = ProgramEnvironment::from_file(call.file());
    call.class_type(
        db,
        ProvidedClass {
            name: Name::new(name),
            bases: declared_base(db, call, name),
            class_members: Box::default(),
            instance_fields: ProvidedInstanceFields {
                fields: Box::default(),
                has_dynamic_fields: false,
                implications: Box::default(),
                data: Some(data),
            },
        },
    )
    .to_instance_approximation(db, &environment)
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
    let values = match kind {
        AttributeKind::String => scalar_values(call, mandatory, Box::from(""), |ty| {
            ty.string_literal_value(db).map(Box::from)
        })
        .map(ScalarValues::String),
        AttributeKind::Int => scalar_values(call, mandatory, 0, |ty| {
            let value = ty.as_int_literal()?;
            i32::try_from(value).ok()
        })
        .map(ScalarValues::Int),
        _ => None,
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
    descriptor(
        db,
        call,
        "Attribute",
        ProvidedData::new(Attribute {
            kind,
            single_file,
            executable: flag("executable"),
            configuration,
            configurability,
            mandatory,
            default,
            default_value,
            values,
            documentation: doc_string(db, call),
        }),
    )
}

fn scalar_values<'db, T>(
    call: &CheckedCall<'_, 'db>,
    mandatory: bool,
    implicit_default: T,
    literal: impl Fn(Type<'db>) -> Option<T>,
) -> Option<Box<[T]>> {
    let CheckedArgument::Value { ty: _, expression } = call.argument("values") else {
        return None;
    };
    let expression = expression?;
    let elements = match expression {
        Expr::List(list) => {
            let ruff_python_ast::ExprList {
                elts,
                ctx: _,
                range: _,
                node_index: _,
            } = list;
            elts
        }
        Expr::Tuple(tuple) => {
            let ruff_python_ast::ExprTuple {
                elts,
                ctx: _,
                range: _,
                node_index: _,
                parenthesized: _,
            } = tuple;
            elts
        }
        _ => return None,
    };
    // Bazel treats an empty values sequence as unrestricted. Its length must be known,
    // including when the inferred homogeneous element type happens to be a literal.
    if elements.is_empty() || elements.iter().any(Expr::is_starred_expr) {
        return None;
    }
    let mut values = elements
        .iter()
        .map(|element| {
            let ty = call.expression_type(element)?;
            literal(ty)
        })
        .collect::<Option<Vec<_>>>()?;
    if !mandatory {
        // Rule and repository defaults are filled after explicit allowed-value validation.
        let default = match call.argument("default") {
            CheckedArgument::Omitted => implicit_default,
            CheckedArgument::Value { ty, expression: _ } => literal(ty)?,
            CheckedArgument::Indeterminate => return None,
        };
        values.push(default);
    }
    Some(values.into_boxed_slice())
}

enum RuleKind {
    Build,
    Repository,
    Macro,
}

fn rule<'db>(db: &'db Database, call: &CheckedCall<'_, 'db>, kind: RuleKind) -> Option<Type<'db>> {
    let environment = ProgramEnvironment::from_file(call.file());
    let common = starpls_bazel::attr::make_common_attributes();
    let parent = if matches!(kind, RuleKind::Build) {
        match call.argument("parent") {
            CheckedArgument::Omitted => None,
            CheckedArgument::Value { ty, expression: _ } => (!ty.is_none(db)).then_some(ty),
            CheckedArgument::Indeterminate => Some(Type::unknown()),
        }
    } else {
        None
    };
    let parent_data = parent.and_then(|parent| {
        let base = declared_class(db, call, "rule")?;
        let base = base.to_instance_approximation(db, &environment)?;
        if !parent.is_subtype_of(db, &environment, base) {
            return None;
        }
        let data = parent.provided_data(db, &environment)?;
        data.downcast_ref::<RuleData>()
    });
    let test = if matches!(kind, RuleKind::Build) && parent.is_none() {
        any_enabled([
            boolean_argument(call, "test"),
            boolean_argument(call, "analysis_test"),
        ])
    } else {
        Some(false)
    };
    let executable = if parent.is_some() {
        parent_data.and_then(|data| data.executable)
    } else if matches!(kind, RuleKind::Build) {
        any_enabled([test, boolean_argument(call, "executable")])
    } else {
        Some(false)
    };
    let mut test_attributes = common.test;
    let inherit_common = matches!(call.argument("inherit_attrs"), CheckedArgument::Value { ty, expression: _ }
        if ty.string_literal_value(db) == Some("common"));
    let mut common = match kind {
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
    let build_setting = if matches!(kind, RuleKind::Build) {
        match call.argument("build_setting") {
            CheckedArgument::Omitted => None,
            CheckedArgument::Value { ty, expression: _ } => (!ty.is_none(db)).then_some(ty),
            CheckedArgument::Indeterminate => Some(Type::unknown()),
        }
    } else {
        None
    };
    let setting_kind = build_setting.and_then(|ty| {
        let data = ty.provided_data(db, &environment)?;
        data.downcast_ref::<BuildSetting>().copied()
    });
    if let Some(setting_kind) = setting_kind {
        common.extend(setting_kind.attributes());
    }
    if test == Some(true) {
        common.append(&mut test_attributes);
    }
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
                source: None,
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
                    values: None,
                    documentation: Some(attribute.doc.into_boxed_str()),
                }),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    if build_setting.is_some() && setting_kind.is_none() {
        // An unresolved descriptor may be None. Admit its possible attributes
        // without weakening other attributes or accepting arbitrary keywords.
        for name in BuildSetting::ATTRIBUTE_NAMES {
            let name = Name::new(name);
            attributes.push(RuleAttribute {
                parameter: Some(
                    Parameter::keyword_only(name.clone()).with_default_type(Type::unknown()),
                ),
                name,
                descriptor: None,
                source: None,
            });
        }
    }
    if test.is_none() {
        for attribute in test_attributes {
            let name = Name::new(attribute.name);
            attributes.push(RuleAttribute {
                parameter: Some(
                    Parameter::keyword_only(name.clone()).with_default_type(Type::unknown()),
                ),
                name,
                descriptor: None,
                source: None,
            });
        }
    }
    let mut complete = match kind {
        RuleKind::Build => match parent {
            Some(parent) => parent_data.is_some_and(|data| {
                inherit_rule_attributes(
                    db,
                    &environment,
                    parent,
                    data,
                    &mut attributes,
                    &mut documentation,
                )
            }),
            None => true,
        },
        RuleKind::Repository => true,
        RuleKind::Macro => {
            inherit_common
                || inherit_macro_attributes(db, call, &mut attributes, &mut documentation)
        }
    };
    let uncertain_parent = parent.is_some() && !complete;
    let own_attributes = match call.argument("attrs") {
        CheckedArgument::Omitted => Some(DictionaryItems {
            items: Box::default(),
            extra_items: DictionaryExtraItems::Closed,
        }),
        CheckedArgument::Value {
            ty: _,
            expression: _,
        } => call.dictionary_argument(db, "attrs"),
        CheckedArgument::Indeterminate => None,
    };
    let own_attributes = own_attributes.unwrap_or(DictionaryItems {
        items: Box::default(),
        extra_items: DictionaryExtraItems::Value(Type::unknown()),
    });
    let is_complete = own_attributes.is_complete();
    let DictionaryItems {
        items,
        extra_items: _,
    } = own_attributes;
    let is_complete = is_complete && items.iter().all(DictionaryItem::is_required);
    complete &= is_complete;
    if parent.is_some() && !is_complete {
        // Unseen overrides can replace a public label's default, but not its
        // inherited flags or requiredness.
        for attribute in &mut attributes {
            override_rule_attribute(attribute, None, None);
        }
    }
    if matches!(kind, RuleKind::Macro) && !is_complete {
        // Unknown own attributes can override or remove any inherited contract.
        for RuleAttribute {
            name,
            parameter,
            descriptor,
            source: _,
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
        kind: _,
    } in IntoIterator::into_iter(items).filter(DictionaryItem::is_required)
    {
        let source = FileRange::new(call.file().file(db), source);
        let name = name.as_str();
        if matches!(kind, RuleKind::Macro) && matches!(name, "name" | "visibility") {
            continue;
        }
        if setting_kind.is_some() && BuildSetting::ATTRIBUTE_NAMES.contains(&name) {
            // Bazel rejects declarations that collide with generated attributes.
            continue;
        }
        let attribute = value
            .provided_data(db, &environment)
            .and_then(|data| data.downcast_ref::<Attribute>());
        if parent.is_some() {
            if let Some(inherited) = attributes.iter_mut().find(|entry| entry.name == name) {
                if is_complete {
                    override_rule_attribute(inherited, attribute, Some(source));
                }
                continue;
            }
        }
        // An unseen parent may already declare this label attribute. Overrides
        // retain its flags, so the child's descriptor cannot establish them.
        let uncertain_attribute = attribute
            .filter(|attribute| {
                uncertain_parent
                    && !name.starts_with('_')
                    && matches!(
                        attribute.kind,
                        AttributeKind::Label | AttributeKind::LabelList
                    )
            })
            .map(|attribute| Attribute {
                kind: attribute.kind.clone(),
                single_file: None,
                executable: None,
                configuration: AttributeConfiguration::Unknown,
                configurability: Configurability::Unknown,
                mandatory: false,
                default: None,
                default_value: DefaultValue::Unknown,
                values: None,
                documentation: attribute.documentation.clone(),
            });
        let attribute = uncertain_attribute.as_ref().or(attribute);
        documentation
            .parameters
            .retain(|(existing, _)| existing.as_str() != name);
        if complete && matches!(kind, RuleKind::Macro) && value.is_none(db) {
            attributes.retain(|attribute| attribute.name.as_str() != name);
            continue;
        }
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
            source: Some(source),
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
        source,
    } in attributes
    {
        parameters.extend(parameter);
        descriptors.push(RuleAttributeData {
            name,
            descriptor,
            source,
        });
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
                implications: Box::default(),
                data: Some(ProvidedData::new(RuleData {
                    documentation,
                    attributes: descriptors.into_boxed_slice(),
                    complete,
                    executable,
                    build_setting: setting_kind,
                })),
            },
        },
    )
    .to_instance_approximation(db, &environment)
}

fn inherit_rule_attributes<'db>(
    db: &'db Database,
    environment: &ProgramEnvironment<'db>,
    parent: Type<'db>,
    data: &RuleData,
    attributes: &mut Vec<RuleAttribute<'db>>,
    documentation: &mut Documentation,
) -> bool {
    let Some(signature) = callable_signature(db, environment, parent) else {
        return false;
    };
    attributes.clear();
    documentation
        .parameters
        .clone_from(&data.documentation.parameters);
    let mut complete = data.complete;
    for RuleAttributeData {
        name,
        descriptor,
        source,
    } in &data.attributes
    {
        let parameter = if name.starts_with('_') {
            None
        } else {
            let parameter = signature
                .parameters()
                .iter()
                .find(|parameter| match parameter.kind() {
                    ParameterKind::KeywordOnly {
                        name: parameter_name,
                        default_type: _,
                    } => parameter_name == name,
                    _ => false,
                })
                .cloned();
            complete &= parameter.is_some();
            parameter
        };
        attributes.push(RuleAttribute {
            name: name.clone(),
            parameter,
            descriptor: descriptor.clone(),
            source: *source,
        });
    }
    complete
}

fn override_rule_attribute(
    inherited: &mut RuleAttribute<'_>,
    attribute: Option<&Attribute>,
    source: Option<FileRange>,
) {
    let RuleAttribute {
        name,
        parameter,
        descriptor,
        source: origin,
    } = inherited;
    let Some(parent) = descriptor else {
        return;
    };
    // Bazel permits only public Starlark label attributes to be overridden.
    // The parent retains all flags; only a non-null default and aspects change.
    if origin.is_none()
        || name.starts_with('_')
        || !matches!(parent.kind, AttributeKind::Label | AttributeKind::LabelList)
        || attribute.is_some_and(|attribute| parent.kind != attribute.kind)
    {
        return;
    }
    if let Some(source) = source {
        *origin = Some(source);
        if let Some(parameter) = parameter {
            *parameter = parameter.clone().with_source_range(source);
        }
    }
    if let Some(attribute) = attribute {
        if attribute.default_value == DefaultValue::None {
            return;
        }
        parent.default = attribute.default;
        parent.default_value = attribute.default_value;
    } else {
        // An unknown descriptor may supply a computed default returning None.
        parent.default = None;
        parent.default_value = DefaultValue::Unknown;
    }
    if !parent.mandatory {
        if let Some(parameter) = parameter {
            *parameter = match parent.default {
                Some(source) => parameter.clone().with_default(ParameterDefault::Source {
                    ty: parameter.annotated_type(),
                    source,
                }),
                None => parameter.clone().with_default_type(Type::unknown()),
            };
        }
    }
}

fn callable_signature<'db>(
    db: &'db Database,
    environment: &ProgramEnvironment<'db>,
    parent: Type<'db>,
) -> Option<Signature<'db>> {
    let mut signatures = Vec::new();
    parent.map_callable_signatures(db, environment, CallableTypeKind::Regular, |signature| {
        signatures.push(signature.clone());
        signature
    });
    let [signature] = signatures.try_into().ok()?;
    Some(signature)
}

fn boolean_argument(call: &CheckedCall<'_, '_>, name: &str) -> Option<bool> {
    match call.argument(name) {
        CheckedArgument::Omitted => Some(false),
        CheckedArgument::Value { ty, expression: _ } => ty.as_bool_literal(),
        CheckedArgument::Indeterminate => None,
    }
}

fn any_enabled(flags: [Option<bool>; 2]) -> Option<bool> {
    if flags.contains(&Some(true)) {
        Some(true)
    } else if flags.contains(&None) {
        None
    } else {
        Some(false)
    }
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
    let Some(signature) = callable_signature(db, &environment, parent) else {
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
            .and_then(|data| data.attributes.iter().find(|entry| entry.name == *name))
            .and_then(|entry| entry.descriptor.clone())
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
            source: parent_data
                .and_then(|data| data.attributes.iter().find(|entry| entry.name == *name))
                .and_then(|entry| entry.source),
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
        values: None,
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
    let class = specialized_native_class(db, declarations, name, value)?;
    class.to_instance_approximation(db, environment)
}

pub(super) fn specialized_native_class<'db>(
    db: &'db Database,
    declarations: ProgramFile<'db>,
    name: &str,
    value: Type<'db>,
) -> Option<Type<'db>> {
    // Callers select generated declarations with exactly one type parameter.
    let class = native_class(db, declarations, name)?;
    let class = class.as_class_literal()?;
    let specialized = class.apply_specialization(db, |context| context.specialize(db, vec![value]));
    Some(Type::from(specialized))
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
    if call.has_binding_errors() {
        return None;
    }
    let environment = ProgramEnvironment::from_file(call.file());
    let mut fields = Vec::new();
    for keyword in &call.call().arguments.keywords {
        match &keyword.arg {
            Some(name) => fields.push(ProvidedField {
                name: name.id.clone(),
                ty: call.expression_type(&keyword.value)?,
                source: Some(FileRange::new(call.file().file(db), name.range())),
            }),
            None => {
                let Some(DictionaryItems {
                    items,
                    extra_items: _,
                }) = call.dictionary_items(db, &keyword.value)
                else {
                    continue;
                };
                for item in items {
                    if !item.is_required() {
                        continue;
                    }
                    let DictionaryItem {
                        name,
                        ty,
                        kind: _,
                        source,
                    } = item;
                    fields.push(ProvidedField {
                        name,
                        ty,
                        source: Some(FileRange::new(call.file().file(db), source)),
                    });
                }
            }
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
                // Unlisted names use the native getter's possibly-missing field contract.
                has_dynamic_fields: false,
                implications: Box::default(),
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
                let names: Option<Vec<_>> = match expression? {
                    Expr::List(list) => Some(list.elts.iter().collect()),
                    Expr::Tuple(tuple) => Some(tuple.elts.iter().collect()),
                    _ => None,
                };
                if let Some(names) = names {
                    for expression in names {
                        let ty = call.expression_type(expression)?;
                        let name = Name::new(ty.string_literal_value(db)?);
                        if !fields.iter().any(|field| field.name == name) {
                            fields.push(ProvidedField {
                                name,
                                ty: Type::unknown(),
                                source: Some(FileRange::new(
                                    call.file().file(db),
                                    expression.range(),
                                )),
                            });
                        }
                    }
                } else {
                    let mapping = call.dictionary_argument(db, "fields")?;
                    let is_complete = mapping.is_complete();
                    let DictionaryItems {
                        items,
                        extra_items: _,
                    } = mapping;
                    open = !is_complete || items.iter().any(|item| !item.is_required());
                    for DictionaryItem {
                        name,
                        ty,
                        source,
                        kind: _,
                    } in IntoIterator::into_iter(items).filter(DictionaryItem::is_required)
                    {
                        if let Some(doc) = ty.string_literal_value(db) {
                            documentation.parameters.push((name.clone(), doc.into()));
                        }
                        fields.push(ProvidedField {
                            name,
                            ty: Type::unknown(),
                            source: Some(FileRange::new(call.file().file(db), source)),
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
                implications: Box::default(),
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
    fn loaded_build_settings_check_defaults_after_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
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
        let settings = fixture.add_file(&mut analysis.db, "//:settings.bzl", "");
        fixture.add_file(
            &mut analysis.db,
            "//:defs.bzl",
            r#"
load("//:settings.bzl", "setting")
def implementation(ctx): pass
typed_rule = rule(implementation, build_setting=setting, attrs={"extra": attr.int()})
"#,
        );
        let caller = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD.bazel",
            "",
            starpls_common::Dialect::Bazel,
            Some(starpls_common::FileInfo::Bazel {
                api_context: starpls_bazel::APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        for (factory, options, valid, invalid) in [
            ("bool", "flag=True", "True", "\"yes\""),
            ("int", "", "42", "\"42\""),
            ("string", "allow_multiple=True", "\"value\"", "[\"value\"]"),
            (
                "string_list",
                "flag=True, repeatable=True",
                "[\"value\"]",
                "[42]",
            ),
        ] {
            analysis.update_file(
                settings,
                format!("make = config.{factory}\nsetting = make({options})\n"),
            );
            for (arguments, expected) in [
                (
                    format!("build_setting_default={valid}, help=\"description\", extra=1"),
                    None,
                ),
                (
                    format!("build_setting_default={invalid}"),
                    Some("invalid-argument-type"),
                ),
                (String::new(), Some("missing-argument")),
                (
                    "build_setting_default=None".to_owned(),
                    Some("invalid-argument-type"),
                ),
                (
                    format!("build_setting_default=select({{\"//conditions:default\": {valid}}})"),
                    Some("invalid-argument-type"),
                ),
                (
                    format!("build_setting_default={valid}, help=1"),
                    Some("invalid-argument-type"),
                ),
                (
                    format!("build_setting_default={valid}, help=select({{\"//conditions:default\": \"text\"}})"),
                    Some("invalid-argument-type"),
                ),
                (
                    format!("build_setting_default={valid}, extra=\"bad\""),
                    Some("invalid-argument-type"),
                ),
            ] {
                analysis.update_file(caller, format!("load(\"//:defs.bzl\", \"typed_rule\")\ntyped_rule(name=\"value\", {arguments})\n"));
                let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
                let ids: Vec<_> = diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.id().as_str())
                    .collect();
                assert_eq!(
                    ids,
                    expected.into_iter().collect::<Vec<_>>(),
                    "{factory}({options}), {arguments}: {diagnostics:?}"
                );
            }
        }
    }

    #[test]
    fn uncertain_build_settings_preserve_known_attributes() {
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
def implementation(ctx): pass
def make(setting):
    target = rule(implementation, build_setting=setting, attrs={"extra": attr.int()})
    target(name="omitted")
    target(name="supplied", build_setting_default=True, help="description")
    target(name="bad", extra="bad")
    target(name="unknown", unrelated=1)
ordinary = rule(implementation, build_setting=None)
ordinary(name="bad", build_setting_default=True)
ordinary(name="bad_help", help="description")
def with_own_attributes(setting):
    target = rule(implementation, build_setting=setting, attrs={"help": attr.int(), "build_setting_default": attr.string()})
    target(name="valid", help=1, build_setting_default="value")
    target(name="bad", help="text")
def bool():
    # type: () -> None
    return None
config = struct(bool=bool)
shadowed = rule(implementation, build_setting=config.bool())
shadowed(name="bad", build_setting_default=True)
"#,
        );
        let snapshot = analysis.snapshot();
        let mut diagnostics = snapshot.diagnostics(file).unwrap();
        diagnostics
            .sort_by_key(|diagnostic| diagnostic.primary_span().unwrap().range().unwrap().start());
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.id().as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "invalid-argument-type",
                "unknown-argument",
                "unknown-argument",
                "unknown-argument",
                "invalid-argument-type",
                "unknown-argument"
            ],
            "{diagnostics:?}"
        );
    }

    #[test]
    fn build_settings_supply_inherited_signatures() {
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
def implementation(ctx): pass
def macro_impl(name, visibility, **kwargs): pass
setting = rule(implementation, build_setting=config.string_list())
inherited = macro(implementation=macro_impl, inherit_attrs=setting)
inherited(name="valid", build_setting_default=("a", "b"))
inherited(name="missing")
inherited(name="selected", build_setting_default=select({"//conditions:default": ["a"]}))
inherited(name="wrong_help", build_setting_default=[], help=1)
inherited(name="signature", build_setting_default=$0[])
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
        let parameters = signature.parameters.as_ref().unwrap();
        let default = parameters
            .iter()
            .find(|parameter| parameter.label.starts_with("build_setting_default:"))
            .unwrap();
        assert_eq!(default.label, "build_setting_default: Iterable[str]");
        assert_eq!(
            default.documentation.as_deref(),
            Some("Default value of this build setting.")
        );
        let diagnostics = snapshot.diagnostics(file).unwrap();
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.id().as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "missing-argument",
                "invalid-argument-type",
                "invalid-argument-type"
            ],
            "{diagnostics:?}"
        );
    }

    #[test]
    fn struct_signature_preserves_fields_and_host_operations() {
        let (mut analysis, _) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let builtins = starpls_bazel::decode_builtins(include_bytes!(
            "../../../starpls/src/builtin/builtin.pb"
        ))
        .unwrap();
        analysis
            .set_builtin_defs(builtins, Default::default())
            .unwrap();
        let file = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            r#"
def increment(value: int) -> int:
    return value + 1
empty = struct()
record = struct(callback=increment)
mixed = struct(text="x", number=1)
params = {"value": "x"}
spread = struct(**params)
callback = record.callback
result = record.callback(1)
text = mixed.text
number = mixed.number
spread_value = spread.value
opaque: dict[str, int] = {}
uncertain = struct(**opaque)
uncertain_repr = uncertain.__repr__
uncertain_str = uncertain.__str__
uncertain_class = uncertain.__class__
known_repr = struct(__repr__=1).__repr__
rendered = repr(empty)
stringified = str(empty)
equal = empty == struct()
label = Label("//:target")
items = depset([1])
integer = int("1")
boolean = bool(empty)
sequence = list((1,))
wrong_argument = record.callback("wrong")
"#,
        );
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let program = db.starlark_program_file(file);
        let parsed = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
        let model = SemanticModel::new(db, program);
        let environment = model.program_environment();
        let mut values = std::collections::BTreeMap::new();
        for statement in parsed.suite() {
            let Stmt::Assign(assignment) = statement else {
                continue;
            };
            let [target] = assignment.targets.as_slice() else {
                panic!("one target");
            };
            let Expr::Name(name) = target else {
                panic!("named target");
            };
            let ty = assignment.value.inferred_type(&model).unwrap();
            values.insert(name.id.as_str(), ty.display(db, &environment).to_string());
            if let Expr::Call(call) = assignment.value.as_ref() {
                if let Expr::Name(name) = call.func.as_ref() {
                    if name.id == "struct" {
                        let ty = call.func.inferred_type(&model).unwrap();
                        let normalized = ty
                            .map_callable_signatures(
                                db,
                                &environment,
                                ty_python_semantic::types::CallableTypeKind::FunctionLike,
                                std::convert::identity,
                            )
                            .unwrap();
                        assert!(normalized.is_fully_static(db, &environment));
                    }
                }
            }
        }
        for (name, expected) in [
            ("empty", "struct"),
            ("callback", "def increment(value: int) -> int"),
            ("result", "int"),
            ("text", "Literal[\"x\"]"),
            ("number", "Literal[1]"),
            ("spread_value", "Literal[\"x\"]"),
            ("uncertain_repr", "Unknown"),
            ("uncertain_str", "Unknown"),
            ("uncertain_class", "Unknown"),
            ("known_repr", "Literal[1]"),
            ("rendered", "str"),
            ("stringified", "str"),
            ("equal", "bool"),
            ("label", "Label"),
            ("items", "depset[int]"),
            ("integer", "int"),
            ("boolean", "bool"),
            ("sequence", "list[int]"),
        ] {
            assert_eq!(values[name], expected, "{name}");
        }
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.id().as_str())
                .collect::<Vec<_>>(),
            ["invalid-argument-type"],
            "{diagnostics:?}"
        );
    }

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
    fn provider_mapping_fields_keep_documentation_and_origins() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        let file = fixture.main_file();
        for field in ["value", "renamed", "value"] {
            for inline in [true, false] {
                let (prefix, fields) = if inline {
                    (
                        String::new(),
                        format!("{{'{field}': 'Field documentation'}}"),
                    )
                } else {
                    (
                        format!("FIELDS = dict({field}='Field documentation')\n"),
                        "FIELDS".to_owned(),
                    )
                };
                let source = format!("{prefix}Info = provider(fields={fields})\nInfo({field}=1)\nInfo(unexpected=1)\n");
                analysis.update_file(file, source.clone());
                let snapshot = analysis.snapshot();
                let diagnostics = snapshot.diagnostics(file).unwrap();
                let ids: Vec<_> = diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.id().as_str())
                    .collect();
                assert_eq!(ids, ["unknown-argument"], "{source}: {diagnostics:?}");
                let position = FilePosition {
                    file_id: file,
                    pos: (source.find(&format!("Info({field}")).unwrap() as u32 + 5).into(),
                };
                let help = snapshot.signature_help(position.clone()).unwrap().unwrap();
                let [signature] = help.signatures.as_slice() else {
                    panic!("{help:?}");
                };
                assert!(!signature.label.contains("**kwargs"), "{help:?}");
                let parameter = &signature.parameters.as_ref().unwrap()[0];
                assert!(
                    parameter.label.starts_with(&format!("{field}:")),
                    "{help:?}"
                );
                assert_eq!(
                    parameter.documentation.as_deref(),
                    Some("Field documentation")
                );
                let locations = snapshot.goto_definition(position, false).unwrap().unwrap();
                let [crate::LocationLink::Local {
                    target_file_id,
                    target_selection_range,
                    origin_selection_range: _,
                    target_range: _,
                }] = locations.as_slice()
                else {
                    panic!("{locations:?}");
                };
                assert_eq!(*target_file_id, file.source);
                let key = if inline {
                    format!("'{field}'")
                } else {
                    field.to_owned()
                };
                assert_eq!(&source[*target_selection_range], key);
            }
        }
    }

    #[test]
    fn conditional_provider_fields_remain_open() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(
            r#"def make_provider(flag: bool):
    fields = {'always': 'Always present'}
    if flag:
        fields['conditional'] = 'Sometimes present'
    Info = provider(fields=fields)
    Info($0always=1, conditional=2)
    return Info
Example = make_provider(True)
"#,
        );
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let snapshot = analysis.snapshot();
        let help = snapshot
            .signature_help(FilePosition { file_id, pos })
            .unwrap()
            .unwrap();
        let [signature] = help.signatures.as_slice() else {
            panic!("{help:?}");
        };
        assert!(signature.label.contains("always:"), "{help:?}");
        assert!(signature.label.contains("**kwargs"), "{help:?}");
        assert!(!signature.label.contains("conditional:"), "{help:?}");
        let diagnostics = snapshot.diagnostics(file_id).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn conditional_attributes_do_not_promise_parameters() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(
            r#"def implementation(ctx): return []
def make_rule(flag: bool):
    attributes = {}
    if flag:
        attributes["dep"] = attr.label(mandatory=True, allow_single_file=True)
    generated = rule(implementation=implementation, attrs=attributes)
    def register(name):
        if flag:
            generated(name=name, dep="//:input")
        else:
            generated($0name=name)
    return generated, register
example, register = make_rule(False)
"#,
        );
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let snapshot = analysis.snapshot();
        let help = snapshot
            .signature_help(FilePosition { file_id, pos })
            .unwrap()
            .unwrap();
        let [signature] = help.signatures.as_slice() else {
            panic!("{help:?}");
        };
        assert!(signature.label.contains("name: str"), "{help:?}");
        assert!(signature.label.contains("**kwargs"), "{help:?}");
        assert!(!signature.label.contains("dep:"), "{help:?}");
        let diagnostics = snapshot.diagnostics(file_id).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn mutated_rule_attributes_keep_types_and_requiredness() {
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
        let source = r#"
def implementation(ctx): pass
attrs = {"before": attr.string()}
attrs["after"] = attr.int(mandatory=True)
attrs.update({"before": attr.int()})
target = rule(implementation, attrs=attrs)
target($0name="valid", before=1, after=2)
"#;
        fixture.add_file(&mut analysis.db, "main.bzl", source);
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let snapshot = analysis.snapshot();
        let help = snapshot
            .signature_help(FilePosition { file_id, pos })
            .unwrap()
            .unwrap();
        let [signature] = help.signatures.as_slice() else {
            panic!("{help:?}");
        };
        let parameters = signature.parameters.as_ref().unwrap();
        assert!(
            parameters
                .iter()
                .any(|parameter| parameter.label == "before: int | select[int | None] | None = ..."),
            "{help:?}"
        );
        assert!(
            parameters
                .iter()
                .any(|parameter| parameter.label == "after: int | select[int | None]"),
            "{help:?}"
        );
        assert!(!signature.label.contains("**kwargs"), "{help:?}");
        let diagnostics = ty_python_semantic::check_file_unwrap(
            &snapshot.db,
            snapshot.db.starlark_program_file(file_id),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        drop(snapshot);
        let source = source.replace("$0", "");
        for (call, expected) in [
            ("target(name='missing')", "missing-argument"),
            ("target(name='wrong', after='bad')", "invalid-argument-type"),
            (
                "target(name='extra', after=1, extra=True)",
                "unknown-argument",
            ),
            ("target(after=1)", "missing-argument"),
        ] {
            analysis.update_file(file_id, format!("{source}\n{call}\n"));
            let diagnostics = analysis.snapshot().diagnostics(file_id).unwrap();
            assert_eq!(diagnostics.len(), 1, "{call}: {diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), expected, "{call}");
        }
    }
}
