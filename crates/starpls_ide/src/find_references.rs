use anyhow::bail;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use ruff_text_size::TextSize;
use starpls_common::parsed_module;
use starpls_common::File;
use starpls_hir::Db as _;
use ty_ide::references_in_file;
use ty_ide::ReferenceTarget;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::global_scope;
use ty_python_core::place_table;
use ty_python_core::semantic_index;
use ty_python_semantic::types::ide_support::definitions_for_keyword_argument;
use ty_python_semantic::types::ide_support::definitions_for_name;
use ty_python_semantic::types::ide_support::resolve_definition;
use ty_python_semantic::HasDefinition;
use ty_python_semantic::ImportAliasResolution;
use ty_python_semantic::ProgramEnvironment;
use ty_python_semantic::ResolvedDefinition;
use ty_python_semantic::SemanticModel;

use crate::selection::Selection;
use crate::util::navigation_token;
use crate::util::text_range;
use crate::Database;
use crate::FilePosition;
use crate::Location;

struct Search<'db> {
    name: String,
    range: TextRange,
    definitions: Vec<ResolvedDefinition<'db>>,
    local_alias: bool,
}

/// A rename is complete before the server checks ownership and converts edits.
#[derive(Debug)]
pub struct Rename {
    pub range: starpls_syntax::TextRange,
    pub locations: Vec<Location>,
}

fn resolve_load<'db>(
    db: &'db Database,
    definition: Definition<'db>,
) -> Vec<ResolvedDefinition<'db>> {
    resolve_definition(
        db,
        &ProgramEnvironment::from_file(definition.program_file(db)),
        definition,
        None,
        ImportAliasResolution::ResolveAliases,
    )
    .into_iter()
    .filter(|resolved| resolved.definition() != Some(definition))
    .collect()
}

fn select(db: &Database, FilePosition { file_id: file, pos }: FilePosition) -> Option<Search<'_>> {
    let program = db.starlark_program_file(file);
    let model = SemanticModel::new(db, program);
    let parsed = parsed_module(db, file).load(db);
    let source = file.contents(db);
    let offset = u32::from(pos).into();
    // A load has two independently named sides: the exported string and its local alias.
    for load in crate::ty::load::bindings(db, file) {
        let local =
            load.spelling.explicit && load.spelling.binding.range.contains_inclusive(offset);
        if local || load.spelling.remote_range.contains_inclusive(offset) {
            return Some(Search {
                name: if local {
                    load.spelling.binding.name.to_string()
                } else {
                    load.spelling.remote.into()
                },
                range: if local {
                    load.spelling.binding.range
                } else {
                    load.spelling.remote_range
                },
                definitions: if local {
                    vec![ResolvedDefinition::Definition(load.definition)]
                } else {
                    resolve_load(db, load.definition)
                },
                local_alias: local,
            });
        }
    }
    if let Some(owner) = starpls_hir::Source::new(db).type_comment_owner(file, offset) {
        let (annotation, model) = model.enter_provided_annotation(owner)?;
        return select_annotation(db, &model, &annotation, &source, offset);
    }
    let token = navigation_token(&source, parsed.tokens(), offset)?;
    let node = covering_node(parsed.syntax().into(), token.range());
    let (name, range, definitions) = match crate::selection::classify(&node, token.range())? {
        Selection::Reference(name) => {
            model.scope(name.into())?;
            (
                name.id.as_str(),
                name.range(),
                definitions_for_name(
                    &model,
                    name.id.as_str(),
                    name.into(),
                    ImportAliasResolution::PreserveAliases,
                ),
            )
        }
        Selection::Definition(function) => {
            model.scope(function.into())?;
            (
                function.name.as_str(),
                function.name.range(),
                vec![ResolvedDefinition::Definition(function.definition(&model))],
            )
        }
        Selection::Parameter(parameter) => {
            model.scope(parameter.into())?;
            (
                parameter.name.as_str(),
                parameter.name.range(),
                vec![ResolvedDefinition::Definition(parameter.definition(&model))],
            )
        }
        Selection::Keyword { keyword, call } => {
            model.scope(call.into())?;
            let name = keyword.arg.as_ref()?;
            (
                name.as_str(),
                name.range(),
                definitions_for_keyword_argument(&model, keyword, call),
            )
        }
        Selection::String(string) => {
            let (annotation, model) = model.enter_string_annotation(string)?;
            return select_annotation(db, &model, &annotation, &source, offset);
        }
        _ => return None,
    };
    make_search(db, name, range, definitions)
}

