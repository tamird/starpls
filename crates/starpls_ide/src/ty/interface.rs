//! Trusted client contracts use ordinary stub exports; implementations remain separate files.

use std::collections::hash_map::Entry;

use ruff_db::files::FileRange;
use ruff_python_ast::Expr;
use ruff_python_ast::HasNodeIndex;
use ruff_python_ast::NodeIndex;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtClassDef;
use ruff_text_size::Ranged;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use salsa::Setter;
use starpls_common::File;
use starpls_hir::Db as _;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::global_scope;
use ty_python_core::place_table;
use ty_python_core::use_def_map;
use ty_python_core::ProgramFile;
use ty_python_semantic::provided::ProvidedField;
use ty_python_semantic::types::ide_support::definitions_for_name;
use ty_python_semantic::types::list_members::all_end_of_scope_members;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::TypeDefinition;
use ty_python_semantic::ImportAliasResolution;
use ty_python_semantic::ProgramEnvironment;
use ty_python_semantic::SemanticModel;

use crate::Analysis;
use crate::Database;

#[salsa::db]
pub(crate) trait Db: ty_python_semantic::Db + starpls_hir::Db {
    fn starlark_program_file(&self, file: File) -> ProgramFile<'_>;
    fn starlark_file(&self, file: ProgramFile<'_>) -> Option<File>;
}

#[salsa::db]
impl Db for Database {
    fn starlark_program_file(&self, file: File) -> ProgramFile<'_> {
        Database::starlark_program_file(self, file)
    }
    fn starlark_file(&self, file: ProgramFile<'_>) -> Option<File> {
        Database::starlark_file(self, file)
    }
}

pub(super) fn diagnostics(db: &Database, file: File) -> Vec<ruff_db::diagnostic::Diagnostic> {
    use ruff_db::diagnostic::Annotation;
    use ruff_db::diagnostic::Diagnostic;
    use ruff_db::diagnostic::DiagnosticId;
    use ruff_db::diagnostic::Severity;
    use ruff_db::diagnostic::Span;
    if !file.is_type_interface(db) {
        return Vec::new();
    }
    let program = db.starlark_program_file(file);
    let parsed = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
    let model = SemanticModel::new(db, program);
    let mut diagnostics = Vec::new();
    let mut report = |range, message: String| {
        let mut diagnostic = Diagnostic::new(
            DiagnosticId::Lint(super::diagnostics::INVALID_PROVIDER_INTERFACE.name()),
            Severity::Error,
            message,
        );
        diagnostic.annotate(Annotation::primary(
            Span::from(file.source).with_range(range),
        ));
        diagnostics.push(diagnostic);
    };
    for statement in parsed.suite() {
        let Stmt::ClassDef(class) = statement else {
            continue;
        };
        if !is_provider_class(class) {
            let index = ty_python_core::semantic_index(db, program);
            let [definition] = index.definitions(class) else {
                continue;
            };
            let ty = model.definition_type(*definition);
            let instance = ty.to_instance_approximation(db, &model.program_environment());
            if class
                .arguments
                .as_ref()
                .is_some_and(|arguments| !arguments.keywords.is_empty())
                && !instance.is_some_and(|instance| matches!(instance, Type::TypedDict(_)))
            {
                report(
                    class.name.range,
                    "Only TypedDict declarations accept class keywords".into(),
                );
            } else if !instance.is_some_and(|instance| {
                matches!(instance, Type::ProtocolInstance(_) | Type::TypedDict(_))
            }) {
                report(
                    class.name.range,
                    "Interface classes with bases must declare a Protocol or TypedDict".into(),
                );
            }
            continue;
        }
        if !class.body.iter().any(|statement| matches!(statement, Stmt::FunctionDef(function) if function.name.as_str() == "__init__")) {
            report(class.name.range, "Provider classes require an explicit __init__ declaration".into());
        }
        for statement in &class.body {
            let Stmt::AnnAssign(field) = statement else {
                continue;
            };
            let Some(name) = field.target.as_name_expr() else {
                continue;
            };
            let qualifiers = model.type_qualifiers(name.into());
            if name.id.starts_with("__") && name.id.ends_with("__") {
                report(
                    name.range(),
                    "Provider fields cannot define special methods".into(),
                );
            } else if !qualifiers.contains(ty_python_semantic::TypeQualifiers::FINAL)
                || qualifiers.contains(ty_python_semantic::TypeQualifiers::CLASS_VAR)
            {
                report(
                    name.range(),
                    "Provider fields require an instance annotation of the form Final[T]".into(),
                );
            }
        }
    }
    diagnostics.extend(pairing_diagnostics(db, file));

    diagnostics
}

