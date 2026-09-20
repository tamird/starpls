use ruff_db::diagnostic::Annotation;
use ruff_db::diagnostic::Diagnostic;
use ruff_db::diagnostic::DiagnosticId;
use ruff_db::diagnostic::Severity;
use ruff_db::diagnostic::Span;
use ruff_python_ast::name::Name;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprCall;
use ruff_python_ast::HasNodeIndex;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use ruff_text_size::TextSize;
use starpls_common::Db;
use starpls_syntax::source::string_value;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::definition::ProvidedBinding;
use ty_python_core::definition::ProvidedStatement;
use ty_python_core::ProgramFile;
use ty_python_semantic::provided::ProvidedBindingResolution;
use ty_python_semantic::provided::ProvidedBindingValue;

use crate::Database;

pub(super) fn statements(db: &Database, file: ProgramFile<'_>) -> Vec<ProvidedStatement> {
    let Some(source_file) = db.starlark_file(file) else {
        return Vec::new();
    };
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let source = source_file.contents(db);
    parsed
        .suite()
        .iter()
        .filter_map(|statement| {
            let Stmt::Expr(statement) = statement else {
                return None;
            };
            let call = load_call(&statement.value)?;
            let bindings = call
                .arguments
                .iter_source_order()
                .skip(1)
                .filter_map(|argument| {
                    let (name, range) = match argument {
                        ArgOrKeyword::Arg(expr) => {
                            let (name, prefix) = string_value(&source[expr.range()])?;
                            let text = &source[expr.range()];
                            let suffix = if text.ends_with("\"\"\"") || text.ends_with("'''") {
                                3
                            } else {
                                1
                            };
                            let range = TextRange::new(
                                expr.start() + TextSize::from(prefix),
                                expr.end() - TextSize::from(suffix),
                            );
                            (Name::new(name), range)
                        }
                        ArgOrKeyword::Keyword(keyword) => {
                            let name = keyword.arg.as_ref()?;
                            (name.id.clone(), name.range())
                        }
                    };
                    let target = match argument {
                        ArgOrKeyword::Arg(expr) => expr.node_index().load(),
                        ArgOrKeyword::Keyword(keyword) => keyword.node_index().load(),
                    };
                    Some(ProvidedBinding {
                        target,
                        name,
                        range,
                    })
                })
                .collect();
            Some(ProvidedStatement {
                statement: statement.node_index().load(),
                bindings,
            })
        })
        .collect()
}

pub(super) fn resolve<'db>(
    db: &'db Database,
    definition: Definition<'db>,
) -> ProvidedBindingResolution<'db> {
    let file = definition.program_file(db);
    let Some(source_file) = db.starlark_file(file) else {
        return ProvidedBindingValue::Unresolved.into();
    };
    let DefinitionKind::ProvidedBinding(binding) = definition.kind(db) else {
        unreachable!("only supplied load definitions reach the Starlark loader");
    };
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let statement = binding.statement(&parsed);
    let call = load_call(&statement.value).expect("supplied statement is a load");
    let source = source_file.contents(db);
    let Some(module) = call.arguments.args.first() else {
        return ProvidedBindingValue::Unresolved.into();
    };
    let Some((module_name, _)) = string_value(&source[module.range()]) else {
        return ProvidedBindingValue::Unresolved.into();
    };
    let target = call
        .arguments
        .iter_source_order()
        .find_map(|argument| match argument {
            ArgOrKeyword::Arg(expr) => {
                (expr.node_index().load() == binding.binding.target).then_some(expr)
            }
            ArgOrKeyword::Keyword(keyword) => {
                (keyword.node_index().load() == binding.binding.target).then_some(&keyword.value)
            }
        })
        .expect("load binding target belongs to its statement");
    let Some((name, _)) = string_value(&source[target.range()]) else {
        return ProvidedBindingValue::Unresolved.into();
    };
    let error = |message: String| {
        let mut diagnostic =
            Diagnostic::new(DiagnosticId::lint("load-error"), Severity::Warning, message);
        diagnostic.annotate(Annotation::primary(
            Span::from(source_file.source).with_range(binding.binding.range),
        ));
        ProvidedBindingResolution {
            value: ProvidedBindingValue::Unresolved,
            diagnostics: vec![diagnostic],
        }
    };
    if name.starts_with('_') {
        return error(format!("Cannot load private symbol \"{name}\""));
    }
    match db.load_file(&module_name, source_file.dialect, source_file) {
        Ok(Some(loaded)) => {
            if loaded.source == source_file.source {
                return error("Cannot load the current file".to_owned());
            }
            ProvidedBindingValue::Export {
                file: db.starlark_program_file(loaded),
                name: Name::new(name),
            }
            .into()
        }
        Ok(None) => ProvidedBindingValue::Unresolved.into(),
        Err(error_message) => error(format!(
            "Could not resolve module \"{module_name}\": {error_message}"
        )),
    }
}