fn make_search<'db>(
    db: &'db Database,
    name: &str,
    range: TextRange,
    definitions: Vec<ResolvedDefinition<'db>>,
) -> Option<Search<'db>> {
    let definitions: Vec<_> = definitions
        .into_iter()
        .filter(|resolved| {
            resolved.definition().is_some_and(|definition| {
                db.starlark_file(definition.program_file(db)).is_some()
                    && matches!(
                        definition.kind(db),
                        DefinitionKind::Function(_)
                            | DefinitionKind::Parameter(_)
                            | DefinitionKind::Assignment(_)
                            | DefinitionKind::AnnotatedAssignment(_)
                            | DefinitionKind::AugmentedAssignment(_)
                            | DefinitionKind::For(_)
                            | DefinitionKind::Comprehension(_)
                            | DefinitionKind::ProvidedBinding(_)
                    )
            })
        })
        .collect();
    if definitions.is_empty() {
        return None;
    }
    let local_alias = definitions.iter().any(|definition| {
        definition.definition().is_some_and(|definition| {
            let file = db
                .starlark_file(definition.program_file(db))
                .expect("admitted Starlark definition");
            crate::ty::load::bindings(db, file)
                .iter()
                .any(|load| load.definition == definition && load.spelling.explicit)
        })
    });
    Some(Search {
        name: name.to_owned(),
        range,
        definitions,
        local_alias,
    })
}

fn select_annotation<'db>(
    db: &'db Database,
    model: &SemanticModel<'db>,
    parsed: &ruff_python_parser::Parsed<ruff_python_ast::ModExpression>,
    source: &str,
    offset: TextSize,
) -> Option<Search<'db>> {
    let token = navigation_token(source, parsed.tokens(), offset)?;
    let node = covering_node(parsed.syntax().into(), token.range());
    match crate::selection::classify(&node, token.range())? {
        Selection::Reference(name) => make_search(
            db,
            name.id.as_str(),
            name.range(),
            definitions_for_name(
                model,
                name.id.as_str(),
                name.into(),
                ImportAliasResolution::PreserveAliases,
            ),
        ),
        Selection::String(string) => {
            let (annotation, model) = model.enter_string_annotation(string)?;
            select_annotation(db, &model, &annotation, source, offset)
        }
        _ => None,
    }
}

pub(crate) fn reference_name(db: &Database, pos: FilePosition) -> Option<String> {
    let mut search = select(db, pos)?;
    follow_load(db, &mut search);
    Some(search.name)
}

fn follow_load<'db>(db: &'db Database, search: &mut Search<'db>) {
    // Resolve the selected load once. Reexport assignments preserve their own identities.
    let [ResolvedDefinition::Definition(definition)] = search.definitions.as_slice() else {
        return;
    };
    let Some((_, _, name)) = crate::ty::load::binding_names(db, *definition) else {
        return;
    };
    search.name = name.into();
    search.definitions = resolve_load(db, *definition);
    search.local_alias = false;
}

pub(crate) fn find_references(
    db: &Database,
    pos: FilePosition,
    include_declaration: bool,
) -> Option<Vec<ReferenceTarget>> {
    let file = pos.file_id;
    let search = select(db, pos)?;
    Some(references_in_file(
        db,
        db.starlark_program_file(file),
        &search.name,
        &search.definitions,
        include_declaration,
    ))
}

pub(crate) fn workspace_references(
    db: &Database,
    pos: FilePosition,
    candidates: &[File],
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let mut search = select(db, pos)?;
    follow_load(db, &mut search);
    // Ambiguous contracts still have useful references; rename requires complete correspondence.
    let _ = pair_definitions(db, &mut search);
    occurrences(db, &search, candidates, include_declaration, false).ok()
}

fn same_family(db: &Database, left: Definition<'_>, right: Definition<'_>) -> bool {
    left.scope(db) == right.scope(db) && left.place(db) == right.place(db)
}