pub(super) fn pairing_diagnostics(
    db: &Database,
    file: File,
) -> Vec<ruff_db::diagnostic::Diagnostic> {
    use ruff_db::diagnostic::Annotation;
    use ruff_db::diagnostic::Diagnostic;
    use ruff_db::diagnostic::DiagnosticId;
    use ruff_db::diagnostic::Severity;
    use ruff_db::diagnostic::Span;
    let program = db.starlark_program_file(file);
    let parsed = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
    let mut diagnostics = Vec::new();
    let mut report = |range, message: String| {
        let mut diagnostic = Diagnostic::new(
            DiagnosticId::Lint(super::diagnostics::INVALID_PROVIDER_INTERFACE.name()),
            Severity::Error,
            message,
        );
        diagnostic.annotate(Annotation::primary(
            Span::from(file.source).with_range(range),
        ));
        diagnostics.push(diagnostic);
    };
    for definitions in provider_pairs(db)
        .values()
        .filter(|definitions| definitions.len() > 1)
    {
        for definition in definitions {
            if definition.program_file(db) == program {
                report(
                    definition.kind(db).target_range(&parsed),
                    "Multiple stub classes describe the same provider declaration".into(),
                );
            }
        }
    }
    diagnostics
}

impl Analysis {
    /// Replace the explicit trusted contracts atomically after validating every mapping.
    pub fn set_type_interfaces(
        &mut self,
        interfaces: impl IntoIterator<Item = (File, File)>,
    ) -> anyhow::Result<()> {
        let Self { db } = self;
        let mut mappings = FxHashMap::default();
        for (source, interface) in interfaces {
            if !source.allows_native_annotations(db)
                || source
                    .path(db)
                    .extension()
                    .is_none_or(|extension| extension != "bzl")
            {
                anyhow::bail!(
                    "type interface source must be a .bzl file: {}",
                    source.path(db).display()
                );
            }
            if !interface.is_type_interface(db) {
                anyhow::bail!(
                    "type interface must be a .bzli file: {}",
                    interface.path(db).display()
                );
            }
            match mappings.entry(source.source) {
                Entry::Vacant(entry) => {
                    entry.insert((source, interface));
                }
                Entry::Occupied(entry) => {
                    let (source, previous): &(File, File) = entry.get();
                    anyhow::bail!(
                        "duplicate type interface for {}: {} and {}",
                        source.path(db).display(),
                        previous.path(db).display(),
                        interface.path(db).display()
                    );
                }
            }
        }
        db.environment().set_type_interfaces(db).to(mappings);
        Ok(())
    }

    /// Physical inputs whose declarations may be referenced by trusted interfaces.
    pub fn type_interface_sources(&self) -> Vec<File> {
        let Self { db } = self;
        db.environment()
            .type_interfaces(db)
            .values()
            .map(|(source, _)| *source)
            .collect()
    }

    pub fn type_interface_files(&self) -> Vec<File> {
        let Self { db } = self;
        let mut files: Vec<_> = db
            .environment()
            .type_interfaces(db)
            .values()
            .map(|(_, interface)| *interface)
            .collect();
        files.sort_by(|left, right| left.path(db).cmp(right.path(db)));
        files.dedup();
        files
    }
}

impl Database {
    /// Pair by a unique source call, including aliases, without inferring its result.
    pub(super) fn provider_interface<'db>(
        &'db self,
        source: ProgramFile<'db>,
        call: NodeIndex,
    ) -> Option<Type<'db>> {
        let [definition] = provider_pairs(self)
            .get(&(source, call.as_u32()?))?
            .as_slice()
        else {
            return None;
        };
        Some(SemanticModel::new(self, definition.program_file(self)).definition_type(*definition))
    }

    pub(crate) fn type_interface(&self, from: File, source: File) -> Option<File> {
        let mappings = self.environment().type_interfaces(self);
        if mappings.is_empty() {
            return None;
        }
        let (_, interface) = mappings.get(&source.source)?;
        // An interface may import the implementation's existing nominal providers.
        // It must not resolve that import back to its own declaration of the name.
        (interface.source != from.source).then_some(*interface)
    }

    pub(crate) fn load_export_file<'db>(
        &'db self,
        from: File,
        source: File,
        name: &str,
    ) -> ProgramFile<'db> {
        if let Some(interface) = self.type_interface(from, source) {
            let file = self.starlark_program_file(interface);
            if !export_definitions(self, file, name).is_empty() {
                return file;
            }
        }
        self.starlark_program_file(source)
    }

    /// Navigation may find an implementation even when its signature is incompatible.
    /// This lookup never participates in selecting or inferring the trusted contract.
    pub(crate) fn interface_implementation<'db>(
        &'db self,
        definition: Definition<'db>,
    ) -> Vec<Definition<'db>> {
        let Some((from, module, name)) = super::load::binding_names(self, definition) else {
            return Vec::new();
        };
        let Ok(Some(source)) = starpls_common::Db::load_file(self, &module, from.dialect, from)
        else {
            return Vec::new();
        };
        let source_file = self.starlark_program_file(source);
        if self.load_export_file(from, source, &name) == source_file {
            return Vec::new();
        }
        export_definitions(self, source_file, &name)
    }
}

