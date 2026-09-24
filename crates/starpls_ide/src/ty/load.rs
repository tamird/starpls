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
use rustc_hash::FxHashMap;
use starpls_common::Db;
use starpls_hir::Db as _;
use starpls_syntax::source::string_value;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::definition::ProvidedBinding;
use ty_python_core::definition::ProvidedStatement;
use ty_python_core::global_scope;
use ty_python_core::place_table;
use ty_python_core::use_def_map;
use ty_python_core::ProgramFile;
use ty_python_semantic::provided::ProvidedBindingResolution;
use ty_python_semantic::provided::ProvidedBindingValue;
use ty_python_semantic::types::Type;

use crate::Database;

pub(crate) struct LoadSpelling {
    pub(crate) binding: ProvidedBinding,
    pub(crate) remote: Box<str>,
    pub(crate) remote_range: TextRange,
    pub(crate) explicit: bool,
    pub(crate) plain: bool,
}

fn spellings(call: &ExprCall, source: &str) -> Vec<LoadSpelling> {
    call.arguments
        .iter_source_order()
        .skip(1)
        .filter_map(|argument| {
            let (expression, target, alias) = match argument {
                ArgOrKeyword::Arg(expression) => (expression, expression.node_index().load(), None),
                ArgOrKeyword::Keyword(keyword) => (
                    &keyword.value,
                    keyword.node_index().load(),
                    Some(keyword.arg.as_ref()?),
                ),
            };
            let text = &source[expression.range()];
            let (remote, prefix) = string_value(text)?;
            let suffix = if text.ends_with("\"\"\"") || text.ends_with("'''") {
                3
            } else {
                1
            };
            let remote_range = TextRange::new(
                expression.start() + TextSize::from(prefix),
                expression.end() - TextSize::from(suffix),
            );
            let (name, range) = alias.map_or_else(
                || (Name::new(remote.as_ref()), remote_range),
                |alias| (alias.id.clone(), alias.range()),
            );
            let plain = &source[remote_range] == remote.as_ref();
            Some(LoadSpelling {
                binding: ProvidedBinding {
                    target,
                    name,
                    range,
                },
                remote,
                remote_range,
                explicit: alias.is_some(),
                plain,
            })
        })
        .collect()
}

pub(crate) struct Load<'db> {
    pub(crate) definition: Definition<'db>,
    pub(crate) spelling: LoadSpelling,
}

pub(crate) fn bindings(db: &Database, file: starpls_common::File) -> Vec<Load<'_>> {
    let program = db.starlark_program_file(file);
    let parsed = starpls_common::parsed_module(db, file).load(db);
    let index = ty_python_core::semantic_index(db, program);
    let source = file.contents(db);
    let mut loads = Vec::new();
    for statement in parsed.suite() {
        let Some(definitions) = index.provided_statement_definitions(statement) else {
            continue;
        };
        let Stmt::Expr(statement) = statement else {
            continue;
        };
        let Some(call) = load_call(&statement.value) else {
            continue;
        };
        for spelling in spellings(call, &source) {
            let Some(definition) = definitions.iter().find(|definition| {
                matches!(definition.kind(db), DefinitionKind::ProvidedBinding(binding) if binding.binding.target == spelling.binding.target)
            }) else { continue; };
            loads.push(Load {
                definition: *definition,
                spelling,
            });
        }
    }
    loads
}

pub(super) fn statements(db: &dyn Db, file: starpls_common::File) -> Vec<ProvidedStatement> {
    let parsed = starpls_common::parsed_module(db, file).load(db);
    let source = file.contents(db);
    parsed
        .suite()
        .iter()
        .filter_map(|statement| {
            let Stmt::Expr(statement) = statement else {
                return None;
            };
            let call = load_call(&statement.value)?;
            Some(ProvidedStatement {
                statement: statement.node_index().load(),
                bindings: spellings(call, &source)
                    .into_iter()
                    .map(|spelling| spelling.binding)
                    .collect(),
            })
        })
        .collect()
}