fn intersects(
    db: &Database,
    targets: &[ResolvedDefinition<'_>],
    definitions: &[ResolvedDefinition<'_>],
) -> bool {
    targets
        .iter()
        .filter_map(ResolvedDefinition::definition)
        .any(|left| {
            definitions
                .iter()
                .filter_map(ResolvedDefinition::definition)
                .any(|right| same_family(db, left, right))
        })
}

fn occurrences(
    db: &Database,
    search: &Search<'_>,
    candidates: &[File],
    include_declaration: bool,
    rename: bool,
) -> anyhow::Result<Vec<Location>> {
    let mut files = candidates.to_vec();
    files.extend(
        search
            .definitions
            .iter()
            .filter_map(ResolvedDefinition::definition)
            .filter_map(|definition| db.starlark_file(definition.program_file(db))),
    );
    files.sort_by(|left, right| left.path(db).cmp(right.path(db)));
    files.dedup_by_key(|file| file.source);
    let mut locations = Vec::new();
    for file in files {
        if !file.contents(db).contains(&search.name) && !file.contents(db).contains('\\') {
            continue;
        }
        locations.extend(
            references_in_file(
                db,
                db.starlark_program_file(file),
                &search.name,
                &search.definitions,
                include_declaration,
            )
            .into_iter()
            .map(|reference| {
                debug_assert_eq!(reference.file(), file.source);
                Location {
                    file_id: file,
                    range: text_range(reference.range()),
                }
            }),
        );
        if rename && !search.local_alias {
            let parsed = parsed_module(db, file).load(db);
            let index = semantic_index(db, db.starlark_program_file(file));
            for statement in parsed.suite() {
                let Stmt::Expr(statement_node) = statement else {
                    continue;
                };
                let Expr::Call(call) = statement_node.value.as_ref() else {
                    continue;
                };
                let Expr::Name(function) = call.func.as_ref() else {
                    continue;
                };
                if function.id == "load"
                    && index.provided_statement_definitions(statement).is_none()
                {
                    let text = file.contents(db);
                    let text = &text[call.range()];
                    if text.contains(&search.name) || text.contains('\\') {
                        bail!(
                            "Cannot resolve a malformed load in {}",
                            file.path(db).display()
                        );
                    }
                }
            }
        }
        for load in crate::ty::load::bindings(db, file) {
            let local_match = search
                .definitions
                .iter()
                .any(|resolved| resolved.definition() == Some(load.definition));
            let remote_match =
                if !search.local_alias && load.spelling.remote.as_ref() == search.name {
                    let resolved = resolve_load(db, load.definition);
                    if rename && resolved.is_empty() {
                        bail!(
                            "Cannot resolve `{}` in {}",
                            load.spelling.remote,
                            file.path(db).display()
                        );
                    }
                    intersects(db, &search.definitions, &resolved)
                } else {
                    false
                };
            if !local_match && !remote_match {
                continue;
            }
            if rename && !load.spelling.plain {
                bail!(
                    "Cannot rename an escaped load spelling in {}",
                    file.path(db).display()
                );
            }
            if local_match {
                if include_declaration {
                    locations.push(Location {
                        file_id: file,
                        range: text_range(load.spelling.binding.range),
                    });
                }
            } else {
                locations.push(Location {
                    file_id: file,
                    range: text_range(load.spelling.remote_range),
                });
            }
            // Explicit aliases retain their local spelling when the export is renamed.
            if !rename || local_match || !load.spelling.explicit {
                locations.extend(
                    references_in_file(
                        db,
                        db.starlark_program_file(file),
                        &load.spelling.binding.name,
                        &[ResolvedDefinition::Definition(load.definition)],
                        include_declaration,
                    )
                    .into_iter()
                    .map(|reference| Location {
                        file_id: file,
                        range: text_range(reference.range()),
                    }),
                );
            }
        }
    }
    locations.sort_by(|left, right| {
        left.file_id
            .path(db)
            .cmp(right.file_id.path(db))
            .then(left.range.start().cmp(&right.range.start()))
    });
    locations.dedup();
    Ok(locations)
}

/// A configured pair shares export names, while its function parameters require syntax correspondence.
fn pair_definitions<'db>(db: &'db Database, search: &mut Search<'db>) -> anyhow::Result<()> {
    let original = search.definitions.clone();
    for definition in original.iter().filter_map(ResolvedDefinition::definition) {
        let file = definition.file(db);
        let pairs: Vec<_> = db
            .environment()
            .type_interfaces(db)
            .values()
            .filter(|(source, stub)| source.source == file || stub.source == file)
            .collect();
        if pairs.len() > 1 {
            bail!("The selected stub describes multiple implementations");
        }
        let Some((source, stub)) = pairs.first() else {
            continue;
        };
        if db
            .environment()
            .type_interfaces(db)
            .values()
            .filter(|(_, candidate)| candidate.source == stub.source)
            .count()
            > 1
        {
            bail!("The selected stub describes multiple implementations");
        }
        let other = if source.source == file {
            *stub
        } else {
            *source
        };
        let other_program = db.starlark_program_file(other);
        if definition.scope(db) == global_scope(db, definition.program_file(db)) {
            let peers = crate::ty::interface::export_definitions(db, other_program, &search.name);
            if peers.is_empty() {
                bail!(
                    "No corresponding `{}` declaration in {}",
                    search.name,
                    other.path(db).display()
                );
            }
            search
                .definitions
                .extend(peers.into_iter().map(ResolvedDefinition::Definition));
        } else if matches!(definition.kind(db), DefinitionKind::Parameter(_)) {
            let parsed = ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db);
            let ty_python_core::scope::NodeWithScopeKind::Function(function) =
                definition.scope(db).node(db)
            else {
                continue;
            };
            let function = function.node(&parsed);
            let exports = crate::ty::interface::export_definitions(
                db,
                definition.program_file(db),
                function.name.as_str(),
            );
            if !exports.contains(
                &function.definition(&SemanticModel::new(db, definition.program_file(db))),
            ) {
                continue;
            }
            let peers =
                crate::ty::interface::export_definitions(db, other_program, function.name.as_str());
            let [peer] = peers.as_slice() else {
                bail!("The paired function is ambiguous");
            };
            let DefinitionKind::Function(peer) = peer.kind(db) else {
                bail!("The paired export is not a direct function declaration");
            };
            let other_parsed = parsed_module(db, other).load(db);
            let peer = peer.node(&other_parsed);
            let pairs = crate::ty::validation::parameter_pairs(function, peer)
                .map_err(anyhow::Error::msg)?;
            let Some((parameter, peer)) = pairs
                .into_iter()
                .find(|(parameter, _)| parameter.name.as_str() == search.name)
            else {
                bail!("The paired parameter is missing");
            };
            if parameter.name != peer.name {
                bail!("The paired parameter has a different name");
            }
            search.definitions.push(ResolvedDefinition::Definition(
                peer.definition(&SemanticModel::new(db, other_program)),
            ));
        }
    }
    Ok(())
}

