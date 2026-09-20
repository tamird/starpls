use ruff_text_size::Ranged;
use starpls_bazel::APIContext;
use starpls_common::parsed_module;
use starpls_common::File;
use starpls_syntax::TextRange;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::global_scope;
use ty_python_core::place_table;
use ty_python_core::use_def_map;

use crate::Database;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolTag {
    Deprecated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentSymbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    pub tags: Option<Vec<SymbolTag>>,
    pub range: TextRange,
    pub selection_range: TextRange,
    pub children: Option<Vec<DocumentSymbol>>,
}

pub(crate) fn document_symbols(db: &Database, file_id: File) -> Option<Vec<DocumentSymbol>> {
    let file = file_id;
    let scope = global_scope(db, db.starlark_program_file(file));
    let places = place_table(db, scope);
    let parsed = parsed_module(db, file).load(db);
    let mut symbols = use_def_map(db, scope)
        .all_reachable_symbols()
        .filter_map(|(symbol, _declarations, bindings)| {
            let (kind, range) = bindings
                .filter_map(|binding| binding.binding.definition())
                .filter_map(|definition| {
                    let kind = definition.kind(db);
                    Some(match kind {
                        DefinitionKind::Function(function) => {
                            let node = function.node(&parsed);
                            (Some(SymbolKind::Function), node.range())
                        }
                        DefinitionKind::Assignment(assignment) => {
                            let target = assignment.target(&parsed);
                            (Some(SymbolKind::Variable), target.range())
                        }
                        DefinitionKind::AugmentedAssignment(assignment) => {
                            let target = &assignment.node(&parsed).target;
                            (Some(SymbolKind::Variable), target.range())
                        }
                        DefinitionKind::For(for_stmt) => {
                            let target = for_stmt.target(&parsed);
                            (Some(SymbolKind::Variable), target.range())
                        }
                        // A load binding hides an earlier local definition from the outline.
                        DefinitionKind::ProvidedBinding(_) => (None, kind.target_range(&parsed)),
                        _ => return None,
                    })
                })
                .max_by_key(|(_, range)| range.start())?;
            let kind = kind?;
            let range = crate::util::text_range(range);
            Some(DocumentSymbol {
                name: places.symbol(symbol).name().to_string(),
                detail: None,
                kind,
                tags: None,
                range,
                selection_range: range,
                children: None,
            })
        })
        .collect();
    if file.api_context() == Some(APIContext::Build) {
        add_target_symbols(db, file, &mut symbols);
    }

    symbols.sort_by_key(|symbol| symbol.range.start());
    Some(symbols)
}

fn add_target_symbols(db: &Database, file: File, acc: &mut Vec<DocumentSymbol>) {
    let source = file.contents(db);
    let parsed = parsed_module(db, file).load(db);
    let targets =
        crate::build_targets::calls(parsed.syntax(), parsed.tokens()).filter_map(|call| {
            let range = crate::util::text_range(call.range());
            let name = crate::build_targets::names(call, &source, parsed.tokens()).next()?;
            Some(DocumentSymbol {
                name: format!(":{}", name),
                detail: None,
                kind: SymbolKind::Variable,
                tags: None,
                range,
                selection_range: range,
                children: None,
            })
        });
    acc.extend(targets);
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use expect_test::Expect;
    use starpls_bazel::APIContext;
    use starpls_common::Dialect;
    use starpls_common::FileInfo;
    use starpls_hir::Fixture;

    use crate::Analysis;

    fn check(input: &str, expect: Expect) {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let file_id = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD.bazel",
            input,
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);

        let symbols = analysis
            .snapshot()
            .document_symbols(file_id)
            .unwrap()
            .unwrap();
        let mut actual = String::new();
        for symbol in symbols {
            actual.push_str(&format!("{:?}", symbol));
            actual.push('\n');
        }
        expect.assert_eq(&actual);
    }

    #[test]
    fn includes_loop_bindings_and_the_last_assignment() {
        let source = "value = 0\nfor item in [1]:\n    value = item\n    nested = item\n";
        let (analysis, fixture) = Analysis::from_single_file_fixture(source);
        let symbols = analysis
            .snapshot()
            .document_symbols(fixture.main_file())
            .unwrap()
            .unwrap();
        let names: Vec<_> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert_eq!(names, ["item", "value", "nested"]);
        let value = symbols
            .iter()
            .find(|symbol| symbol.name == "value")
            .unwrap();
        assert_eq!(
            usize::from(value.range.start()),
            source.rfind("value").unwrap()
        );
    }

    #[test]
    fn excludes_definitions_in_unsupported_statements() {
        let (analysis, fixture) = Analysis::from_single_file_fixture(
            "visible = 1\ndef visible_function():\n    pass\nwhile flag:\n    visible = 2\n    hidden = 2\n    def visible_function():\n        pass\n    def hidden_function():\n        pass\nclass visible:\n    field = 3\n",
        );
        let symbols = analysis
            .snapshot()
            .document_symbols(fixture.main_file())
            .unwrap()
            .unwrap();
        let names: Vec<_> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert_eq!(names, ["visible", "visible_function"]);
    }

    #[test]
    fn test_none() {
        check(r#""#, expect![]);
    }

    #[test]
    fn test_variables_and_functions() {
        check(
            r#"s = "abc"

def foo():
    pass
"#,
            expect![[r#"
                DocumentSymbol { name: "s", detail: None, kind: Variable, tags: None, range: 0..1, selection_range: 0..1, children: None }
                DocumentSymbol { name: "foo", detail: None, kind: Function, tags: None, range: 11..30, selection_range: 11..30, children: None }
            "#]],
        );
    }

    #[test]
    fn test_use_last_assignment() {
        check(
            r#"
y = "abc"
x = 123
x = "123"
"#,
            expect![[r#"
                DocumentSymbol { name: "y", detail: None, kind: Variable, tags: None, range: 1..2, selection_range: 1..2, children: None }
                DocumentSymbol { name: "x", detail: None, kind: Variable, tags: None, range: 19..20, selection_range: 19..20, children: None }
            "#]],
        );
    }

    #[test]
    fn test_skip_load_items() {
        check(
            r#"
load("foo.star", "foo")

bar = 1
"#,
            expect![[r#"
                DocumentSymbol { name: "bar", detail: None, kind: Variable, tags: None, range: 26..29, selection_range: 26..29, children: None }
            "#]],
        )
    }

    #[test]
    fn test_targets() {
        check(
            r#"
NUMS = [1, 2, 3]

rust_library(
    name = "starpls_ide",
    srcs = glob(["src/**/*.rs"]),
)

rust_library_test(
    name = "starpls_ide_test",
    crates = ":starpls_ide",
)
"#,
            expect![[r#"
                DocumentSymbol { name: "NUMS", detail: None, kind: Variable, tags: None, range: 1..5, selection_range: 1..5, children: None }
                DocumentSymbol { name: ":starpls_ide", detail: None, kind: Variable, tags: None, range: 19..94, selection_range: 19..94, children: None }
                DocumentSymbol { name: ":starpls_ide_test", detail: None, kind: Variable, tags: None, range: 96..176, selection_range: 96..176, children: None }
            "#]],
        )
    }
}