pub(super) fn resolve<'db>(
    db: &'db Database,
    definition: Definition<'db>,
) -> ProvidedBindingResolution<'db> {
    let Some((source_file, module_name, name)) = binding_names(db, definition) else {
        return ProvidedBindingValue::Unresolved.into();
    };
    let DefinitionKind::ProvidedBinding(binding) = definition.kind(db) else {
        unreachable!("only supplied load definitions reach the Starlark loader");
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
                return ProvidedBindingValue::Value(Type::unknown()).into();
            }
            let loaded_file = db.load_export_file(source_file, loaded, &name);
            if is_loaded_alias(db, loaded_file, &name) {
                return error(format!(
                    "Cannot load \"{name}\" from \"{module_name}\": loaded symbols are not exported"
                ));
            }
            ProvidedBindingValue::Export {
                file: loaded_file,
                name: Name::new(name),
            }
            .into()
        }
        // File diagnostics own module-resolution errors, including loads without bindings.
        Ok(None) => ProvidedBindingValue::Value(Type::unknown()).into(),
        Err(_) => ProvidedBindingValue::Value(Type::unknown()).into(),
    }
}

pub(crate) fn binding_names(
    db: &dyn super::interface::Db,
    definition: Definition<'_>,
) -> Option<(starpls_common::File, Box<str>, Box<str>)> {
    let file = definition.program_file(db);
    let source_file = db.starlark_file(file)?;
    let DefinitionKind::ProvidedBinding(binding) = definition.kind(db) else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let statement = binding.statement(&parsed);
    let call = load_call(&statement.value).expect("supplied statement is a load");
    let source = source_file.contents(db);
    let module = call.arguments.args.first()?;
    let (module_name, _) = string_value(&source[module.range()])?;
    let spelling = spellings(call, &source)
        .into_iter()
        .find(|spelling| spelling.binding.target == binding.binding.target)
        .expect("load binding target belongs to its statement");
    let name = spelling.remote;
    Some((source_file, module_name, name))
}

/// A load introduces a local binding, but only a declaration or assignment exports it again.
/// Inspect binding origins without inferring the module's unrelated values.
fn is_loaded_alias(db: &Database, file: ProgramFile<'_>, name: &str) -> bool {
    let scope = global_scope(db, file);
    let Some(symbol) = place_table(db, scope).symbol_id(name) else {
        return false;
    };
    let mut has_load = false;
    for definition in use_def_map(db, scope)
        .end_of_scope_symbol_bindings(symbol)
        .filter_map(|binding| binding.binding.definition())
    {
        if !matches!(definition.kind(db), DefinitionKind::ProvidedBinding(_)) {
            return false;
        }
        has_load = true;
    }
    has_load
}

/// A syntactic load edge, including files whose exported values are unused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadDependency {
    pub module: Box<str>,
    pub range: TextRange,
    pub resolution: LoadResolution,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadResolution {
    Resolved(starpls_common::File),
    Pending,
    Failed(String),
}

pub(crate) fn dependencies(db: &dyn Db, file: starpls_common::File) -> &[LoadDependency] {
    direct_loads(db, file.source, (file.dialect, file.info))
}

#[salsa::tracked(returns(ref))]
fn direct_loads(
    db: &dyn Db,
    source: ruff_db::files::File,
    context: (starpls_common::Dialect, Option<starpls_common::FileInfo>),
) -> Vec<LoadDependency> {
    let (dialect, info) = context;
    let file = starpls_common::File {
        source,
        dialect,
        info,
    };
    statement_locations(db, file)
        .iter()
        .map(|(module, range)| LoadDependency {
            module: module.clone(),
            range: *range,
            resolution: resolve_dependency(db, file, module),
        })
        .collect()
}

pub(crate) fn resolve_dependency(
    db: &dyn Db,
    file: starpls_common::File,
    module: &str,
) -> LoadResolution {
    salsa::Database::unwind_if_revision_cancelled(db);
    match db.load_file(module, file.dialect, file) {
        Ok(Some(file)) => LoadResolution::Resolved(file),
        Ok(None) => LoadResolution::Pending,
        Err(error) => LoadResolution::Failed(format!("{error:#}")),
    }
}

/// Reading source locations must not resolve other loads in the same file.
pub(crate) fn statement_locations(
    db: &dyn Db,
    file: starpls_common::File,
) -> &[(Box<str>, TextRange)] {
    load_statements(db, file.source, (file.dialect, file.info))
}