pub(crate) fn rename(
    db: &Database,
    pos: FilePosition,
    candidates: &[File],
    new_name: Option<&str>,
) -> anyhow::Result<Option<Rename>> {
    let Some(mut search) = select(db, pos) else {
        return Ok(None);
    };
    if let Some(name) = new_name {
        let parsed = ruff_python_parser::parse_module(name)
            .map_err(|_| anyhow::anyhow!("`{name}` is not a Starlark identifier"))?;
        let mut errors = Vec::new();
        starpls_syntax::validate(
            name,
            &parsed,
            starpls_syntax::AnnotationMode::Source,
            &mut |error| errors.push(error),
        );
        let valid = matches!(parsed.suite().as_slice(), [Stmt::Expr(statement)] if matches!(statement.value.as_ref(), Expr::Name(identifier) if identifier.id == name));
        if !valid || !errors.is_empty() {
            bail!("`{name}` is not a Starlark identifier");
        }
    }
    if !search.local_alias {
        follow_load(db, &mut search);
    }
    let Some(first) = search
        .definitions
        .first()
        .and_then(ResolvedDefinition::definition)
    else {
        return Ok(None);
    };
    if !search.definitions.iter().all(|definition| {
        definition
            .definition()
            .is_some_and(|definition| same_family(db, first, definition))
    }) {
        bail!("The selected name refers to unrelated declarations");
    }
    if search
        .definitions
        .iter()
        .filter_map(ResolvedDefinition::definition)
        .any(|definition| db.starlark_file(definition.program_file(db)).is_none())
    {
        return Ok(None);
    }
    if !search.local_alias {
        pair_definitions(db, &mut search)?;
    }
    let locations = occurrences(db, &search, candidates, true, true)?;
    if let Some(name) = new_name {
        if name != search.name {
            if name.starts_with('_')
                && locations.iter().any(|location| {
                    crate::ty::load::bindings(db, location.file_id)
                        .iter()
                        .any(|load| {
                            text_range(load.spelling.remote_range) == location.range
                                && !search.local_alias
                        })
                })
            {
                bail!("A symbol loaded by another file must remain public");
            }
            for definition in search
                .definitions
                .iter()
                .filter_map(ResolvedDefinition::definition)
            {
                check_binding_collision(db, definition, name)?;
            }
            for location in &locations {
                check_collision(db, location, name)?;
            }
        }
    }
    Ok(Some(Rename {
        range: text_range(search.range),
        locations,
    }))
}