/// Correspondence is syntax and binding provenance; type inference begins after selection.
#[salsa::tracked(returns(ref))]
fn provider_pairs(db: &dyn Db) -> FxHashMap<(ProgramFile<'_>, u32), Vec<Definition<'_>>> {
    let mut pairs: FxHashMap<_, Vec<_>> = FxHashMap::default();
    for (implementation, interface) in db.environment().type_interfaces(db).values() {
        let file = db.starlark_program_file(*interface);
        for symbol in place_table(db, global_scope(db, file)).symbols() {
            let definitions = export_definitions(db, file, symbol.name());
            let [definition] = definitions.as_slice() else {
                continue;
            };
            let DefinitionKind::Class(class) = definition.kind(db) else {
                continue;
            };
            let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
            if !is_provider_class(class.node(&parsed)) {
                continue;
            }
            let implementations =
                export_definitions(db, db.starlark_program_file(*implementation), symbol.name());
            let [implementation] = implementations.as_slice() else {
                continue;
            };
            let Some(origin) = provider_origin(
                db,
                *implementation,
                ProviderPart::Constructor,
                &mut FxHashSet::default(),
            ) else {
                continue;
            };
            let definitions = pairs.entry(origin).or_default();
            if !definitions.contains(definition) {
                definitions.push(*definition);
            }
        }
    }
    pairs
}

fn provider_origin<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    part: ProviderPart,
    visited: &mut FxHashSet<Definition<'db>>,
) -> Option<(ProgramFile<'db>, u32)> {
    if !visited.insert(definition) {
        return None;
    }
    let file = definition.program_file(db);
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    match definition.kind(db) {
        DefinitionKind::Assignment(assignment) => match assignment.value(&parsed) {
            Expr::Call(call) => {
                if assignment.unpack().is_some() {
                    let node = ruff_python_ast::find_node::covering_node(
                        parsed.syntax().into(),
                        call.range(),
                    );
                    let ruff_python_ast::AnyNodeRef::StmtAssign(parent) = node.parent()? else {
                        return None;
                    };
                    let [Expr::Tuple(tuple)] = parent.targets.as_slice() else {
                        return None;
                    };
                    let [constructor, raw] = tuple.elts.as_slice() else {
                        return None;
                    };
                    let target = match part {
                        ProviderPart::Constructor => constructor,
                        ProviderPart::Raw => raw,
                    };
                    if target.range() != assignment.target(&parsed).range() {
                        return None;
                    }
                } else if matches!(part, ProviderPart::Raw) {
                    return None;
                }
                Some((file, call.node_index().load().as_u32()?))
            }
            Expr::Name(name) => {
                let model = SemanticModel::new(db, file);
                let definitions = definitions_for_name(
                    &model,
                    &name.id,
                    name.into(),
                    ImportAliasResolution::PreserveAliases,
                );
                let [definition] = definitions.as_slice() else {
                    return None;
                };
                provider_origin(db, definition.definition()?, part, visited)
            }
            _ => None,
        },
        DefinitionKind::ProvidedBinding(_) => {
            let (from, module, name) = super::load::binding_names(db, definition)?;
            let source = starpls_common::Db::load_file(db, &module, from.dialect, from).ok()??;
            let definitions = export_definitions(db, db.starlark_program_file(source), &name);
            let [definition] = definitions.as_slice() else {
                return None;
            };
            provider_origin(db, *definition, part, visited)
        }
        _ => None,
    }
}

pub(super) fn provider_implementation<'db>(
    db: &'db Database,
    source: File,
    class: Type<'db>,
) -> Option<(ProgramFile<'db>, FileRange)> {
    let environment = ProgramEnvironment::from_file(db.starlark_program_file(source));
    let class = provider_definition(db, class, &environment)?;
    provider_export_origin(db, source, &class.name(db)?, ProviderPart::Constructor)
}