#[salsa::tracked(returns(ref))]
fn load_statements(
    db: &dyn Db,
    source: ruff_db::files::File,
    context: (starpls_common::Dialect, Option<starpls_common::FileInfo>),
) -> Vec<(Box<str>, TextRange)> {
    let (dialect, info) = context;
    let file = starpls_common::File {
        source,
        dialect,
        info,
    };
    let parsed = starpls_common::parsed_module(db, file).load(db);
    let source = file.contents(db);
    parsed
        .suite()
        .iter()
        .filter_map(|statement| {
            salsa::Database::unwind_if_revision_cancelled(db);
            let Stmt::Expr(statement) = statement else {
                return None;
            };
            let call = load_call(&statement.value)?;
            let module = call.arguments.args.first()?;
            let (name, _) = string_value(&source[module.range()])?;
            Some((name, module.range()))
        })
        .collect()
}

// This recursion reads only module edges. A Salsa cycle therefore identifies
// a load cycle, independently of value inference or the order of checked roots.
#[salsa::tracked(returns(copy), cycle_initial=|_, _, _, _| true)]
fn reaches_cycle(
    db: &dyn Db,
    source: ruff_db::files::File,
    context: (starpls_common::Dialect, Option<starpls_common::FileInfo>),
) -> bool {
    direct_loads(db, source, context).iter().any(|edge| {
        let LoadResolution::Resolved(file) = edge.resolution else {
            return false;
        };
        reaches_cycle(db, file.source, (file.dialect, file.info))
    })
}

fn cycle_path(db: &dyn Db, mut file: starpls_common::File) -> Option<Vec<String>> {
    if !reaches_cycle(db, file.source, (file.dialect, file.info)) {
        return None;
    }
    let mut path = Vec::new();
    let mut positions = FxHashMap::default();
    loop {
        salsa::Database::unwind_if_revision_cancelled(db);
        if let Some(&start) = positions.get(&file) {
            return Some(
                path[start..]
                    .iter()
                    .chain(std::iter::once(&file))
                    .map(|file: &starpls_common::File| format!("- {}", file.source.path(db)))
                    .collect(),
            );
        }
        positions.insert(file, path.len());
        path.push(file);
        file = dependencies(db, file).iter().find_map(|edge| {
            let LoadResolution::Resolved(next) = edge.resolution else {
                return None;
            };
            reaches_cycle(db, next.source, (next.dialect, next.info)).then_some(next)
        })?;
    }
}

