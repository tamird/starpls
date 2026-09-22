use ruff_python_ast::find_node::covering_node;
use ruff_text_size::Ranged;
use starpls_common::parsed_module;
use ty_ide::references_in_file;
use ty_ide::ReferenceTarget;
use ty_python_core::definition::DefinitionKind;
use ty_python_semantic::types::ide_support::definitions_for_keyword_argument;
use ty_python_semantic::types::ide_support::definitions_for_name;
use ty_python_semantic::types::ide_support::ImportAliasResolution;
use ty_python_semantic::HasDefinition;
use ty_python_semantic::ResolvedDefinition;
use ty_python_semantic::SemanticModel;

use crate::selection::Selection;
use crate::util::navigation_token;
use crate::Database;
use crate::FilePosition;

pub(crate) fn find_references(
    db: &Database,
    FilePosition { file_id: file, pos }: FilePosition,
    include_declaration: bool,
) -> Option<Vec<ReferenceTarget>> {
    let model = SemanticModel::new(db, db.starlark_program_file(file));
    let parsed = parsed_module(db, file).load(db);
    let source = file.contents(db);
    let token = navigation_token(&source, parsed.tokens(), u32::from(pos).into())?;
    let node = covering_node(parsed.syntax().into(), token.range());
    let (name, definitions) = match crate::selection::classify(&node, token.range())? {
        Selection::Reference(name) => {
            model.scope(name.into())?;
            (
                name.id.as_str(),
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
                vec![ResolvedDefinition::Definition(function.definition(&model))],
            )
        }
        Selection::Parameter(parameter) => {
            model.scope(parameter.into())?;
            (
                parameter.name.as_str(),
                vec![ResolvedDefinition::Definition(parameter.definition(&model))],
            )
        }
        Selection::Keyword { keyword, call } => {
            model.scope(call.into())?;
            (
                keyword.arg.as_ref()?.as_str(),
                definitions_for_keyword_argument(&model, keyword, call),
            )
        }
        _ => return None,
    };
    let definitions = definitions
        .into_iter()
        .filter(|resolved| {
            resolved.definition().is_some_and(|definition| {
                definition.program_file(db) == model.program_file()
                    && matches!(
                        definition.kind(db),
                        DefinitionKind::Function(_)
                            | DefinitionKind::Parameter(_)
                            | DefinitionKind::Assignment(_)
                            | DefinitionKind::AnnotatedAssignment(_)
                            | DefinitionKind::AugmentedAssignment(_)
                            | DefinitionKind::For(_)
                            | DefinitionKind::Comprehension(_)
                    )
            })
        })
        .collect::<Vec<_>>();
    if definitions.is_empty() {
        return None;
    }

    Some(references_in_file(
        db,
        model.program_file(),
        name,
        &definitions,
        include_declaration,
    ))
}

#[cfg(test)]
mod tests {
    use crate::Analysis;
    use crate::FilePosition;
    use crate::ReferenceKind;

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
    fn local_alias_references_stay_in_the_selected_file() {
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
        assert!(analysis
            .snapshot()
            .find_references(
                FilePosition {
                    file_id: main,
                    pos: u32::try_from(imported).unwrap().into(),
                },
                true
            )
            .unwrap()
            .is_none());
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