#[derive(Clone, Copy)]
pub(super) enum ProviderPart {
    Constructor,
    Raw,
}

pub(super) fn provider_export_origin<'db>(
    db: &'db dyn Db,
    source: File,
    name: &str,
    part: ProviderPart,
) -> Option<(ProgramFile<'db>, FileRange)> {
    let definitions = export_definitions(db, db.starlark_program_file(source), name);
    let [definition] = definitions.as_slice() else {
        return None;
    };
    let (program, node) = provider_origin(db, *definition, part, &mut FxHashSet::default())?;
    let parsed = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
    Some((
        program,
        FileRange::new(
            program.file(db),
            parsed.get_by_index(NodeIndex::from(node)).range(),
        ),
    ))
}

/// Read annotation types and origins from Ty's class scope, including recursive fields.
pub(super) fn provider_fields<'db>(
    db: &'db Database,
    class: Type<'db>,
    environment: &ProgramEnvironment<'db>,
) -> Option<Vec<ProvidedField<'db>>> {
    let definition = provider_definition(db, class, environment)?;
    let DefinitionKind::Class(class) = definition.kind(db) else {
        return None;
    };
    let file = definition.program_file(db);
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let class = class.node(&parsed);
    let index = ty_python_core::semantic_index(db, file);
    let scope = index
        .node_scope(ty_python_core::scope::NodeWithScopeRef::Class(class))
        .to_scope_id(db, file);
    let members: Vec<_> = all_end_of_scope_members(db, scope).collect();
    let mut fields = Vec::new();
    for statement in &class.body {
        let Stmt::AnnAssign(assignment) = statement else {
            continue;
        };
        let name = assignment.target.as_name_expr()?;
        let member = members
            .iter()
            .find(|member| member.member.name == name.id)?;
        fields.push(ProvidedField {
            name: name.id.clone(),
            ty: member.member.ty,
            source: Some(FileRange::new(file.file(db), name.range())),
        });
    }
    Some(fields)
}

// Provider correspondence must remain independent of type inference: source
// provider inference asks for this correspondence before constructing its type.
fn is_provider_class(class: &StmtClassDef) -> bool {
    class
        .arguments
        .as_ref()
        .is_none_or(|arguments| arguments.is_empty())
}

pub(super) fn provider_definition<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
    environment: &ProgramEnvironment<'db>,
) -> Option<Definition<'db>> {
    let TypeDefinition::StaticClass(definition) = ty.definition(db, environment)? else {
        return None;
    };
    let DefinitionKind::Class(class) = definition.kind(db) else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db);
    is_provider_class(class.node(&parsed)).then_some(definition)
}

pub(crate) fn export_definitions<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    name: &str,
) -> Vec<Definition<'db>> {
    let scope = global_scope(db, file);
    let Some(symbol) = place_table(db, scope).symbol_id(name) else {
        return Vec::new();
    };
    use_def_map(db, scope)
        .end_of_scope_symbol_bindings(symbol)
        .filter_map(|binding| binding.binding.definition())
        .filter(|definition| !matches!(definition.kind(db), DefinitionKind::ProvidedBinding(_)))
        .collect()
}

#[cfg(test)]
mod tests {
    use starpls_bazel::APIContext;
    use starpls_common::Dialect;
    use starpls_common::FileInfo;
    use starpls_hir::Fixture;

    use crate::Analysis;
    use crate::FilePosition;
    use crate::LocationLink;