fn check_binding_collision(
    db: &Database,
    definition: Definition<'_>,
    name: &str,
) -> anyhow::Result<()> {
    if let Some(file) = db.starlark_file(definition.program_file(db)) {
        let source = file.contents(db);
        let contains_name = |text: &str| {
            text.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .any(|word| word == name)
        };
        if starpls_common::syntax_info(db, file)
            .iter()
            .any(|comment| contains_name(&source[comment.range]))
        {
            bail!(
                "`{name}` already appears in a type comment in {}",
                file.path(db).display()
            );
        }
        let parsed = parsed_module(db, file).load(db);
        let model = SemanticModel::new(db, definition.program_file(db));
        for token in parsed
            .tokens()
            .iter()
            .filter(|token| token.kind() == ruff_python_ast::token::TokenKind::String)
        {
            if !contains_name(&source[token.range()]) {
                continue;
            }
            let node = covering_node(parsed.syntax().into(), token.range());
            let Some(Selection::String(string)) = crate::selection::classify(&node, token.range())
            else {
                continue;
            };
            if model.enter_string_annotation(string).is_some() {
                bail!(
                    "`{name}` already appears in a quoted annotation in {}",
                    file.path(db).display()
                );
            }
        }
    }
    let index = semantic_index(db, definition.program_file(db));
    let owner = definition.file_scope(db);
    // Shared scope facts also catch capture of an existing destination-name use
    // that is not one of the occurrences being renamed.
    for scope in index.scope_ids() {
        if index
            .ancestor_scopes(scope.file_scope_id(db))
            .any(|(candidate, _)| candidate == owner)
        {
            let table = place_table(db, scope);
            if table.symbol_id(name).is_some_and(|symbol| {
                let symbol = table.symbol(symbol);
                symbol.is_local() || symbol.is_used()
            }) {
                bail!("`{name}` is already used in an affected scope");
            }
        }
    }
    Ok(())
}