fn load_call(expr: &Expr) -> Option<&ExprCall> {
    let Expr::Call(call) = expr else { return None };
    let Expr::Name(name) = call.func.as_ref() else {
        return None;
    };
    (name.id == "load").then_some(call)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ruff_db::system::InMemorySystem;
    use ruff_db::system::SystemPath;
    use ruff_db::system::WritableSystem;
    use ruff_python_ast::Stmt;
    use ty_python_semantic::HasType;
    use ty_python_semantic::SemanticModel;

    use crate::Analysis;
    use crate::FilePosition;

    #[test]
    fn build_prelude_signature_tracks_loaded_declaration() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let dependency = fixture.add_file(&mut analysis.db, "defs.bzl", "");
        fixture.add_prelude_file(
            &mut analysis.db,
            "load(\"defs.bzl\", prelude_function = \"compute\")\n",
        );
        let build = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD",
            "prelude_function(1$0)",
            starpls_common::Dialect::Bazel,
            Some(starpls_common::FileInfo::Bazel {
                api_context: starpls_bazel::APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        let (_, pos) = fixture.cursor_pos.unwrap();
        for (ty, expected) in [("int", "int"), ("string", "str")] {
            analysis.update_file(
                dependency,
                format!("def compute(value):\n    # type: ({ty}) -> {ty}\n    return value\n"),
            );
            let help = analysis
                .snapshot()
                .signature_help(FilePosition {
                    file_id: build,
                    pos,
                })
                .unwrap()
                .unwrap();
            assert_eq!(
                help.signatures[0].label,
                format!("def compute(value: {expected}) -> {expected}")
            );
            let mut ordinary = build;
            ordinary.info = Some(starpls_common::FileInfo::Bazel {
                api_context: starpls_bazel::APIContext::Bzl,
                is_external: false,
            });
            let snapshot = analysis.snapshot();
            let diagnostics = ty_python_semantic::check_file_unwrap(
                &snapshot.db,
                snapshot.db.starlark_program_file(ordinary),
            );
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(
                diagnostics[0].id().as_str(),
                "unresolved-reference",
                "{diagnostics:?}"
            );
        }
    }

    #[test]
    fn loaded_contracts_update_unchanged_signature_consumer() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let dependency = fixture.add_file(&mut analysis.db, "defs.bzl", "");
        let caller = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            r#"
load("defs.bzl")
load("defs.bzl", "value", chosen = "compute")
observed = value
chosen(1$0)
"#,
        );
        loader.add_files_from_fixture(&fixture);
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        for (source, expected, value_type) in [
            (
                r#"value = 1
def compute(value):
    # type: (int) -> int
    return value
"#,
                "def compute(value: int) -> int",
                "Literal[1]",
            ),
            (
                r#"value = "text"
def compute(other):
    # type: (string) -> string
    return other
"#,
                "def compute(other: str) -> str",
                "Literal[\"text\"]",
            ),
        ] {
            analysis.update_file(dependency, source.to_owned());
            let snapshot = analysis.snapshot();
            let help = snapshot
                .signature_help(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            let [signature] = help.signatures.as_slice() else {
                panic!("{help:?}")
            };
            assert_eq!(signature.label, expected);
            assert_eq!(signature.active_parameter, Some(0));
            let db = &snapshot.db;
            let file = db.starlark_program_file(caller);
            let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
            let model = SemanticModel::new(db, file);
            let assignment = parsed
                .suite()
                .iter()
                .find_map(|statement| match statement {
                    Stmt::Assign(assignment) => Some(assignment),
                    _ => None,
                })
                .unwrap();
            let ty = assignment.value.inferred_type(&model).unwrap();
            assert_eq!(
                ty.display(db, &model.program_environment()).to_string(),
                value_type
            );
            let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| !diagnostic.headline_message().contains("load")),
                "{diagnostics:?}"
            );
        }
    }

    #[test]
    fn loads_participate_in_ordinary_binding_order() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "defs.bzl", "value = 42\n");
        let caller = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            r#"