    #[test]
    fn protocol_methods_use_ordinary_types_and_editor_origins() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        // The private helper name also exists in the source, but it describes
        // no provider identity in the interface.
        let source = fixture.add_file(&mut analysis.db, "source.bzl", "_Builder = provider(fields=['value'])\noriginal = _Builder(value='source')\ndef make(): pass\n");
        let interface_text = "class _Builder(Protocol):\n    def set(self, value: int) -> _Builder: ...\n    def build(self) -> str: ...\ndef make() -> _Builder: ...\n";
        let interface = fixture.add_file(&mut analysis.db, "source.bzli", interface_text);
        let caller_text = "load('source.bzl', 'make')\nresult: str = make().set(1).build()\n";
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", caller_text);
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        analysis.set_type_interfaces([(source, interface)]).unwrap();
        for file in [source, interface, caller] {
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }
        let locations = analysis
            .snapshot()
            .goto_definition(
                FilePosition {
                    file_id: caller,
                    pos: (caller_text.find(".build").unwrap() as u32 + 1).into(),
                },
                false,
            )
            .unwrap()
            .unwrap();
        let [LocationLink::Local {
            target_file_id,
            target_selection_range,
            origin_selection_range: _,
            target_range: _,
        }] = locations.as_slice()
        else {
            panic!("{locations:?}");
        };
        assert_eq!(*target_file_id, interface.source);
        assert_eq!(&interface_text[*target_selection_range], "build");
        for (source, expected) in [
            (
                caller_text.replace("set(1)", "set('bad')"),
                "invalid-argument-type",
            ),
            (
                caller_text.replace("result: str", "result: int"),
                "invalid-assignment",
            ),
        ] {
            analysis.update_file(caller, source);
            let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
            let [diagnostic] = diagnostics.as_slice() else {
                panic!("{diagnostics:?}");
            };
            assert_eq!(diagnostic.id().as_str(), expected);
        }
        analysis.update_file(caller, caller_text.into());
        analysis.update_file(
            interface,
            interface_text.replace("value: int", "value: str"),
        );
        let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("{diagnostics:?}");
        };
        assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
    }

    #[test]
    fn typed_dictionary_contracts_retain_keys_across_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "def make(): return {'name': 'example'}\n",
        );
        let stub = "class _Row(TypedDict):\n    name: str\n    count: NotRequired[int]\ndef make() -> _Row: ...\n";
        let interface = fixture.add_file(&mut analysis.db, "source.bzli", stub);
        let caller_text = "load('source.bzl', 'make')\nrow = make()\nvalue = row['name']\nname: str = value\ncount: int | None = row.get('count')\n";
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", caller_text);
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, interface)]).unwrap();
        for file in [interface, caller] {
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }
        for (declaration, expected_type, valid) in [
            (stub.to_owned(), "str", true),
            (stub.replace("name: str", "name: int"), "int", false),
            (stub.to_owned(), "str", true),
        ] {
            analysis.update_file(interface, declaration);
            let snapshot = analysis.snapshot();
            let diagnostics = snapshot.diagnostics(caller).unwrap();
            if valid {
                assert!(diagnostics.is_empty(), "{diagnostics:?}");
            } else {
                let [diagnostic] = diagnostics.as_slice() else {
                    panic!("{diagnostics:?}");
                };
                assert_eq!(diagnostic.id().as_str(), "invalid-assignment");
            }
            let hover = snapshot
                .hover(FilePosition {
                    file_id: caller,
                    pos: (caller_text.find("value =").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            assert!(
                hover
                    .contents
                    .value
                    .contains(&format!("value: {expected_type}")),
                "{}",
                hover.contents.value
            );
            let completions = snapshot
                .completions(
                    FilePosition {
                        file_id: caller,
                        pos: (caller_text.find("'name'").unwrap() as u32 + 2).into(),
                    },
                    None,
                )
                .unwrap()
                .unwrap();
            for name in ["name", "count"] {
                assert!(
                    completions.iter().any(|item| item.label == name),
                    "{completions:?}"
                );
            }
        }
    }

    #[test]
    fn rules_rs_metadata_stub_preserves_field_types() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "data.bzl",
            "DEP_DATA = {}\nEXTRA = 'source export'\n",
        );
        let interface = fixture.add_file(
            &mut analysis.db,
            "data.bzli",
            include_str!("../../../../stubs/rules_rs/data.bzli"),
        );
        let caller_text = "load('data.bzl', 'DEP_DATA', 'EXTRA')\nrow = DEP_DATA['sample']\nname: str = row['crate_name']\nalias: str = row['aliases']['//:dep']\nbinary: str = row.get('binaries', {}).keys()[0]\nplatform_deps: list[str] = row.get('dev_deps_by_platform', {}).values()[0]\nfeatures: list[str] = row['crate_features']\nlint: str | None = row.get('lint_config')\nextra: str = EXTRA\n";
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", caller_text);
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, interface)]).unwrap();
        for file in [interface, caller] {
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }
        analysis.update_file(caller, format!("{caller_text}row['deps'].append(42)\n"));
        let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("{diagnostics:?}");
        };
        assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
    }

    #[test]
    fn interface_bases_require_supported_type_identity() {
        let (mut analysis, _) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let file = fixture.add_file(&mut analysis.db, "types.bzli", "");
        for source in [
            "class Concrete:\n    def __init__(self) -> None: ...\nclass _Derived(Concrete): pass\n",
            "def Protocol() -> int: ...\nclass _Derived(Protocol): pass\n",
            "def TypedDict() -> int: ...\nclass _Derived(TypedDict): pass\n",
        ] {
            analysis.update_file(file, source.into());
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(diagnostics.iter().any(|diagnostic| diagnostic.headline_message() == "Interface classes with bases must declare a Protocol or TypedDict"), "{diagnostics:?}");
        }
    }

    #[test]
    fn provider_classes_share_source_identity_and_recursive_fields() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "Info = provider(fields=['value', 'next'])\ndef consume(value: Info) -> Info:\n    return value\noriginal = Info(value='source', next=None)\n",
        );
        let interface = fixture.add_file(
            &mut analysis.db,
            "source.bzli",
            "class Info:\n    value: Final[str]\n    next: Final[Info | None]\n    def __init__(self, *, value: str, next: Info | None) -> None: ...\n",
        );
        let caller_text = "load('source.bzl', 'Info', 'consume', 'original')\nitem = consume(Info(value='ok', next=original))\ntext = item.value\n";
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", caller_text);
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        analysis.set_type_interfaces([(source, interface)]).unwrap();
        for file in [source, interface, caller] {
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }
        let hover = analysis
            .snapshot()
            .hover(FilePosition {
                file_id: caller,
                pos: (caller_text.rfind("value").unwrap() as u32).into(),
            })
            .unwrap()
            .unwrap();
        assert!(
            hover.contents.value.contains("value: str"),
            "{}",
            hover.contents.value
        );
        for bad_call in [
            "Info(value=42, next=None)",
            "Info(value='missing')",
            "item.value = 'changed'",
            "Other = provider(fields=['value', 'next'])\nconsume(Other(value='ok', next=None))",
        ] {
            analysis.update_file(caller, format!("{caller_text}{bad_call}\n"));
            let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
            assert_eq!(diagnostics.len(), 1, "{bad_call}: {diagnostics:?}");
        }
    }

    #[test]
    fn provider_aliases_raw_constructors_and_editor_origins_follow_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(&mut analysis.db, "source.bzl", "");
        let interface = fixture.add_file(&mut analysis.db, "source.bzli", "");
        let caller_text = "load('source.bzl', 'Info', 'raw')\nitem = raw(value='ok')\ntext = item.value\ndef consume(target: Target) -> str:\n    return target[Info].value\n";
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", caller_text);
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        analysis.set_type_interfaces([(source, interface)]).unwrap();
        for (prefix, annotation, valid) in [
            ("", "str", true),
            ("# moved declaration\n", "int", false),
            ("\n", "str", true),
        ] {
            let source_text = format!("{prefix}def _init(value):\n    return {{'value': value}}\n_Original, raw = provider(fields=['value'], init=_init)\nInfo = _Original\n");
            let interface_text = format!("{prefix}class Info:\n    \"\"\"Provider documentation.\"\"\"\n    value: Final[{annotation}]\n    def __init__(self, value: {annotation}) -> None: ...\ndef raw(*, value: {annotation}) -> Info: ...\n");
            analysis.update_file(source, source_text.clone());
            analysis.update_file(interface, interface_text.clone());
            let snapshot = analysis.snapshot();
            let diagnostics = snapshot.diagnostics(caller).unwrap();
            assert_eq!(diagnostics.is_empty(), valid, "{diagnostics:?}");
            let hover = snapshot
                .hover(FilePosition {
                    file_id: caller,
                    pos: (caller_text.rfind(".value").unwrap() as u32 + 1).into(),
                })
                .unwrap()
                .unwrap();
            assert!(
                hover.contents.value.contains(annotation),
                "{}",
                hover.contents.value
            );
            let provider_hover = snapshot
                .hover(FilePosition {
                    file_id: caller,
                    pos: (caller_text.rfind("Info").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            assert!(
                provider_hover
                    .contents
                    .value
                    .contains("Provider documentation."),
                "{}",
                provider_hover.contents.value
            );
            for (needle, target, expected) in
                [(".value", interface, "value"), ("Info", source, "Info")]
            {
                let offset =
                    caller_text.rfind(needle).unwrap() + usize::from(needle.starts_with('.'));
                let locations = snapshot
                    .goto_definition(
                        FilePosition {
                            file_id: caller,
                            pos: (offset as u32).into(),
                        },
                        false,
                    )
                    .unwrap()
                    .unwrap();
                let [LocationLink::Local {
                    target_file_id,
                    target_selection_range,
                    origin_selection_range: _,
                    target_range: _,
                }] = locations.as_slice()
                else {
                    panic!("{locations:?}");
                };
                assert_eq!(*target_file_id, target.source);
                let text = if target == source {
                    &source_text
                } else {
                    &interface_text
                };
                assert_eq!(&text[*target_selection_range], expected);
            }
            let help = snapshot
                .signature_help(FilePosition {
                    file_id: caller,
                    pos: (caller_text.find("'ok'").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            assert!(
                help.signatures
                    .iter()
                    .any(|signature| signature.label.contains(&format!("value: {annotation}"))),
                "{help:?}"
            );
            let completions = snapshot
                .completions(
                    FilePosition {
                        file_id: caller,
                        pos: (caller_text.rfind(".value").unwrap() as u32 + 1).into(),
                    },
                    None,
                )
                .unwrap()
                .unwrap();
            assert!(
                completions.iter().any(|item| item.label == "value"),
                "{completions:?}"
            );
        }
        analysis.update_file(
            interface,
            "class Info:\n    value: Fi\n    def __init__(self, *, value: str) -> None: ...\n"
                .into(),
        );
        let completions = analysis
            .snapshot()
            .completions(
                FilePosition {
                    file_id: interface,
                    pos: 25.into(),
                },
                None,
            )
            .unwrap()
            .unwrap();
        assert!(
            completions.iter().any(|item| item.label == "Final"),
            "{completions:?}"
        );
    }

    #[test]
    fn provider_pairing_rejects_distinct_contracts_for_one_key() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "Info = provider(fields=['value'])\nAlias = Info\n",
        );
        let reexport = fixture.add_file(
            &mut analysis.db,
            "reexport.bzl",
            "load('source.bzl', _Info='Info')\nInfo = _Info\n",
        );
        let class = "class Info:\n    value: Final[str]\n    def __init__(self, *, value: str) -> None: ...\n";
        let interface = fixture.add_file(&mut analysis.db, "source.bzli", class);
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", "load('source.bzl', 'Info')\nload('reexport.bzl', Other='Info')\ndef consume(value: Info) -> str:\n    return value.value\ntext = consume(Other(value='ok'))\n");
        loader.add_files_from_fixture(&fixture);
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
            .set_type_interfaces([(source, interface), (reexport, interface)])
            .unwrap();
        for file in [interface, caller] {
            assert!(analysis.snapshot().diagnostics(file).unwrap().is_empty());
        }
        analysis.update_file(
            interface,
            format!("{class}{}", class.replace("Info", "Alias")),
        );
        let diagnostics = analysis.snapshot().diagnostics(interface).unwrap();
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.id().as_str() == "invalid-provider-interface")
                .count(),
            2,
            "{diagnostics:?}"
        );
        let reports = analysis
            .validate_stubs(|path| path.file_name().unwrap() == "source.bzl")
            .unwrap();
        assert_eq!(
            reports
                .iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .filter(|diagnostic| diagnostic.id().as_str() == "invalid-provider-interface")
                .count(),
            2,
            "{reports:?}"
        );
        analysis.update_file(interface, class.into());
        for text in [
            "Info = provider(fields=['value'])\n",
            "# shifted\nInfo = provider(fields=['value'])\n",
        ] {
            analysis.update_file(source, text.into());
            assert!(analysis.snapshot().diagnostics(caller).unwrap().is_empty());
        }
    }

    #[test]
    fn trusted_exports_are_partial_and_independent_of_the_implementation() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source_text = "def compute(legacy: int) -> int:\n    return 'bad'\nuntouched = 42\n";
        let source = fixture.add_file(&mut analysis.db, "source.bzl", source_text);
        let interface = fixture.add_file(&mut analysis.db, "source.bzli", "");
        let caller_text = "load(\"source.bzl\", \"compute\", \"untouched\", \"only\")\nresult = compute(value='ok')\noriginal = untouched\n";
        let caller = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD",
            caller_text,
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, interface)]).unwrap();
        for (annotation, valid) in [("string", true), ("int", false), ("string", true)] {
            analysis.update_file(
                interface,
                format!("def compute(value: {annotation}) -> {annotation}: ...\nonly: int\n"),
            );
            let snapshot = analysis.snapshot();
            let completions = snapshot
                .completions(
                    FilePosition {
                        file_id: caller,
                        pos: (caller_text.find("only").unwrap() as u32).into(),
                    },
                    None,
                )
                .unwrap()
                .unwrap();
            for name in ["compute", "only", "untouched"] {
                assert_eq!(
                    completions.iter().filter(|item| item.label == name).count(),
                    1,
                    "{name}"
                );
            }
            let diagnostics = snapshot.diagnostics(caller).unwrap();
            assert_eq!(diagnostics.is_empty(), valid, "{diagnostics:?}");
            if !valid {
                let [diagnostic] = diagnostics.as_slice() else {
                    panic!("{diagnostics:?}")
                };
                assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
            }
            let diagnostics = snapshot.diagnostics(source).unwrap();
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.id().as_str() == "invalid-return-type"),
                "{diagnostics:?}"
            );
            let hover = snapshot
                .hover(FilePosition {
                    file_id: caller,
                    pos: (caller_text.rfind("untouched").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            assert!(
                hover.contents.value.contains("Literal[42]"),
                "{}",
                hover.contents.value
            );
            let position = FilePosition {
                file_id: caller,
                pos: (caller_text.rfind("compute").unwrap() as u32).into(),
            };
            let locations = snapshot.goto_definition(position, false).unwrap().unwrap();
            let [LocationLink::Local {
                target_file_id,
                origin_selection_range: _,
                target_range: _,
                target_selection_range: _,
            }] = locations.as_slice()
            else {
                panic!("{locations:?}")
            };
            assert_eq!(*target_file_id, source.source);
            let position = FilePosition {
                file_id: caller,
                pos: (caller_text.find("only").unwrap() as u32).into(),
            };
            let locations = snapshot.goto_definition(position, false).unwrap().unwrap();
            let [LocationLink::Local {
                target_file_id,
                origin_selection_range: _,
                target_range: _,
                target_selection_range: _,
            }] = locations.as_slice()
            else {
                panic!("{locations:?}")
            };
            assert_eq!(*target_file_id, interface.source);
            let help = snapshot
                .signature_help(FilePosition {
                    file_id: caller,
                    pos: (caller_text.find("'ok'").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            let [signature] = help.signatures.as_slice() else {
                panic!("{help:?}")
            };
            let expected = if valid { "str" } else { "int" };
            assert_eq!(
                signature.label,
                format!("def compute(value: {expected}) -> {expected}")
            );
        }
        analysis.update_file(source, "untouched = 42\nmalformed(\n".to_owned());
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(caller).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(!snapshot.diagnostics(source).unwrap().is_empty());
        let locations = snapshot
            .goto_definition(
                FilePosition {
                    file_id: caller,
                    pos: (caller_text.rfind("compute").unwrap() as u32).into(),
                },
                false,
            )
            .unwrap()
            .unwrap();
        let [LocationLink::Local {
            target_file_id,
            origin_selection_range: _,
            target_range: _,
            target_selection_range: _,
        }] = locations.as_slice()
        else {
            panic!("{locations:?}")
        };
        assert_eq!(*target_file_id, interface.source);
    }

    #[test]
    fn navigation_retains_the_loaded_source_and_reexport_policy() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let first = fixture.add_file(&mut analysis.db, "first.bzl", "def compute(): pass\n");
        let origin = fixture.add_file(&mut analysis.db, "origin.bzl", "def original(): pass\n");
        let second = fixture.add_file(
            &mut analysis.db,
            "second.bzl",
            "load(\"origin.bzl\", \"original\")\ncompute = original\n",
        );
        let interface = fixture.add_file(
            &mut analysis.db,
            "shared.bzli",
            "def compute(value: int) -> int: ...\n",
        );
        let text = "load(\"second.bzl\", \"compute\")\ncompute(1)\n";
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", text);
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_type_interfaces([(first, interface), (second, interface)])
            .unwrap();
        let snapshot = analysis.snapshot();
        for (skip, expected) in [(false, second), (true, origin)] {
            let locations = snapshot
                .goto_definition(
                    FilePosition {
                        file_id: caller,
                        pos: (text.rfind("compute").unwrap() as u32).into(),
                    },
                    skip,
                )
                .unwrap()
                .unwrap();
            let [LocationLink::Local {
                target_file_id,
                origin_selection_range: _,
                target_range: _,
                target_selection_range: _,
            }] = locations.as_slice()
            else {
                panic!("{locations:?}")
            };
            assert_eq!(*target_file_id, expected.source);
        }
    }

    #[test]
    fn interface_imports_preserve_original_nominal_provider_identity() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "Info = provider(fields=[])\ndef consume(): pass\n",
        );
        let interface = fixture.add_file(
            &mut analysis.db,
            "source.bzli",
            "load(\"source.bzl\", Original=\"Info\")\ndef Info() -> Original: ...\ndef consume(value: Original) -> Original: ...\n",
        );
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", "load(\"source.bzl\", \"Info\", \"consume\")\nOther = provider(fields=[])\nresult = consume(Info())\nconsume(Other())\n");
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        analysis.set_type_interfaces([(source, interface)]).unwrap();
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(interface).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = snapshot.diagnostics(caller).unwrap();
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("{diagnostics:?}")
        };
        assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
    }
}