fn check_collision(db: &Database, location: &Location, name: &str) -> anyhow::Result<()> {
    let file = location.file_id;
    let parsed = parsed_module(db, file).load(db);
    let range = TextRange::new(
        u32::from(location.range.start()).into(),
        u32::from(location.range.end()).into(),
    );
    let node = covering_node(parsed.syntax().into(), range);
    for load in crate::ty::load::bindings(db, file) {
        if load.spelling.explicit && load.spelling.remote_range == range {
            return Ok(());
        }
        if load.spelling.binding.range == range {
            return check_binding_collision(db, load.definition, name);
        }
    }
    if let Some(Selection::Keyword { keyword: _, call }) = crate::selection::classify(&node, range)
    {
        if call.arguments.keywords.iter().any(|keyword| {
            keyword
                .arg
                .as_ref()
                .is_some_and(|argument| argument.as_str() == name)
        }) {
            bail!(
                "The call already supplies `{name}` in {}",
                file.path(db).display()
            );
        }
        return Ok(());
    }
    let model = SemanticModel::new(db, db.starlark_program_file(file));
    for (scope, _) in model.ancestor_scopes(node.node()) {
        let scope = scope.to_scope_id(db, model.program_file());
        let table = place_table(db, scope);
        if table
            .symbol_id(name)
            .is_some_and(|symbol| table.symbol(symbol).is_local())
        {
            bail!(
                "`{name}` is already bound in an affected scope in {}",
                file.path(db).display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::Analysis;
    use crate::FilePosition;
    use crate::ReferenceKind;

    fn renamed(
        snapshot: &crate::AnalysisSnapshot,
        rename: &super::Rename,
        file: starpls_common::File,
        name: &str,
    ) -> String {
        let mut contents = snapshot.source(file).unwrap().text.to_string();
        for location in rename
            .locations
            .iter()
            .rev()
            .filter(|location| location.file_id == file)
        {
            contents.replace_range(
                usize::from(location.range.start())..usize::from(location.range.end()),
                name,
            );
        }
        contents
    }

    #[test]
    fn workspace_exports_preserve_aliases_and_reexports() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "defs.bzl",
            "def original(value): return value\n",
        );
        let plain = fixture.add_file(
            &mut analysis.db,
            "BUILD",
            "load(\"defs.bzl\", \"original\")\noriginal(value=1)\n",
        );
        let aliased = fixture.add_file(
            &mut analysis.db,
            "alias.bzl",
            "load(\"defs.bzl\", local=\"original\")\npublished = local\n",
        );
        let consumer = fixture.add_file(
            &mut analysis.db,
            "consumer.bzl",
            "load(\"alias.bzl\", \"published\")\npublished(2)\n",
        );
        loader.add_files_from_fixture(&fixture);
        let snapshot = analysis.snapshot();
        let files = [source, plain, aliased, consumer];
        let position = FilePosition {
            file_id: source,
            pos: 4.into(),
        };
        let references = snapshot
            .workspace_references(position.clone(), &files, false)
            .unwrap()
            .unwrap();
        assert!(references.iter().any(|location| location.file_id == plain));
        assert!(references
            .iter()
            .any(|location| location.file_id == aliased));
        assert!(!references
            .iter()
            .any(|location| location.file_id == consumer));
        let rename = snapshot
            .rename(position, &files, Some("renamed"))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            renamed(&snapshot, &rename, source, "renamed"),
            "def renamed(value): return value\n"
        );
        assert_eq!(
            renamed(&snapshot, &rename, plain, "renamed"),
            "load(\"defs.bzl\", \"renamed\")\nrenamed(value=1)\n"
        );
        assert_eq!(
            renamed(&snapshot, &rename, aliased, "renamed"),
            "load(\"defs.bzl\", local=\"renamed\")\npublished = local\n"
        );
        let alias_position = FilePosition {
            file_id: aliased,
            pos: 17.into(),
        };
        let rename = snapshot
            .rename(alias_position, &files, Some("private_alias"))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            renamed(&snapshot, &rename, aliased, "private_alias"),
            "load(\"defs.bzl\", private_alias=\"original\")\npublished = private_alias\n"
        );
        assert!(rename
            .locations
            .iter()
            .all(|location| location.file_id == aliased));
        let implicit = FilePosition {
            file_id: plain,
            pos: 30.into(),
        };
        let rename = snapshot
            .rename(implicit, &files, Some("renamed"))
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(rename
            .locations
            .iter()
            .any(|location| location.file_id == source));
    }

    #[test]
    fn paired_parameter_rename_updates_source_stub_and_callers() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "defs.bzl",
            "def echo(value): return value\n",
        );
        let stub = fixture.add_file(
            &mut analysis.db,
            "defs.bzli",
            "def echo(value: int) -> int: ...\n",
        );
        let caller = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "load(\"defs.bzl\", local=\"echo\")\nlocal(value=1)\n",
        );
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        let files = [source, stub, caller];
        let snapshot = analysis.snapshot();
        for file in [source, stub, caller] {
            let text = snapshot.source(file).unwrap().text;
            let pos = (text.find("value").unwrap() as u32).into();
            let rename = snapshot
                .rename(FilePosition { file_id: file, pos }, &files, Some("item"))
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                renamed(&snapshot, &rename, source, "item"),
                "def echo(item): return item\n"
            );
            assert_eq!(
                renamed(&snapshot, &rename, stub, "item"),
                "def echo(item: int) -> int: ...\n"
            );
            assert_eq!(
                renamed(&snapshot, &rename, caller, "item"),
                "load(\"defs.bzl\", local=\"echo\")\nlocal(item=1)\n"
            );
        }
        drop(snapshot);
        analysis.update_file(stub, "def echo(other: int) -> int: ...\n".into());
        assert!(analysis
            .snapshot()
            .rename(
                FilePosition {
                    file_id: source,
                    pos: 9.into()
                },
                &files,
                Some("item")
            )
            .unwrap()
            .is_err());
    }

    #[test]
    fn rename_rejects_unsafe_names_and_load_spellings() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "defs.bzl",
            "def echo(value): return value\nother = 1\n",
        );
        let caller = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "load(\"defs.bzl\", \"echo\")\necho(1)\n",
        );
        loader.add_files_from_fixture(&fixture);
        let files = [source, caller];
        let position = FilePosition {
            file_id: source,
            pos: 4.into(),
        };
        for name in ["for", "load", "écho", "x y", "_private", "other"] {
            assert!(
                analysis
                    .snapshot()
                    .rename(position.clone(), &files, Some(name))
                    .unwrap()
                    .is_err(),
                "{name}"
            );
        }
        analysis.update_file(
            caller,
            r#"load("defs.bzl", alias="e\x63ho")
alias(1)
"#
            .into(),
        );
        assert!(analysis
            .snapshot()
            .rename(position, &files, Some("renamed"))
            .unwrap()
            .is_err());
    }

    #[test]
    fn rename_enters_type_comments_with_their_own_scope() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let text = r#"P = provider()
def function(value): # type: (P) -> P
    P = provider()
def parameter(value, # type: P
): pass
def assignment():
    value = None # type: P
def quoted(value: "'P'"): pass
def native(value: int): # type: (P) -> None
    pass
def shadow():
    P = provider()
    value = None # type: P
"#;
        let file = fixture.add_file(&mut analysis.db, "defs.bzl", text);
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                starpls_bazel::Builtins::default(),
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let rename = snapshot
            .rename(
                FilePosition {
                    file_id: file,
                    pos: 0.into(),
                },
                &[file],
                Some("Record"),
            )
            .unwrap()
            .unwrap()
            .unwrap();
        let expected = r#"Record = provider()
def function(value): # type: (Record) -> Record
    P = provider()
