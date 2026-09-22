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

enum CallableKind {
    Function,
    Method,
}

#[derive(Clone, Copy)]
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

    let declared_classes: BTreeSet<_> = classes.keys().cloned().collect();
    let mut body = String::new();
    for class in classes.values() {
        if class.name == "struct" {
            writeln!(
                body,
                "    class struct(_starpls_typing.Generic[_StructField]):"
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
        for field in &class.field {
            if !names.insert(&field.name) {
                continue;
            }
            match &field.callable {
                Some(callable) => write_function(
                    &mut body,
                    "        ",
                    field,
                    callable,
                    CallableKind::Method,
                    if class.name == "native" && rule_names.contains(field.name.as_str()) {
                        AnnotationUse::AttributeInput
                    } else {
                        AnnotationUse::Value
                    },
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
                    } else {
                        None
                    };
                    let field_type = field_type.map(str::to_owned).unwrap_or_else(|| {
                        annotation(
                            &field.r#type,
                            false,
                            &declared_classes,
                            AnnotationUse::Value,
                        )
                    });
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
    let mut exports = String::new();
    for (context, globals) in contexts {
        writeln!(exports, "class _starpls_globals_{context:?}:")?;
        if globals.is_empty() {
            writeln!(exports, "    pass")?;
        }
        for value in globals.values() {
            match &value.callable {
                Some(callable) => {
                    writeln!(exports, "    @staticmethod")?;
                    write_function(
                        &mut exports,
                        "    ",
                        value,
                        callable,
                        CallableKind::Function,
                        if matches!(
                            context,
                            APIContext::Bzl | APIContext::Build | APIContext::Prelude
                        ) && rule_names.contains(value.name.as_str())
                        {
                            AnnotationUse::AttributeInput
                        } else {
                            AnnotationUse::Value
                        },
                        &declared_classes,
                    )?;
                }
                None => writeln!(
                    exports,
                    "    {}: {}",
                    value.name,
                    annotation(
                        &value.r#type,
                        false,
                        &declared_classes,
                        AnnotationUse::Value
                    )
                )?,
            }
        }
        for name in globals.keys() {
            writeln!(
                exports,
                "{} = _starpls_globals_{context:?}.{name}",
                export_name(context, name)
            )?;
        }
    }
    if body.is_empty() {
        body.push_str("    pass\n");
    }
    let mut output = String::from(
        "import builtins as _starpls_builtins\nimport typing as _starpls_typing\n\n_StructField = _starpls_typing.TypeVar(\"_StructField\", covariant=True)\n\nclass _starpls_types:\n",
    );
    output.push_str(&body);
    output.push('\n');
    for name in classes.keys() {
        writeln!(output, "_starpls_annotation_{name} = _starpls_types.{name}")?;
    }
    output.push_str(&exports);
    output.push('\n');
    output.push_str(include_str!("starlark.pyi"));
    Ok(output)
}

fn write_function(
    output: &mut String,
    indent: &str,
    value: &Value,
    callable: &Callable,
    kind: CallableKind,
    input: AnnotationUse,
    classes: &BTreeSet<String>,
) -> anyhow::Result<()> {
    write!(output, "{indent}def {}(", value.name)?;
    let mut separator = "";
    if !matches!(kind, CallableKind::Function) {
        output.push_str("_starpls_self");
        separator = ", ";
    }
    let mut optional = false;
    let mut keyword_only = false;
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
        if *is_mandatory && optional && !keyword_only && !is_star_arg && !is_star_star_arg {
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
        write!(
            output,
            "{name}: {}",
            annotation(r#type, *is_star_arg || *is_star_star_arg, classes, input)
        )?;
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
    let return_type = &callable.return_type;
    writeln!(
        output,
        ") -> {}:",
        annotation(return_type, false, classes, AnnotationUse::Value)
    )?;
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
    writeln!(output, "{indent}    {}", quoted(&documentation))?;
    writeln!(output, "{indent}    ...")?;
    Ok(())
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
                    let key = annotation(key, false, classes, AnnotationUse::Value);
                    let value = annotation(value, false, classes, AnnotationUse::Value);
                    return format!("_starpls_builtins.dict[{key}, {value}]");
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
                let element = annotation(
                    element.unwrap_or("Unknown"),
                    false,
                    classes,
                    AnnotationUse::Value,
                );
                if variadic {
                    return element;
                }
                return if container == "_starpls_builtins.dict" {
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
boolean_rule(0)
boolean_rule(1)
boolean_rule(True)
boolean_rule(False)
native.boolean_rule(0)
native.boolean_rule(1)
native.boolean_rule(True)
native.boolean_rule(False)
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
            ("boolean_rule(2)", "invalid-argument-type"),
            ("native.boolean_rule(2)", "invalid-argument-type"),
            ("strict_bool(0)", "invalid-argument-type"),
            ("strict_bool(1)", "invalid-argument-type"),
            (
                "def generic(flag):\n    # type: (int) -> None\n    boolean_rule(flag)",
                "invalid-argument-type",
            ),
            (
                "def generic(flag):\n    # type: (int) -> None\n    native.boolean_rule(flag)",
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