pub(super) fn diagnostics(db: &Database, file: ProgramFile<'_>) -> Vec<Diagnostic> {
    let Some(source_file) = db.starlark_file(file) else {
        return Vec::new();
    };
    let mut diagnostics = Vec::new();
    for LoadDependency {
        module,
        range,
        resolution,
    } in dependencies(db, source_file)
    {
        let message = match resolution {
            LoadResolution::Resolved(loaded) => {
                if loaded.source == source_file.source {
                    "Cannot load the current file".to_owned()
                } else if db.environment().options(db).skip_load_cycle_checks {
                    continue;
                } else if let Some(cycle) = cycle_path(db, *loaded) {
                    format!("Detected circular import\n{}", cycle.join("\n"))
                } else {
                    continue;
                }
            }
            LoadResolution::Pending => continue,
            LoadResolution::Failed(error) => {
                format!("Could not resolve module \"{module}\": {error}")
            }
        };
        let mut diagnostic =
            Diagnostic::new(DiagnosticId::lint("load-error"), Severity::Warning, message);
        diagnostic.annotate(Annotation::primary(
            Span::from(source_file.source).with_range(*range),
        ));
        diagnostics.push(diagnostic);
    }
    diagnostics
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
    fn shared_load_tails_are_resolved_once_across_roots() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "leaf.bzl", "value = 1\n");
        fixture.add_file(&mut analysis.db, "tail.bzl", "load('leaf.bzl')\n");
        fixture.add_file(&mut analysis.db, "left.bzl", "load('tail.bzl')\n");
        fixture.add_file(&mut analysis.db, "right.bzl", "load('tail.bzl')\n");
        let first = fixture.add_file(
            &mut analysis.db,
            "first.bzl",
            "load('left.bzl')\nload('right.bzl')\n",
        );
        let second = fixture.add_file(&mut analysis.db, "second.bzl", "load('tail.bzl')\n");
        loader.add_files_from_fixture(&fixture);
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        for file in [first, second, second, first] {
            assert!(super::diagnostics(db, db.starlark_program_file(file)).is_empty());
        }
        let requests = loader.requests.lock().unwrap();
        assert_eq!(
            requests.iter().filter(|path| *path == "leaf.bzl").count(),
            1,
            "{requests:?}"
        );
        assert_eq!(requests.len(), 6, "{requests:?}");
    }

    #[test]
    fn overlapping_load_cycles_are_independent_of_root_order() {
        for reverse in [false, true] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
            let first = fixture.add_file(&mut analysis.db, "first.bzl", "load('shared.bzl')\n");
            let second = fixture.add_file(&mut analysis.db, "second.bzl", "load('shared.bzl')\n");
            let shared = fixture.add_file(
                &mut analysis.db,
                "shared.bzl",
                "load('first.bzl')\nload('second.bzl')\n",
            );
            loader.add_files_from_fixture(&fixture);
            let roots = if reverse {
                [second, first]
            } else {
                [first, second]
            };
            for cyclic in [true, false, true] {
                analysis.update_file(
                    shared,
                    if cyclic {
                        "load('first.bzl')\nload('second.bzl')\n"
                    } else {
                        "value = 1\n"
                    }
                    .into(),
                );
                let snapshot = analysis.snapshot();
                for root in roots {
                    let db = &snapshot.db;
                    let diagnostics = super::diagnostics(db, db.starlark_program_file(root));
                    assert_eq!(!diagnostics.is_empty(), cyclic, "{diagnostics:?}");
                }
            }
        }
    }

    #[test]
    fn load_revision_retries_failed_edges_and_detects_new_cycles() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let caller = fixture.add_file(&mut analysis.db, "caller.bzl", "load('new.bzl')\n");
        loader.add_files_from_fixture(&fixture);
        {
            let snapshot = analysis.snapshot();
            let db = &snapshot.db;
            let diagnostics = super::diagnostics(db, db.starlark_program_file(caller));
            assert!(diagnostics[0]
                .headline_message()
                .contains("Could not resolve module"));
        }
        fixture.add_file(&mut analysis.db, "new.bzl", "load('caller.bzl')\n");
        loader.add_files_from_fixture(&fixture);
        analysis.invalidate_loads();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let diagnostics = super::diagnostics(db, db.starlark_program_file(caller));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(
            diagnostics[0]
                .headline_message()
                .contains("circular import"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn load_cycles_do_not_depend_on_recursive_value_inference() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "a.bzl", "load(\"b.bzl\", \"b\")\na = 1\n");
        let dependency = fixture.add_file(&mut analysis.db, "b.bzl", "b = 2\n");
        let caller = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "load(\"a.bzl\", \"a\")\nresult = a\n",
        );
        loader.add_files_from_fixture(&fixture);

        for (source, cyclic) in [
            ("load(\"a.bzl\", \"a\")\nb = 2\n", true),
            ("b = 2\n", false),
            ("load(\"a.bzl\", \"a\")\nb = 2\n", true),
        ] {
            analysis.update_file(dependency, source.to_owned());
            let snapshot = analysis.snapshot();
            let db = &snapshot.db;
            let diagnostics = super::diagnostics(db, db.starlark_program_file(caller));
            assert_eq!(
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.id().as_str() == "load-error"
                        && diagnostic.headline_message().contains("circular import")
                }),
                cyclic,
                "{diagnostics:?}",
            );
        }
    }

    #[test]
    fn loads_without_bindings_still_report_module_errors() {
        let (analysis, fixture) = Analysis::from_single_file_fixture("load(\"missing.bzl\")\n");
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let diagnostics = super::diagnostics(db, db.starlark_program_file(fixture.main_file()));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0]
            .headline_message()
            .starts_with("Could not resolve module"));
    }

    #[test]
    fn loaded_bindings_require_an_explicit_assignment_to_be_exported() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "origin.bzl", "value = 1\n");
        let dependency = fixture.add_file(&mut analysis.db, "middle.bzl", "");
        let caller = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "load(\"middle.bzl\", \"value\")\nresult = value\n",
        );
        loader.add_files_from_fixture(&fixture);

        for (source, exported) in [
            ("load(\"origin.bzl\", \"value\")\n", false),
            ("load(\"origin.bzl\", \"value\")\nvalue = value\n", true),
            ("load(\"origin.bzl\", \"value\")\n", false),
        ] {
            analysis.update_file(dependency, source.to_owned());
            let snapshot = analysis.snapshot();
            let db = &snapshot.db;
            let diagnostics =
                ty_python_semantic::types::check_types(db, db.starlark_program_file(caller));
            assert_eq!(
                diagnostics
                    .iter()
                    .any(|diagnostic| { diagnostic.headline_message().contains("not exported") }),
                !exported,
                "{diagnostics:?}",
            );
        }
    }

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