def parameter(value, # type: Record
): pass
def assignment():
    value = None # type: Record
def quoted(value: "'Record'"): pass
def native(value: int): # type: (P) -> None
    pass
def shadow():
    P = provider()
    value = None # type: P
"#;
        assert_eq!(renamed(&snapshot, &rename, file, "Record"), expected);
    }

    #[test]
    fn annotation_selection_preserves_local_aliases() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let source = fixture.add_file(&mut analysis.db, "defs.bzl", "P = provider()\n");
        let text = "load(\"defs.bzl\", Local=\"P\")\ndef f(value): # type: (Local) -> int\n    pass\ndef g(value: \"Local\"): pass\n";
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", text);
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                starpls_bazel::Builtins::default(),
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        for (pos, _) in text.match_indices("Local") {
            let rename = snapshot
                .rename(
                    FilePosition {
                        file_id: caller,
                        pos: (pos as u32).into(),
                    },
                    &[source, caller],
                    Some("Record"),
                )
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(rename
                .locations
                .iter()
                .all(|location| location.file_id == caller));
            assert_eq!(
                renamed(&snapshot, &rename, caller, "Record"),
                text.replace("Local", "Record")
            );
        }
        let pos = (text.find("int").unwrap() as u32).into();
        assert!(snapshot
            .rename(
                FilePosition {
                    file_id: caller,
                    pos
                },
                &[source, caller],
                Some("Integer")
            )
            .unwrap()
            .unwrap()
            .is_none());
    }

    #[test]
    fn rename_rejects_destination_capture_and_shared_stub_owners() {
        for text in [
            "Old = 1\ndef f(): return New\nOld\n",
            "def f():\n    Old = provider()\n    value = None # type: int\n",
            "def f():\n    Old = provider()\n    value: \"int\" = None\n",
        ] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(text);
            let file = fixture.main_file();
            let name = if text.contains("New") { "New" } else { "int" };
            let pos = (text.find("Old").unwrap() as u32).into();
            assert!(analysis
                .snapshot()
                .rename(FilePosition { file_id: file, pos }, &[file], Some(name))
                .unwrap()
                .is_err());
        }
        let text = "Old = 1\ntext = \"int\"\nOld\n";
        let (analysis, fixture) = Analysis::from_single_file_fixture(text);
        let file = fixture.main_file();
        let snapshot = analysis.snapshot();
        let rename = snapshot
            .rename(
                FilePosition {
                    file_id: file,
                    pos: 0.into(),
                },
                &[file],
                Some("int"),
            )
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            renamed(&snapshot, &rename, file, "int"),
            "int = 1\ntext = \"int\"\nint\n"
        );
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let source = fixture.add_file(&mut analysis.db, "first.bzl", "def f(): pass\n");
        let second = fixture.add_file(&mut analysis.db, "second.bzl", "def f(): pass\n");
        let stub = fixture.add_file(&mut analysis.db, "shared.bzli", "def f() -> None: ...\n");
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_type_interfaces([(source, stub), (second, stub)])
            .unwrap();
        for file in [source, second, stub] {
            assert!(analysis
                .snapshot()
                .rename(
                    FilePosition {
                        file_id: file,
                        pos: 4.into()
                    },
                    &[source, second, stub],
                    Some("renamed")
                )
                .unwrap()
                .is_err());
        }
    }

    #[test]
    fn parameter_and_keyword_references_preserve_read_write_kinds() {
        let source = "def echo(value):\n    value = 2\n    def shadow(value):\n        return value\n    return value\necho(value=1)\n";
        let (analysis, fixture) = Analysis::from_single_file_fixture(source);
        let file = fixture.main_file();
        let positions = source
            .match_indices("value")
            .map(|(offset, _)| offset as u32)
            .collect::<Vec<_>>();
        let expected = vec![
            (positions[0], ReferenceKind::Other),
            (positions[1], ReferenceKind::Write),
            (positions[4], ReferenceKind::Read),
            (positions[5], ReferenceKind::Read),
        ];
        for pos in [positions[0], positions[4], positions[5]] {
            let snapshot = analysis.snapshot();
            let highlights = snapshot
                .document_highlights(FilePosition {
                    file_id: file,
                    pos: pos.into(),
                })
                .unwrap()
                .unwrap();
            let actual: Vec<_> = highlights
                .iter()
                .map(|reference| (u32::from(reference.range().start()), reference.kind()))
                .collect();
            assert_eq!(actual, expected);
            let references = snapshot
                .find_references(
                    FilePosition {
                        file_id: file,
                        pos: pos.into(),
                    },
                    false,
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                references
                    .into_iter()
                    .map(|reference| u32::from(reference.range.start()))
                    .collect::<Vec<_>>(),
                [positions[1], positions[4], positions[5]]
            );
        }
    }

    fn check_find_references(fixture: &str) {
        let (analysis, fixture) = Analysis::from_single_file_fixture(fixture);
        check_find_references_from_fixture(analysis, fixture);
    }

    fn check_find_references_from_fixture(analysis: Analysis, fixture: starpls_hir::Fixture) {
        let references = analysis
            .snapshot()
            .find_references(
                fixture
                    .cursor_pos
                    .map(|(file_id, pos)| FilePosition { file_id, pos })
                    .unwrap(),
                true,
            )
            .unwrap()
            .unwrap();

        let mut actual_locations = references
            .into_iter()
            .map(|location| (location.file_id, location.range))
            .collect::<Vec<_>>();
        actual_locations.sort_by_key(|(_, range)| range.start());
        actual_locations.sort_by_key(|(file, _)| file.path(&analysis.db));

        assert_eq!(fixture.selected_ranges, actual_locations);
    }

    #[test]
    fn native_identity_follows_edits() {
        let original = "value = 1\nvalue\n";
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(original);
        let file = fixture.main_file();
        for prefix in ["", "unrelated = [1, 2, 3]\n", ""] {
            let source = format!("{prefix}{original}");
            analysis.update_file(file, source.clone());
            let locations = analysis
                .snapshot()
                .find_references(
                    FilePosition {
                        file_id: file,
                        pos: u32::try_from(source.rfind("value").unwrap())
                            .unwrap()
                            .into(),
                    },
                    true,
                )
                .unwrap()
                .unwrap();
            let starts = locations
                .into_iter()
                .map(|location| u32::from(location.range.start()))
                .collect::<Vec<_>>();
            let base = u32::try_from(prefix.len()).unwrap();
            assert_eq!(starts, [base, base + 10]);
        }
    }

    #[test]
    fn ignores_nonreference_occurrences() {
        check_find_references(
            r#"
value = 1
#^^^^
value_suffix = "value"
obj.value
# value

def f(value):
    return value

val$0ue
#^^^^
"#,
        );
    }

    #[test]
    fn distinguishes_closure_bindings_from_shadowed_locals() {
        check_find_references(
            r#"
value = 1
#^^^^
def outer():
    value = 2
    def inner():
        return value
    return value

def read_global():
    return val$0ue
           #^^^^
value = 3
#^^^^
"#,
        );
    }

    #[test]
    fn selects_name_after_dedent_and_at_eof() {
        check_find_references("def f():\n    pass\nvalue = 1\n#^^^^\n$0value\n#^^^^");
        check_find_references("value = 1\n#^^^^\nvalue$0\n#^^^^");
    }

    #[test]
    fn local_reexports_preserve_their_own_identity() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "defs.bzl", "def local(): pass\nlocal()\n");
        let main = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            r#"
