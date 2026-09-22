//! Bazel's fixed metadata is a declaration input, separate from user source.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write;

use ruff_db::system::SystemVirtualPathBuf;
use starpls_bazel::builtin::Callable;
use starpls_bazel::builtin::Param;
use starpls_bazel::builtin::Type;
use starpls_bazel::builtin::Value;
use starpls_bazel::env;
use starpls_bazel::APIContext;
use starpls_bazel::Builtins;
use starpls_bazel::BUILTINS_VALUES_DENY_LIST;
use starpls_common::Dialect;
use ty_ide::Docstring;
use ty_ide::MarkupKind;

pub(super) struct DeclarationSource {
    pub(super) path: SystemVirtualPathBuf,
    pub(super) contents: String,
}

#[derive(Clone, Copy)]
enum CallableKind<'a> {
    Function,
    Method(&'a str),
    Rule,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnnotationUse {
    Value,
    AttributeInput,
}

pub(super) fn path(dialect: Dialect) -> SystemVirtualPathBuf {
    let dialect = match dialect {
        Dialect::Standard => "standard",
        Dialect::Bazel => "bazel",
    };
    format!("starpls-native:{dialect}.pyi").into()
}

pub(super) fn export_name(context: APIContext, name: &str) -> String {
    format!("_starpls_{context:?}_{name}")
}

pub(super) fn generate(
    dialect: Dialect,
    builtins: &Builtins,
    rules: &Builtins,
) -> anyhow::Result<DeclarationSource> {
    let contents = declarations(dialect, builtins, rules)?;
    ruff_python_parser::parse_module(&contents).map_err(|error| {
        let offset = usize::from(error.location.start());
        let start = contents[..offset].rfind('\n').map_or(0, |index| index + 1);
        let end = contents[offset..]
            .find('\n')
            .map_or(contents.len(), |index| offset + index);
        let line = &contents[start..end];
        anyhow::Error::new(error)
            .context(format!("Invalid generated {dialect:?} declaration: {line}"))
    })?;
    Ok(DeclarationSource {
        path: path(dialect),
        contents,
    })
}

pub(super) fn globals(
    dialect: Dialect,
    context: APIContext,
    builtins: &Builtins,
    rules: &Builtins,
) -> BTreeMap<String, Value> {
    let mut globals = BTreeMap::new();
    let mut add_globals = |values: &[Value]| {
        for value in values {
            if value.name.is_empty() {
                continue;
            }
            if BUILTINS_VALUES_DENY_LIST.contains(&value.name.as_str()) {
                continue;
            }
            let mut value = value.clone();
            refine_builtin_signature(&mut value);
            globals.insert(value.name.clone(), value);
        }
    };
    match dialect {
        Dialect::Standard => add_globals(&builtins.global),
        Dialect::Bazel => {
            let extra = match context {
                APIContext::Bzl => env::make_bzl_builtins(),
                APIContext::Build => env::make_bzl_builtins(),
                APIContext::Prelude => env::make_bzl_builtins(),
                APIContext::Module => env::make_module_bazel_builtins(),
                APIContext::Repo => env::make_repo_builtins(),
                APIContext::Workspace => env::make_workspace_builtins(),
                APIContext::Cquery => env::make_cquery_builtins(),
                APIContext::Vendor => env::make_vendor_builtins(),
            };
            add_globals(&extra.global);
            if matches!(
                context,
                APIContext::Bzl | APIContext::Build | APIContext::Prelude
            ) {
                add_globals(&env::make_build_builtins().global);
                add_globals(&builtins.global);
                add_globals(&rules.global);
            }
        }
    }
    globals
}

fn refine_builtin_signature(value: &mut Value) {
    let Some(callable) = &mut value.callable else {
        return;
    };
    match value.name.as_str() {
        // The inventory omits Label's input and result types.
        "Label" => {
            callable.return_type = "Label".to_owned();
            if let [input] = callable.param.as_mut_slice() {
                input.r#type = "string; or Label".to_owned();
            }
        }
        // Rule values are callable objects with a nominal inheritance contract.
        "rule" => callable.return_type = "rule".to_owned(),
        // Bazel documents a mutable list, which the inventory calls a sequence.
        "glob" => callable.return_type = "list of strings".to_owned(),
        _ => {}
    }
}

fn declarations(dialect: Dialect, builtins: &Builtins, rules: &Builtins) -> anyhow::Result<String> {
    let rule_names: BTreeSet<_> = rules
        .global
        .iter()
        .filter(|_| dialect == Dialect::Bazel)
        .map(|rule| rule.name.as_str())
        .collect();
    let contexts: Vec<_> = [
        APIContext::Bzl,
        APIContext::Build,
        APIContext::Module,
        APIContext::Repo,
        APIContext::Workspace,
        APIContext::Prelude,
        APIContext::Cquery,
        APIContext::Vendor,
    ]
    .into_iter()
    .map(|context| (context, globals(dialect, context, builtins, rules)))
    .collect();
    let mut classes: BTreeMap<String, Type> = builtins
        .r#type
        .iter()
        .filter(|class| {
            // These types use Ty's canonical core declarations and generics.
            // Attribute, struct, and Target are native nominal declarations.
            !matches!(
                class.name.as_str(),
                "bool"
                    | "bytes"
                    | "builtin_function_or_method"
                    | "dict"
                    | "float"
                    | "function"
                    | "int"
                    | "list"
                    | "range"
                    | "string"
                    | "tuple"
                    | "None"
                    | "NoneType"
            )
        })
        .map(|class| (class.name.clone(), class.clone()))
        .collect();
    for (name, fields) in env::make_missing_module_members() {
        if let Some(class) = classes.get_mut(&name) {
            for field in fields {
                if !class
                    .field
                    .iter()
                    .any(|existing| existing.name == field.name)
                {
                    class.field.push(field);
                }
            }
        }
    }
    if let Some(native) = classes.get_mut("native") {
        for field in &mut native.field {
            refine_builtin_signature(field);
        }
        let workspace = env::make_workspace_builtins();
        for field in rules.global.iter().chain(workspace.global.iter()) {
            if field.name != "workspace"
                && !native
                    .field
                    .iter()
                    .any(|existing| existing.name == field.name)
            {
                native.field.push(field.clone());
            }
        }
    }

    if !rule_names.is_empty() {
        classes.entry("rule".to_owned()).or_insert_with(|| Type {
            name: "rule".to_owned(),
            ..Default::default()
        });
    }
    let declared_classes: BTreeSet<_> = classes.keys().cloned().collect();
    let mut body = String::new();
    for class in classes.values() {
        let parameter = match class.name.as_str() {
            "struct" => Some("_StructField"),
            "Provider" => Some("_ProviderValue"),
            "depset" => Some("_DepsetElement"),
            "DefaultInfo" => Some("_DefaultInfoFiles"),
            _ => None,
        };
        if let Some(parameter) = parameter {
            writeln!(
                body,
                "    class {}(_starpls_typing.Generic[{parameter}]):",
                class.name
            )?;
        } else {
            writeln!(body, "    class {}:", class.name)?;
        }
        writeln!(
            body,
            "        {}",
            quoted(&env::normalize_doc(&class.doc, false))
        )?;
        if class.name == "struct" {
            // The inventory describes an open record. Known source factory
            // fields are supplied separately; arbitrary native fields retain
            // their declared element contract through ordinary member lookup.
            writeln!(body, "        @_starpls_typing.type_check_only")?;
            writeln!(
                body,
                "        def __getattr__(self, name: _starpls_builtins.str) -> _StructField: ..."
            )?;
        }
        let mut names = BTreeSet::new();
        if class.name == "Target" {
            names.extend(["label", "__getitem__", "__contains__"]);
            writeln!(body, "        label: _starpls_types.Label")?;
            // Target access normalizes DefaultInfo.files even when the raw
            // provider constructor omitted it.
            writeln!(body, "        @_starpls_typing.overload")?;
            writeln!(
                body,
                "        def __getitem__(self, key: _starpls_typing.Callable[..., _starpls_types.DefaultInfo]) -> _starpls_types.DefaultInfo[_starpls_types.depset[_starpls_types.File]]: ..."
            )?;
            writeln!(body, "        @_starpls_typing.overload")?;
            writeln!(
                body,
                "        def __getitem__(self, key: _starpls_types.Provider[_ProviderValue] | _starpls_typing.Callable[..., _ProviderValue]) -> _ProviderValue: ..."
            )?;
            writeln!(
                body,
                "        def __contains__(self, key: _starpls_types.Provider[_starpls_typing.Any] | _starpls_typing.Callable[..., _starpls_typing.Any]) -> _starpls_builtins.bool: ..."
            )?;
        }
        if matches!(class.name.as_str(), "rule" | "macro") {
            names.insert("__call__");
            writeln!(
                body,
                "        def __call__(self, **kwargs: _starpls_typing.Any) -> None: ..."
            )?;
        }
        for field in &class.field {
            if !names.insert(field.name.as_str()) {
                continue;
            }
            if class.name == "native" && rule_names.contains(field.name.as_str()) {
                writeln!(body, "        {}: {}", field.name, rule_type(&field.name))?;
                writeln!(
                    body,
                    "        {}",
                    quoted(&env::normalize_doc(&field.doc, false))
                )?;
                continue;
            }
            match &field.callable {
                Some(callable) => write_function(
                    &mut body,
                    "        ",
                    field,
                    callable,
                    CallableKind::Method(&class.name),
                    AnnotationUse::Value,
                    &declared_classes,
                )?,
                None => {
                    let field_type = if class.name == "ctx" {
                        match field.name.as_str() {
                            "file" => Some("_starpls_types.struct[_starpls_types.File]"),
                            "outputs" => Some("_starpls_types.struct[_starpls_types.File]"),
                            "executable" => Some("_starpls_types.struct[_starpls_types.File]"),
                            "files" => Some("_starpls_types.struct[_starpls_builtins.list[_starpls_types.File]]"),
                            _ => None,
                        }
                    } else if class.name == "DefaultInfo" && field.name == "files" {
                        Some("_DefaultInfoFiles")
                    } else {
                        None
                    };
                    let field_type = field_type
                        .map(str::to_owned)
                        .unwrap_or_else(|| value_annotation(field, &declared_classes));
                    writeln!(body, "        {}: {field_type}", field.name)?;
                    if !field.doc.is_empty() {
                        writeln!(
                            body,
                            "        {}",
                            quoted(&env::normalize_doc(&field.doc, false))
                        )?;
                    }
                }
            }
        }
    }
    let mut rule_declarations = String::new();
    for value in rules.global.iter().filter(|_| dialect == Dialect::Bazel) {
        let Some(callable) = &value.callable else {
            continue;
        };
        writeln!(
            rule_declarations,
            "class {}(_starpls_types.rule):",
            value.name
        )?;
        let documentation = callable_documentation(value, callable)?;
        writeln!(rule_declarations, "    {}", quoted(&documentation))?;
        write_function(
            &mut rule_declarations,
            "    ",
            value,
            callable,
            CallableKind::Rule,
            AnnotationUse::AttributeInput,
            &declared_classes,
        )?;
        writeln!(
            rule_declarations,
            "{} = {}",
            rule_type(&value.name),
            value.name
        )?;
    }
    let mut exports = String::new();
    let mut emitted: BTreeMap<String, Vec<(Value, AnnotationUse, String)>> = BTreeMap::new();
    for (context, globals) in contexts {
        for value in globals.values() {
            let export = export_name(context, &value.name);
            let Some(callable) = &value.callable else {
                writeln!(
                    exports,
                    "{export}: {}",
                    value_annotation(value, &declared_classes)
                )?;
                continue;
            };
            let input = if matches!(
                context,
                APIContext::Bzl | APIContext::Build | APIContext::Prelude
            ) && rule_names.contains(value.name.as_str())
            {
                AnnotationUse::AttributeInput
            } else {
                AnnotationUse::Value
            };
            if matches!(input, AnnotationUse::AttributeInput) {
                writeln!(exports, "{export}: {}", rule_type(&value.name))?;
                continue;
            }
            let previous = emitted.entry(value.name.clone()).or_default();
            if let Some((_, _, alias)) = previous
                .iter()
                .find(|(definition, usage, _)| definition == value && *usage == input)
            {
                writeln!(exports, "{export} = {alias}")?;
                continue;
            }
            write_function(
                &mut exports,
                "",
                value,
                callable,
                CallableKind::Function,
                input,
                &declared_classes,
            )?;
            // Capture this definition before another context reuses its public name.
            writeln!(exports, "{export} = {}", value.name)?;
            previous.push((value.clone(), input, export));
        }
    }
    if body.is_empty() {
        body.push_str("    pass\n");
    }
    let mut output = String::from(
        "import builtins as _starpls_builtins\nimport typing as _starpls_typing\n\n_StructField = _starpls_typing.TypeVar(\"_StructField\", covariant=True)\n_ProviderValue = _starpls_typing.TypeVar(\"_ProviderValue\")\n_DepsetElement = _starpls_typing.TypeVar(\"_DepsetElement\", covariant=True)\n_DefaultInfoFiles = _starpls_typing.TypeVar(\"_DefaultInfoFiles\", bound=\"_starpls_types.depset[_starpls_types.File] | None\", default=\"_starpls_types.depset[_starpls_types.File] | None\", covariant=True)\n\nclass _starpls_types:\n",
    );
    output.push_str(&body);
    output.push('\n');
    for name in classes.keys() {
        writeln!(output, "_starpls_annotation_{name} = _starpls_types.{name}")?;
    }
    output.push_str(&rule_declarations);
    output.push_str(&exports);
    output.push('\n');
    output.push_str(include_str!("starlark.pyi"));
    Ok(output)
}

fn rule_type(name: &str) -> String {
    format!("_starpls_rule_{name}")
}

fn value_annotation(value: &Value, classes: &BTreeSet<String>) -> String {
    let annotation = annotation(&value.r#type, false, classes, AnnotationUse::Value);
    if starpls_bazel::KNOWN_PROVIDER_TYPES.contains(&value.name.as_str()) {
        format!("_starpls_types.Provider[{annotation}]")
    } else {
        annotation
    }
}

fn write_function(
    output: &mut String,
    indent: &str,
    value: &Value,
    callable: &Callable,
    kind: CallableKind<'_>,
    input: AnnotationUse,
    classes: &BTreeSet<String>,
) -> anyhow::Result<()> {
    let name = if matches!(kind, CallableKind::Rule) {
        "__call__"
    } else {
        &value.name
    };
    write!(output, "{indent}def {name}(")?;
    let mut separator = "";
    if !matches!(kind, CallableKind::Function) {
        output.push_str("_starpls_self");
        separator = ", ";
    }
    let mut optional = false;
    let mut keyword_only = matches!(kind, CallableKind::Rule) && !callable.param.is_empty();
    if keyword_only {
        output.push_str(", *");
    }
    for parameter in &callable.param {
        output.push_str(separator);
        separator = ", ";
        let Param {
            name,
            r#type,
            doc: _,
            default_value,
            is_mandatory,
            is_star_arg,
            is_star_star_arg,
        } = parameter;
        let name = name.trim_start_matches('*');
        // This inventory omits parameter kinds. A required parameter after an
        // optional one proves a keyword-only boundary, but optional tails do
        // not. Preserve explicit variadics and avoid inventing other boundaries.
        let transitive = matches!(kind, CallableKind::Function)
            && value.name == "depset"
            && name == "transitive";
        if ((*is_mandatory && optional) || transitive)
            && !keyword_only
            && !is_star_arg
            && !is_star_star_arg
        {
            output.push_str("*, ");
            keyword_only = true;
        }
        if *is_star_arg {
            keyword_only = true;
        }
        if *is_star_arg && name.is_empty() {
            output.push('*');
            continue;
        }
        if *is_star_star_arg {
            output.push_str("**");
        } else if *is_star_arg {
            output.push('*');
        }
        let parameter_type = match (kind, value.name.as_str(), name) {
            (CallableKind::Function, "depset", "direct") => {
                Some("_starpls_typing.Sequence[_DepsetElement] | None")
            }
            (CallableKind::Function, "depset", "transitive") => {
                Some("_starpls_typing.Sequence[_starpls_types.depset[_DepsetElement]] | None")
            }
            (CallableKind::Function, "DefaultInfo", "files") => Some("_DefaultInfoFiles"),
            _ => None,
        };
        let parameter_type = parameter_type.map(str::to_owned).unwrap_or_else(|| {
            annotation(r#type, *is_star_arg || *is_star_star_arg, classes, input)
        });
        write!(output, "{name}: {parameter_type}")?;
        if !is_star_arg && !is_star_star_arg && !is_mandatory {
            optional = true;
            let default = if default_value.is_empty() || default_value == "unbound" {
                "..."
            } else {
                default_value
            };
            write!(output, " = {default}")?;
        }
    }
    let return_type = match (kind, value.name.as_str()) {
        (CallableKind::Function, "depset") => Some("_starpls_types.depset[_DepsetElement]"),
        (CallableKind::Function, "DefaultInfo") => {
            Some("_starpls_types.DefaultInfo[_DefaultInfoFiles]")
        }
        (CallableKind::Method("depset"), "to_list") => {
            Some("_starpls_builtins.list[_DepsetElement]")
        }
        _ => None,
    };
    let return_type = return_type
        .map(str::to_owned)
        .unwrap_or_else(|| annotation(&callable.return_type, false, classes, AnnotationUse::Value));
    writeln!(output, ") -> {return_type}:")?;
    let documentation = callable_documentation(value, callable)?;
    writeln!(output, "{indent}    {}", quoted(&documentation))?;
    writeln!(output, "{indent}    ...")?;
    Ok(())
}

fn callable_documentation(value: &Value, callable: &Callable) -> anyhow::Result<String> {
    let mut documentation = env::normalize_doc(&value.doc, false);
    let mut documented_parameters = callable
        .param
        .iter()
        .filter(|parameter| !parameter.doc.is_empty())
        .peekable();
    if documented_parameters.peek().is_some() {
        documentation.push_str("\n\nArgs:");
    }
    for parameter in documented_parameters {
        let description =
            Docstring::new(env::normalize_doc(&parameter.doc, false)).render(MarkupKind::PlainText);
        let mut lines = description.lines();
        write!(
            documentation,
            "\n    {}: {}",
            parameter.name.trim_start_matches('*'),
            lines.next().unwrap_or_default()
        )?;
        for line in lines {
            write!(documentation, "\n        {line}")?;
        }
    }
    Ok(documentation)
}

/// Decode the inventory's prose vocabulary, not arbitrary annotation source.
/// Undeclared umbrella/prose types carry no nominal identity or usable contract.
fn annotation(
    text: &str,
    variadic: bool,
    classes: &BTreeSet<String>,
    usage: AnnotationUse,
) -> String {
    let text = env::normalize_doc(text, true);
    text.split("; or ")
        .filter(|part| part.trim() != "unbound")
        .map(|part| {
            if let Some(mapping) = part.trim().strip_prefix("Dictionary: ") {
                if let Some((key, value)) = mapping.split_once(" -> ") {
                    let label_key = matches!(key.trim(), "Label" | "label")
                        && classes.contains("Label");
                    let key = annotation(key, false, classes, usage);
                    let value = annotation(value, false, classes, usage);
                    let mapping = match usage {
                        AnnotationUse::Value => "_starpls_builtins.dict",
                        AnnotationUse::AttributeInput => "_starpls_typing.Mapping",
                    };
                    // Mapping keys are invariant; Bazel converts either key kind.
                    if matches!(usage, AnnotationUse::AttributeInput) && label_key {
                        return format!(
                            "{mapping}[_starpls_types.Label, {value}] | {mapping}[_starpls_builtins.str, {value}] | {mapping}[{key}, {value}]"
                        );
                    }
                    return format!("{mapping}[{key}, {value}]");
                }
            }
            let (name, element) = part
                .trim()
                .split_once(" of ")
                .map_or((part.trim(), None), |(name, element)| {
                    (name, Some(element.strip_suffix('s').unwrap_or(element)))
                });
            let container = match name {
                "list" => Some("_starpls_builtins.list"),
                "List" => Some("_starpls_builtins.list"),
                "sequence" => Some("_starpls_typing.Sequence"),
                "Sequence" => Some("_starpls_typing.Sequence"),
                "iterable" => Some("_starpls_typing.Iterable"),
                "Iterable" => Some("_starpls_typing.Iterable"),
                "Tuple" => Some("_starpls_builtins.tuple"),
                "tuple" => Some("_starpls_builtins.tuple"),
                "dict" => Some("_starpls_builtins.dict"),
                "Dict" => Some("_starpls_builtins.dict"),
                "Dictionary" => Some("_starpls_builtins.dict"),
                _ => None,
            };
            if let Some(container) = container {
                let container = match (container, usage) {
                    ("_starpls_builtins.list", AnnotationUse::AttributeInput) => {
                        "_starpls_typing.Iterable"
                    }
                    ("_starpls_builtins.dict", AnnotationUse::AttributeInput) => {
                        "_starpls_typing.Mapping"
                    }
                    _ => container,
                };
                let element = annotation(element.unwrap_or("Unknown"), false, classes, usage);
                if variadic {
                    return element;
                }
                return if matches!(
                    container,
                    "_starpls_builtins.dict" | "_starpls_typing.Mapping"
                ) {
                    format!("{container}[_starpls_typing.Any, {element}]")
                } else if container == "_starpls_builtins.tuple" {
                    format!("{container}[{element}, ...]")
                } else {
                    format!("{container}[{element}]")
                };
            }
            let boolean = match usage {
                AnnotationUse::Value => "_starpls_builtins.bool",
                // Rule attributes convert 0 and 1; ordinary native parameters do not.
                AnnotationUse::AttributeInput => {
                    "_starpls_builtins.bool | _starpls_typing.Literal[0, 1]"
                }
            };
            let scalar = match name {
                "" => "_starpls_typing.Any",
                "Unknown" => "_starpls_typing.Any",
                "unknown" => "_starpls_typing.Any",
                "Any" => "_starpls_typing.Any",
                "None" => "None",
                "NoneType" => "None",
                "int" => "_starpls_builtins.int",
                "Integer" => "_starpls_builtins.int",
                "float" => "_starpls_builtins.float",
                "bool" => boolean,
                "boolean" => boolean,
                "Boolean" => boolean,
                "string" => "_starpls_builtins.str",
                "String" => "_starpls_builtins.str",
                "str" => "_starpls_builtins.str",
                "bytes" => "_starpls_builtins.bytes",
                "tuple" => "_starpls_builtins.tuple[_starpls_typing.Any, ...]",
                "range" => "_starpls_builtins.range",
                "function" => "_starpls_typing.Callable[..., _starpls_typing.Any]",
                "callable" => "_starpls_typing.Callable[..., _starpls_typing.Any]",
                _ => {
                    let name = match name {
                        "label" => "Label",
                        "structure" => "struct",
                        _ => name,
                    };
                    if !classes.contains(name) {
                        return "_starpls_typing.Any".to_owned();
                    }
                    if name == "Label" && matches!(usage, AnnotationUse::AttributeInput) {
                        return "_starpls_types.Label | _starpls_builtins.str".to_owned();
                    }
                    return format!("_starpls_types.{name}");
                }
            };
            scalar.to_owned()
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn quoted(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            _ => {
                if character.is_control() {
                    write!(output, "\\u{:04x}", u32::from(character)).unwrap();
                } else {
                    output.push(character);
                }
            }
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ruff_python_ast::Stmt;
    use starpls_hir::Db;
    use ty_python_semantic::HasType;
    use ty_python_semantic::SemanticModel;

    use super::*;
    use crate::Analysis;
    use crate::FilePosition;

    #[test]
    fn bundled_metadata_uses_recursive_native_declarations() {
        let builtins = starpls_bazel::decode_builtins(include_bytes!(
            "../../../starpls/src/builtin/builtin.pb"
        ))
        .unwrap();
        let (mut analysis, _) = Analysis::new_for_test();
        analysis
            .set_builtin_defs(builtins, Builtins::default())
            .unwrap();
        let source = "label = Label(\"//pkg:target\")\nrelative = label.relative(\":other\")\n";
        let file = analysis
            .open_document(
                Path::new("/main.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let file = db.starlark_program_file(file);
        let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
        let model = SemanticModel::new(db, file);
        let types: Vec<_> = parsed
            .suite()
            .iter()
            .map(|statement| {
                let Stmt::Assign(assignment) = statement else {
                    panic!("expected assignment")
                };
                assignment.value.inferred_type(&model).unwrap()
            })
            .collect();
        let [label, relative] = types.as_slice() else {
            panic!("expected two assignments")
        };
        assert_eq!(label, relative);
        assert_eq!(
            label.display(db, &model.program_environment()).to_string(),
            "Label"
        );
        let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn glob_returns_a_mutable_list_of_strings() {
        for (path, glob) in [("/main.bzl", "native.glob"), ("/BUILD.bazel", "glob")] {
            let builtins = starpls_bazel::decode_builtins(include_bytes!(
                "../../../starpls/src/builtin/builtin.pb"
            ))
            .unwrap();
            let (mut analysis, _) = Analysis::new_for_test();
            analysis
                .set_builtin_defs(builtins, Builtins::default())
                .unwrap();
            let source = format!(
                "files = {glob}([\"*.rs\"])\ncombined = files + [\"extra.rs\"]\nfiles.append(\"other.rs\")\nfiles.append(1)\n"
            );
            let invalid = u32::try_from(source.rfind('1').unwrap()).unwrap();
            let file = analysis
                .open_document(Path::new(path), Dialect::Bazel, None, source, 1)
                .unwrap();
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(diagnostics.len(), 1, "{path}: {diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), "invalid-argument-type");
            assert_eq!(
                diagnostics[0].range().unwrap(),
                ruff_text_size::TextRange::new(invalid.into(), (invalid + 1).into()),
            );
        }
    }

    #[test]
    fn label_rule_inputs_accept_converted_iterables() {
        let function = |name: &str, input: &str| Value {
            name: name.to_owned(),
            callable: Some(Callable {
                param: vec![Param {
                    name: "srcs".to_owned(),
                    r#type: input.to_owned(),
                    is_mandatory: true,
                    ..Default::default()
                }],
                return_type: "None".to_owned(),
            }),
            ..Default::default()
        };
        let mut builtins = starpls_bazel::decode_builtins(include_bytes!(
            "../../../starpls/src/builtin/builtin.pb"
        ))
        .unwrap();
        builtins
            .global
            .push(function("strict_labels", "List of Labels"));
        let (mut analysis, _) = Analysis::new_for_test();
        analysis
            .set_builtin_defs(
                builtins,
                Builtins {
                    global: vec![
                        function("label_rule", "List of Labels"),
                        function("mapping_rule", "Dictionary: string -> list of strings"),
                        function("keyed_rule", "Dictionary: Label -> string"),
                    ],
                    ..Default::default()
                },
            )
            .unwrap();
        let source = r#"
def implementation(ctx):
    return []
source_rule = rule(implementation=implementation, attrs={"srcs": attr.label_list(), "mapping": attr.string_list_dict(), "keyed": attr.label_keyed_string_dict()})
paths = native.glob(["*.rs"])
labels = [Label("//:input.rs")]
mapping = {"key": ["value"]}
native.mapping_rule(srcs=mapping)
string_keys = {"//:input": "value"}
label_keys = {Label("//:input"): "value"}
mixed_keys = {"//:input": "one", Label("//:other"): "two"}
native.keyed_rule(srcs=string_keys)
native.keyed_rule(srcs=label_keys)
native.keyed_rule(srcs=mixed_keys)
source_rule(name="string_keys", keyed=string_keys)
source_rule(name="label_keys", keyed=label_keys)
source_rule(name="mixed_keys", keyed=mixed_keys)
source_rule(name="mapping", mapping=mapping)
label_rule(srcs=paths)
native.label_rule(srcs=paths)
source_rule(name="source", srcs=paths)
native.label_rule(srcs=labels)
source_rule(name="labels", srcs=labels)
native.label_rule(srcs=("input.rs", Label("//:other.rs")))
source_rule(name="tuple", srcs=("input.rs", Label("//:other.rs")))
strict_labels(labels)
"#;
        let file = analysis
            .open_document(
                Path::new("/main.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for call in [
            "native.label_rule(srcs=42)",
            "native.label_rule(srcs=\"input.rs\")",
            "native.label_rule(srcs=[42])",
            "source_rule(name=\"bad\", srcs=42)",
            "source_rule(name=\"bad\", srcs=\"input.rs\")",
            "source_rule(name=\"bad\", srcs=[42])",
            "strict_labels(paths)",
            "native.keyed_rule(srcs={42: \"bad\"})",
            "source_rule(name=\"bad\", keyed={42: \"bad\"})",
        ] {
            analysis
                .open_document(
                    Path::new("/main.bzl"),
                    Dialect::Bazel,
                    None,
                    format!("{source}\n{call}\n"),
                    2,
                )
                .unwrap();
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(diagnostics.len(), 1, "{call}: {diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), "invalid-argument-type");
            assert!(usize::from(diagnostics[0].range().unwrap().start()) > source.len());
        }
    }

    #[test]
    fn native_rules_are_nominal_keyword_callables() {
        let builtins = starpls_bazel::decode_builtins(include_bytes!(
            "../../../starpls/src/builtin/builtin.pb"
        ))
        .unwrap();
        let rules = Builtins {
            global: vec![Value {
                name: "sources".to_owned(),
                doc: "A rule with source files.".to_owned(),
                callable: Some(Callable {
                    param: vec![Param {
                        name: "srcs".to_owned(),
                        r#type: "List of Labels".to_owned(),
                        doc: "Source file labels.".to_owned(),
                        is_mandatory: true,
                        ..Default::default()
                    }],
                    return_type: "None".to_owned(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let (mut analysis, _) = Analysis::new_for_test();
        analysis.set_builtin_defs(builtins, rules).unwrap();
        let source = r#"
def implementation(**kwargs):
    pass
sources(srcs=["//:input"])
native.sources(srcs=["//:input"])
alias = native.sources
alias(srcs=["//:input"])
macro(implementation=implementation, inherit_attrs=native.sources)
macro(implementation=implementation, inherit_attrs=sources)
"#;
        let file = analysis
            .open_document(
                Path::new("/main.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let hover = analysis
            .snapshot()
            .hover(FilePosition {
                file_id: file,
                pos: u32::try_from(source.find("native.sources").unwrap() + 8)
                    .unwrap()
                    .into(),
            })
            .unwrap()
            .unwrap();
        assert!(
            hover.contents.value.contains("srcs:"),
            "{}",
            hover.contents.value
        );
        assert!(
            hover.contents.value.contains("A rule with source files."),
            "{}",
            hover.contents.value
        );
        for callee in ["native.sources", "alias"] {
            let help = analysis
                .snapshot()
                .signature_help(FilePosition {
                    file_id: file,
                    pos: u32::try_from(
                        source.find(&format!("{callee}(srcs=")).unwrap() + callee.len() + 1,
                    )
                    .unwrap()
                    .into(),
                })
                .unwrap()
                .unwrap();
            let [signature] = help.signatures.as_slice() else {
                panic!("{help:?}");
            };
            assert!(
                signature.label.starts_with(&format!("def {callee}(")),
                "{signature:?}"
            );
            let [parameter] = signature.parameters.as_deref().unwrap() else {
                panic!("{signature:?}");
            };
            assert_eq!(
                parameter.documentation.as_deref(),
                Some("Source file labels.")
            );
        }
        for (call, expected) in [
            ("native.sources()", "missing-argument"),
            ("sources(srcs=42)", "invalid-argument-type"),
            (
                "native.sources([\"//:input\"])",
                "too-many-positional-arguments",
            ),
            (
                "macro(implementation=implementation, inherit_attrs=native.glob)",
                "invalid-argument-type",
            ),
        ] {
            analysis.update_file(file, format!("{source}\n{call}\n"));
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.id().as_str() == expected
                        && diagnostic
                            .range()
                            .is_some_and(|range| range.start().to_usize() >= source.len())),
                "{call}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn public_names_preserve_context_specific_declarations() {
        let mut builtins = starpls_bazel::decode_builtins(include_bytes!(
            "../../../starpls/src/builtin/builtin.pb"
        ))
        .unwrap();
        builtins.global.push(Value {
            name: "register_toolchains".to_owned(),
            callable: Some(Callable {
                param: vec![Param {
                    name: "number".to_owned(),
                    r#type: "int".to_owned(),
                    is_mandatory: true,
                    ..Default::default()
                }],
                return_type: "None".to_owned(),
            }),
            ..Default::default()
        });
        let (mut analysis, _) = Analysis::new_for_test();
        analysis
            .set_builtin_defs(builtins, Builtins::default())
            .unwrap();
        for context in [
            APIContext::Bzl,
            APIContext::Module,
            APIContext::Workspace,
            APIContext::Prelude,
        ] {
            let file = analysis
                .open_document(
                    Path::new("/main.bzl"),
                    Dialect::Bazel,
                    Some(starpls_common::FileInfo::Bazel {
                        api_context: context,
                        is_external: false,
                    }),
                    "register_toolchains(1)".to_owned(),
                    1,
                )
                .unwrap();
            let snapshot = analysis.snapshot();
            let diagnostics = snapshot.diagnostics(file).unwrap();
            assert_eq!(
                diagnostics.is_empty(),
                matches!(context, APIContext::Bzl | APIContext::Prelude),
                "{context:?}: {diagnostics:?}"
            );
            let help = snapshot
                .signature_help(FilePosition {
                    file_id: file,
                    pos: 20.into(),
                })
                .unwrap()
                .unwrap();
            let signature = &help.signatures[0].label;
            match context {
                APIContext::Bzl | APIContext::Prelude => {
                    assert_eq!(signature, "def register_toolchains(number: int) -> None")
                }
                APIContext::Module => assert!(
                    signature.contains("str") && signature.contains("dev_dependency"),
                    "{signature}"
                ),
                APIContext::Workspace => assert!(
                    signature.contains("Label") && !signature.contains("dev_dependency"),
                    "{signature}"
                ),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn native_context_edits_preserve_other_context_and_file_identity() {
        let (mut analysis, _) = Analysis::new_for_test();
        let file = analysis
            .open_document(
                Path::new("/shared.bzl"),
                Dialect::Bazel,
                None,
                "native_value(1)".to_owned(),
                1,
            )
            .unwrap();
        {
            let snapshot = analysis.snapshot();
            let db = &snapshot.db;
            let diagnostics =
                ty_python_semantic::check_file_unwrap(db, db.starlark_program_file(file));
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), "unresolved-reference");
        }
        let documentation = "A documented native parameter.\n    A continuation.\n\n    ```python\n    if True:\n        value = 1\n    ```";
        let metadata = |ty: &str| Builtins {
            global: vec![Value {
                name: "native_value".to_owned(),
                callable: Some(Callable {
                    param: vec![Param {
                        name: "value".to_owned(),
                        r#type: ty.to_owned(),
                        doc: documentation.to_owned(),
                        is_mandatory: true,
                        ..Default::default()
                    }],
                    return_type: ty.to_owned(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        analysis
            .set_builtin_defs(metadata("string"), Builtins::default())
            .unwrap();
        let mut identity = None;
        for standard_ty in ["int", "float"] {
            analysis
                .db
                .set_builtin_defs(
                    Dialect::Standard,
                    metadata(standard_ty),
                    Builtins::default(),
                )
                .unwrap();
            let native_file = analysis
                .db
                .files
                .try_virtual_file(&path(Dialect::Standard))
                .unwrap()
                .file();
            if let Some(previous) = identity {
                assert_eq!(native_file, previous);
            }
            identity = Some(native_file);
            for (dialect, expected) in [(Dialect::Standard, standard_ty), (Dialect::Bazel, "str")] {
                let mut file_id = file;
                file_id.dialect = dialect;
                let help = analysis
                    .snapshot()
                    .signature_help(FilePosition {
                        file_id,
                        pos: starpls_syntax::TextSize::new(14),
                    })
                    .unwrap()
                    .unwrap();
                let [signature] = help.signatures.as_slice() else {
                    panic!("{help:?}")
                };
                assert_eq!(
                    signature.label,
                    format!("def native_value(value: {expected}) -> {expected}")
                );
                assert_eq!(
                    signature.parameters.as_ref().unwrap()[0]
                        .documentation
                        .as_deref(),
                    Some("A documented native parameter.  \nA continuation.  \n  \n```python\nif True:\n    value = 1\n```")
                );
                let snapshot = analysis.snapshot();
                let db = &snapshot.db;
                let diagnostics =
                    ty_python_semantic::check_file_unwrap(db, db.starlark_program_file(file_id));
                assert_eq!(
                    diagnostics.len(),
                    usize::from(dialect == Dialect::Bazel),
                    "{diagnostics:?}"
                );
            }
        }
        let mut malformed = metadata("int");
        malformed.global[0].name = "not a name".to_owned();
        assert!(analysis
            .set_builtin_defs(malformed, Builtins::default())
            .is_err());
        let help = analysis
            .snapshot()
            .signature_help(FilePosition {
                file_id: file,
                pos: starpls_syntax::TextSize::new(14),
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            help.signatures[0].label,
            "def native_value(value: str) -> str"
        );
    }

    #[test]
    fn boolean_conversion_is_limited_to_rule_inputs() {
        let function = |name: &str, returns: &str| Value {
            name: name.to_owned(),
            callable: Some(Callable {
                param: vec![Param {
                    name: "flag".to_owned(),
                    r#type: "Boolean".to_owned(),
                    is_mandatory: true,
                    ..Default::default()
                }],
                return_type: returns.to_owned(),
            }),
            ..Default::default()
        };
        let mut builtins = starpls_bazel::decode_builtins(include_bytes!(
            "../../../starpls/src/builtin/builtin.pb"
        ))
        .unwrap();
        builtins.global.push(function("strict_bool", "Boolean"));
        builtins.r#type.push(Type {
            name: "BooleanRecord".to_owned(),
            field: vec![Value {
                name: "value".to_owned(),
                r#type: "Boolean".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        });
        builtins.global.push(Value {
            name: "boolean_record".to_owned(),
            r#type: "BooleanRecord".to_owned(),
            ..Default::default()
        });
        let (mut analysis, _) = Analysis::new_for_test();
        analysis
            .set_builtin_defs(
                builtins,
                Builtins {
                    global: vec![function("boolean_rule", "None")],
                    ..Default::default()
                },
            )
            .unwrap();
        let source = "\
boolean_rule(flag=0)
boolean_rule(flag=1)
boolean_rule(flag=True)
boolean_rule(flag=False)
native.boolean_rule(flag=0)
native.boolean_rule(flag=1)
native.boolean_rule(flag=True)
native.boolean_rule(flag=False)
returned = strict_bool(True)
field = boolean_record.value
strict_bool(False)
";
        let file = analysis
            .open_document(
                Path::new("/booleans.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        {
            let snapshot = analysis.snapshot();
            let db = &snapshot.db;
            let file = db.starlark_program_file(file);
            let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
            let model = SemanticModel::new(db, file);
            for statement in parsed.suite() {
                let Stmt::Assign(assignment) = statement else {
                    continue;
                };
                let ty = assignment.value.inferred_type(&model).unwrap();
                assert_eq!(
                    ty.display(db, &model.program_environment()).to_string(),
                    "bool"
                );
            }
        }
        for (statement, expected) in [
            ("boolean_rule(flag=2)", "invalid-argument-type"),
            ("native.boolean_rule(flag=2)", "invalid-argument-type"),
            ("strict_bool(0)", "invalid-argument-type"),
            ("strict_bool(1)", "invalid-argument-type"),
            (
                "def generic(flag):\n    # type: (int) -> None\n    boolean_rule(flag=flag)",
                "invalid-argument-type",
            ),
            (
                "def generic(flag):\n    # type: (int) -> None\n    native.boolean_rule(flag=flag)",
                "invalid-argument-type",
            ),
            ("annotated = 1 # type: bool", "invalid-assignment"),
        ] {
            analysis.update_file(file, format!("{source}\n{statement}\n"));
            let snapshot = analysis.snapshot();
            let db = &snapshot.db;
            let diagnostics =
                ty_python_semantic::check_file_unwrap(db, db.starlark_program_file(file));
            assert_eq!(diagnostics.len(), 1, "{statement}: {diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), expected, "{statement}");
        }
    }

    #[test]
    fn native_collections_accept_source_values_across_programs() {
        let (mut analysis, _) = Analysis::new_for_test();
        analysis
            .set_builtin_defs(
                Builtins {
                    global: vec![Value {
                        name: "consume".to_owned(),
                        callable: Some(Callable {
                            param: vec![Param {
                                name: "values".to_owned(),
                                r#type: "List of ints".to_owned(),
                                is_mandatory: true,
                                ..Default::default()
                            }],
                            return_type: "None".to_owned(),
                        }),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Builtins::default(),
            )
            .unwrap();
        let source = "consume([1])\nvalues = [1]\nconsume(values)\n";
        let file = analysis
            .open_document(
                Path::new("/main.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let diagnostics = ty_python_semantic::check_file_unwrap(db, db.starlark_program_file(file));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn native_and_source_contracts_cross_build_context() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Builtins::default(),
            )
            .unwrap();
        fixture.add_file(
            &mut analysis.db,
            "defs.bzl",
            r#"
def consume(value, values, target):
    # type: (int, list[int], Label) -> None
    pass
"#,
        );
        let file = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD",
            r#"
load("defs.bzl", "consume")
consume(1, [1], Label("//pkg:target"))
values = [1]
consume(1, values, Label("//pkg:target"))
"#,
            Dialect::Bazel,
            Some(starpls_common::FileInfo::Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let diagnostics = ty_python_semantic::check_file_unwrap(db, db.starlark_program_file(file));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn starlark_intrinsics_use_declared_language_contracts() {
        let (mut analysis, _) = Analysis::new_for_test();
        let file = analysis
            .open_document(
                Path::new("/intrinsics.bzl"),
                Dialect::Bazel,
                None,
                r#"
kind = type(1)
entries = enumerate(list=[1], start=1)
backwards = reversed({1: 2})
ordered = sorted([1], None, reverse=True)
pairs = zip([1], ["text"])
printed = print(1)
def stop():
    fail("stop")
"#
                .to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let file = db.starlark_program_file(file);
        let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
        let model = SemanticModel::new(db, file);
        let types: Vec<_> = parsed
            .suite()
            .iter()
            .filter_map(|statement| {
                let Stmt::Assign(assignment) = statement else {
                    return None;
                };
                Some(
                    assignment
                        .value
                        .inferred_type(&model)
                        .unwrap()
                        .display(db, &model.program_environment())
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(
            types,
            [
                "str",
                "list[tuple[int, int]]",
                "list[int]",
                "list[int]",
                "list[tuple[int, str]]",
                "None"
            ]
        );
        let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn starlark_intrinsic_parameter_kinds() {
        for (source, parameter, index) in [
            ("enumerate(list=[1$0])", "list:", 0),
            ("sorted([1], None$0)", "key:", 1),
            ("reversed({1: 2$0})", "sequence:", 0),
        ] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let help = analysis
                .snapshot()
                .signature_help(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            let [signature] = help.signatures.as_slice() else {
                panic!("{source}: {help:?}");
            };
            assert_eq!(
                signature.active_parameter,
                Some(index),
                "{source}: {help:?}"
            );
            assert!(
                signature.parameters.as_ref().unwrap()[index]
                    .label
                    .starts_with(parameter),
                "{source}: {help:?}"
            );
        }
    }
}