before = value
load("defs.bzl", "value")
imported = value
value = "local"
after = value
"#,
        );
        loader.add_files_from_fixture(&fixture);
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let file = db.starlark_program_file(caller);
        let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
        let model = SemanticModel::new(db, file);
        let types: Vec<_> = parsed
            .suite()
            .iter()
            .filter_map(|statement| match statement {
                Stmt::Assign(assignment) => Some(
                    assignment
                        .value
                        .inferred_type(&model)
                        .unwrap()
                        .display(db, &model.program_environment())
                        .to_string(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(
            types,
            [
                "Unknown",
                "Literal[42]",
                "Literal[\"local\"]",
                "Literal[\"local\"]"
            ]
        );
        let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].id().to_string(), "unresolved-reference");
    }

    #[test]
    fn fetched_loads_refresh_ty_exports() {
        let disk = InMemorySystem::default();
        let loader = Arc::new(crate::SimpleFileLoader::default());
        let mut analysis = Analysis::with_system(loader, Default::default(), disk.clone());
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let caller = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "load(\"/external.bzl\", \"value\")\nresult = value\n",
        );
        let inferred = |analysis: &Analysis| {
            let snapshot = analysis.snapshot();
            let db = &snapshot.db;
            let file = db.starlark_program_file(caller);
            let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
            let statement = parsed.suite().last().unwrap();
            let Stmt::Assign(assignment) = statement else {
                panic!("fixture ends in an assignment")
            };
            let model = SemanticModel::new(db, file);
            assignment
                .value
                .inferred_type(&model)
                .unwrap()
                .display(db, &model.program_environment())
                .to_string()
        };
        assert_eq!(inferred(&analysis), "Unknown");
        disk.write_file(SystemPath::new("/external.bzl"), "value = 42\n")
            .unwrap();
        assert_eq!(inferred(&analysis), "Unknown");
        analysis.invalidate_loads();
        assert_eq!(inferred(&analysis), "Literal[42]");
    }

    #[test]
    fn signature_lookup_keeps_unrelated_loaded_bodies_lazy() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(
            &mut analysis.db,
            "defs.bzl",
            r#"
load("unused.bzl", "host")
def compute(value):
    # type: (int) -> int
    return value

def unrelated():
    return host.value
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "load(\"defs.bzl\", \"compute\")\ncompute(1$0)\n",
        );
        loader.add_files_from_fixture(&fixture);
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let help = analysis
            .snapshot()
            .signature_help(FilePosition { file_id, pos })
            .unwrap()
            .unwrap();
        let [signature] = help.signatures.as_slice() else {
            panic!("{help:?}")
        };
        assert_eq!(signature.label, "def compute(value: int) -> int");
        assert_eq!(*loader.requests.lock().unwrap(), ["defs.bzl"]);
    }
}