load("defs.bzl", imported="local")
local = imported
#^^^^
loc$0al()
#^^^^
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "consumer.bzl",
            "load(\"main.bzl\", \"local\")\nlocal()\n",
        );
        loader.add_files_from_fixture(&fixture);

        let imported = main.contents(&analysis.db).rfind("imported").unwrap();
        let references = analysis
            .snapshot()
            .find_references(
                FilePosition {
                    file_id: main,
                    pos: u32::try_from(imported).unwrap().into(),
                },
                true,
            )
            .unwrap()
            .unwrap();
        assert!(references.iter().any(|reference| reference.file_id != main));
        check_find_references_from_fixture(analysis, fixture);
    }

    #[test]
    fn test_variable() {
        check_find_references(
            r#"
abc = 123
#^^

a$0bc
#^^
"#,
        );
    }

    #[test]
    fn test_variable_with_function_definition() {
        check_find_references(
            r#"
def foo():
    #^^
    pass

f$0oo()
#^^
"#,
        );
    }

    #[test]
    fn test_function_definition() {
        check_find_references(
            r#"
def f$0oo():
    #^^
    pass

foo()
#^^
"#,
        );
    }

    #[test]
    fn test_redeclared_variable() {
        check_find_references(
            r#"
foo = 123
#^^
foo
#^^
foo = "abc"
#^^
f$0oo
#^^
"#,
        );
    }

    #[test]
    fn test_redeclared_function() {
        check_find_references(
            r#"
def foo():
    #^^
    pass
foo
#^^
while True:
    def foo():
        pass
    foo
foo = "abc"
#^^
f$0oo
#^^
"#,
        );
    }
}
