use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::find_node::CoveringNode;
use ruff_python_ast::visitor::source_order::SourceOrderVisitor;
use ruff_python_ast::visitor::source_order::TraversalSignal;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::ExprName;
use ruff_python_ast::StmtFunctionDef;
use ruff_text_size::Ranged;
use starpls_common::parsed_module;
use starpls_common::File;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_semantic::types::ide_support::definitions_for_name;
use ty_python_semantic::types::ide_support::ImportAliasResolution;
use ty_python_semantic::HasDefinition;
use ty_python_semantic::SemanticModel;

use crate::util::navigation_token;
use crate::util::text_range;
use crate::Database;
use crate::FilePosition;
use crate::Location;

enum NameNode<'a> {
    Reference(&'a ExprName),
    Definition(&'a StmtFunctionDef),
}

impl<'a> NameNode<'a> {
    fn at(node: &CoveringNode<'a>, range: ruff_text_size::TextRange) -> Option<Self> {
        match node.node() {
            AnyNodeRef::ExprName(name) => Some(Self::Reference(name)),
            AnyNodeRef::Identifier(_) => {
                let AnyNodeRef::StmtFunctionDef(def) = node.parent()? else {
                    return None;
                };
                (def.name.range() == range).then_some(Self::Definition(def))
            }
            _ => None,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Reference(name) => name.id.as_str(),
            Self::Definition(def) => def.name.as_str(),
        }
    }

    fn definitions<'db>(&self, model: &SemanticModel<'db>) -> Vec<Definition<'db>> {
        match self {
            Self::Reference(name) => definitions_for_name(
                model,
                self.name(),
                (*name).into(),
                ImportAliasResolution::PreserveAliases,
            )
            .into_iter()
            .filter_map(|definition| definition.definition())
            .collect(),
            Self::Definition(def) => {
                if model.scope((*def).into()).is_none() {
                    return Vec::new();
                }
                vec![def.definition(model)]
            }
        }
    }
}

pub(crate) fn find_references(
    db: &Database,
    FilePosition { file_id: file, pos }: FilePosition,
) -> Option<Vec<Location>> {
    let model = SemanticModel::new(db, db.starlark_program_file(file));
    let parsed = parsed_module(db, file).load(db);
    let source = file.contents(db);
    let token = navigation_token(&source, parsed.tokens(), u32::from(pos).into())?;
    let node = covering_node(parsed.syntax().into(), token.range());
    let selected = NameNode::at(&node, token.range())?;
    let name = selected.name();
    let definitions = selected
        .definitions(&model)
        .into_iter()
        .filter(|definition| {
            definition.program_file(db) == model.program_file()
                && matches!(
                    definition.kind(db),
                    DefinitionKind::Function(_)
                        | DefinitionKind::Assignment(_)
                        | DefinitionKind::AugmentedAssignment(_)
                        | DefinitionKind::For(_)
                        | DefinitionKind::Comprehension(_)
                )
        })
        .collect::<Vec<_>>();
    if definitions.is_empty() {
        return None;
    }

    let mut visitor = ReferenceVisitor {
        model: &model,
        file,
        name,
        definitions: &definitions,
        locations: Vec::new(),
    };
    visitor.visit_body(&parsed.syntax().body);
    Some(visitor.locations)
}

struct ReferenceVisitor<'db, 'request> {
    model: &'request SemanticModel<'db>,
    file: File,
    name: &'request str,
    definitions: &'request [Definition<'db>],
    locations: Vec<Location>,
}

impl<'ast> SourceOrderVisitor<'ast> for ReferenceVisitor<'_, '_> {
    fn enter_node(&mut self, node: AnyNodeRef<'ast>) -> TraversalSignal {
        let candidate = match node {
            AnyNodeRef::ExprName(name) => NameNode::Reference(name),
            AnyNodeRef::StmtFunctionDef(def) => NameNode::Definition(def),
            _ => return TraversalSignal::Traverse,
        };
        let Self {
            model,
            file,
            name,
            definitions,
            locations,
        } = self;
        if candidate.name() == *name
            && candidate
                .definitions(model)
                .iter()
                .any(|def| definitions.contains(def))
        {
            let range = match candidate {
                NameNode::Reference(name) => name.range(),
                NameNode::Definition(def) => def.name.range(),
            };
            locations.push(Location {
                file_id: *file,
                range: text_range(range),
            });
        }
        TraversalSignal::Traverse
    }
}

#[cfg(test)]
mod tests {
    use crate::Analysis;
    use crate::FilePosition;

    fn check_find_references(fixture: &str) {
        let (analysis, fixture) = Analysis::from_single_file_fixture(fixture);
        let references = analysis
            .snapshot()
            .find_references(
                fixture
                    .cursor_pos
                    .map(|(file_id, pos)| FilePosition { file_id, pos })
                    .unwrap(),
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
                .find_references(FilePosition {
                    file_id: file,
                    pos: u32::try_from(source.rfind("value").unwrap())
                        .unwrap()
                        .into(),
                })
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
foo = "abc"
#^^
f$0oo
#^^
"#,
        );
    }
}
