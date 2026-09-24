use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::Expr;
use ruff_text_size::Ranged;
use rustc_hash::FxHashSet;
use starpls_common::File;
use starpls_hir::Source;
use starpls_syntax::source::string_value;
use starpls_syntax::TextRange;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::definition::ParameterDefinitionNodeKind;
use ty_python_core::semantic_index;
use ty_python_semantic::definitions_for_attribute;
use ty_python_semantic::types::ide_support::definitions_for_keyword_argument;
use ty_python_semantic::types::ide_support::definitions_for_name;
use ty_python_semantic::types::ide_support::resolve_definition;
use ty_python_semantic::ImportAliasResolution;
use ty_python_semantic::ProgramEnvironment;
use ty_python_semantic::ResolvedDefinition;
use ty_python_semantic::SemanticModel;

use crate::selection::Selection;
use crate::util::navigation_token;
use crate::util::text_range;
use crate::Database;
use crate::FilePosition;
use crate::LocationLink;
use crate::ResolvedPath;

struct GotoDefinitionHandler<'a> {
    db: &'a Database,
    sema: Source<'a>,
    model: SemanticModel<'a>,
    file: File,
    origin: TextRange,
    skip_re_exports: bool,
}

impl<'db> GotoDefinitionHandler<'db> {
    fn handle(&self, selection: Selection<'_>, source: &str) -> Option<Vec<LocationLink>> {
        let Self {
            db: _,
            sema,
            model,
            file,
            origin,
            skip_re_exports: _,
        } = self;
        match selection {
            Selection::Reference(name) => {
                model.scope(name.into())?;
                let definitions = definitions_for_name(
                    model,
                    name.id.as_str(),
                    name.into(),
                    ImportAliasResolution::PreserveAliases,
                );
                let definitions = definitions
                    .into_iter()
                    .flat_map(|resolved| {
                        if let ResolvedDefinition::Definition(definition) = resolved {
                            if matches!(
                                definition.kind(model.db()),
                                DefinitionKind::ProvidedBinding(_)
                            ) {
                                return self
                                    .load_definitions(definition, &mut FxHashSet::default());
                            }
                        }
                        vec![resolved]
                    })
                    .collect();
                Some(self.semantic_locations(definitions))
            }
            Selection::Attribute(expr) => {
                model.scope(expr.into())?;
                Some(self.semantic_locations(definitions_for_attribute(model, expr)))
            }
            Selection::Keyword { keyword, call } => {
                model.scope(call.into())?;
                Some(
                    self.semantic_locations(definitions_for_keyword_argument(model, keyword, call)),
                )
            }
            Selection::LoadModule(call) => Some(vec![LocationLink::Local {
                origin_selection_range: Some(*origin),
                target_range: Default::default(),
                target_selection_range: Default::default(),
                target_file_id: sema.resolve_load_stmt(*file, call)?.source,
            }]),
            Selection::LoadItem { call: _, item } => {
                let target = match item {
                    ArgOrKeyword::Arg(expression) => expression.into(),
                    ArgOrKeyword::Keyword(keyword) => keyword.into(),
                };
                let index = semantic_index(model.db(), model.program_file());
                let [definition] = index.try_definitions(target)? else {
                    return None;
                };
                let definitions = self.load_definitions(*definition, &mut FxHashSet::default());
                Some(self.semantic_locations(definitions))
            }
            Selection::String(expr) => {
                // The node must be admitted by the Starlark frontend.
                model.scope(expr.into())?;
                let (value, _) = string_value(&source[expr.range()])?;
                self.string_location(&value)
            }
            Selection::Definition(_) => None,
            Selection::Parameter(_) => None,
        }
    }

    fn semantic_locations(&self, definitions: Vec<ResolvedDefinition<'db>>) -> Vec<LocationLink> {
        definitions
            .into_iter()
            .filter_map(|definition| {
                let db = self.model.db();
                let source = definition.focus_range(db);
                let file = source.file();
                // Native and vendored declarations are checker inputs, not user
                // source files the Starlark client can open.
                file.path(db).as_system_path()?;
                let selection = text_range(source.range());
                let target = definition
                    .definition()
                    .and_then(|definition| {
                        let DefinitionKind::Parameter(parameter) = definition.kind(db) else {
                            return None;
                        };
                        let ParameterDefinitionNodeKind::Parameter(parameter) = parameter else {
                            return None;
                        };
                        let parsed =
                            ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db);
                        Some(text_range(parameter.node(&parsed).range()))
                    })
                    .unwrap_or(selection);
                Some(LocationLink::Local {
                    origin_selection_range: Some(self.origin),
                    target_range: target,
                    target_selection_range: selection,
                    target_file_id: file,
                })
            })
            .collect()
    }

    fn load_definitions(
        &self,
        definition: Definition<'db>,
        visited: &mut FxHashSet<Definition<'db>>,
    ) -> Vec<ResolvedDefinition<'db>> {
        let db = self.model.db();
        if !visited.insert(definition) {
            return Vec::new();
        }
        let environment = ProgramEnvironment::from_file(definition.program_file(db));
        let implementation = self.db.interface_implementation(definition);
        let definitions = if implementation.is_empty() {
            resolve_definition(
                db,
                &environment,
                definition,
                None,
                ImportAliasResolution::ResolveAliases,
            )
        } else {
            implementation
                .into_iter()
                .map(ResolvedDefinition::Definition)
                .collect()
        };
        definitions
            .into_iter()
            .filter(|resolved| resolved.definition() != Some(definition))
            .flat_map(|resolved| {
                if self.skip_re_exports {
                    if let Some(reexport) = resolved
                        .definition()
                        .and_then(|definition| self.reexported_binding(definition))
                    {
                        let targets = self.load_definitions(reexport, visited);
                        if !targets.is_empty() {
                            return targets;
                        }
                    }
                }
                vec![resolved]
            })
            .collect()
    }

    /// The Starlark option follows a direct assignment of a loaded name. Shared
    /// name resolution decides which binding that source expression refers to.
    fn reexported_binding(&self, definition: Definition<'db>) -> Option<Definition<'db>> {
        let db = self.model.db();
        let DefinitionKind::Assignment(assignment) = definition.kind(db) else {
            return None;
        };
        if assignment.unpack().is_some() {
            return None;
        }
        let parsed = ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db);
        let Expr::Name(name) = assignment.value(&parsed) else {
            return None;
        };
        let model = SemanticModel::new(db, definition.program_file(db));
        let definitions = definitions_for_name(
            &model,
            name.id.as_str(),
            name.into(),
            ImportAliasResolution::PreserveAliases,
        );
        let [ResolvedDefinition::Definition(binding)] = definitions.as_slice() else {
            return None;
        };
        matches!(binding.kind(db), DefinitionKind::ProvidedBinding(_)).then_some(*binding)
    }

    fn string_location(&self, value: &str) -> Option<Vec<LocationLink>> {
        let Self {
            db: _,
            sema,
            model: _,
            file,
            origin,
            skip_re_exports: _,
        } = self;
        match sema.db.resolve_path(value, file.dialect, *file).ok()?? {
            ResolvedPath::Source { path } => path.try_exists().ok()?.then(|| {
                vec![LocationLink::External {
                    origin_selection_range: Some(*origin),
                    target_path: path,
                }]
            }),
            ResolvedPath::BuildTarget { build_file, target } => {
                let source = build_file.contents(sema.db);
                let parsed = starpls_common::parsed_module(sema.db, build_file).load(sema.db);
                let range = crate::build_targets::calls(parsed.syntax(), parsed.tokens())
                    .find(|call| {
                        crate::build_targets::names(call, &source, parsed.tokens())
                            .any(|name| *name == target)
                    })
                    .map(|call| text_range(call.range()))
                    .unwrap_or_default();
                Some(vec![LocationLink::Local {
                    origin_selection_range: Some(*origin),
                    target_range: range,
                    target_selection_range: range,
                    target_file_id: build_file.source,
                }])
            }
        }
    }
}

pub(crate) fn goto_definition(
    db: &Database,
    FilePosition { file_id: file, pos }: FilePosition,
    skip_re_exports: bool,
) -> Option<Vec<LocationLink>> {
    let sema = Source::new(db);
    let source = file.contents(db);
    let parsed = starpls_common::parsed_module(db, file).load(db);
    let token = navigation_token(&source, parsed.tokens(), u32::from(pos).into())?;
    if crate::selection::type_comment_at_cursor(
        starpls_common::syntax_info(db, file),
        u32::from(pos).into(),
        token,
        &source,
    )
    .is_some()
    {
        return None;
    }
    let node = covering_node(parsed.syntax().into(), token.range());
    let selection = crate::selection::classify(&node, token.range())?;
    GotoDefinitionHandler {
        db,
        sema,
        model: SemanticModel::new(db, db.starlark_program_file(file)),
        file,
        origin: text_range(token.range()),
        skip_re_exports,
    }
    .handle(selection, &source)
}

#[cfg(test)]
mod tests {
    use starpls_bazel::APIContext;
    use starpls_common::Dialect;
    use starpls_common::FileInfo::Bazel;
    use starpls_hir::Fixture;

    use crate::Analysis;
    use crate::FilePosition;
    use crate::LocationLink;

    fn check_goto_definition(fixture: &str) {
        let (analysis, fixture) = Analysis::from_single_file_fixture(fixture);
        check_goto_definition_from_fixture(analysis, fixture, false);
    }

    fn check_goto_definition_from_fixture(
        mut analysis: Analysis,
        fixture: Fixture,
        skip_re_exports: bool,
    ) {
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        let actual = analysis
            .snapshot()
            .goto_definition(
                fixture
                    .cursor_pos
                    .map(|(file_id, pos)| FilePosition { file_id, pos })
                    .unwrap(),
                skip_re_exports,
            )
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|loc| match loc {
                LocationLink::Local {
                    target_range,
                    target_file_id,
                    ..
                } => (target_file_id, target_range),
                _ => panic!("expected local location"),
            })
            .collect::<Vec<_>>();
        let expected: Vec<_> = fixture
            .selected_ranges
            .into_iter()
            .map(|(file, range)| (file.source, range))
            .collect();
        assert_eq!(expected, actual);
    }

    #[test]
    fn test_simple() {
        check_goto_definition(
            r#"
foo = 1
#^^
f$0oo
"#,
        )
    }

    #[test]
    fn test_global_variable() {
        check_goto_definition(
            r#"
GLOBAL_LIST = [1, 2, 3]
#^^^^^^^^^^
def f():
    print(GLOBAL$0_LIST)
"#,
        )
    }

    #[test]
    fn test_function() {
        check_goto_definition(
            r#"
def foo():
    #^^
    pass

f$0oo()
"#,
        );
    }

    #[test]
    fn test_param() {
        check_goto_definition(
            r#"
def f(abc):
      #^^
      a$0bc
"#,
        )
    }

    #[test]
    fn test_lambda_param() {
        check_goto_definition(
            r#"
lambda abc: print(a$0bc)
       #^^
"#,
        );
    }

    #[test]
    fn test_keyword_argument() {
        check_goto_definition(
            r#"
def foo(abc):
        #^^
        print(abc)

foo(a$0bc = 123)
"#,
        );
    }

    #[test]
    fn test_rule_attribute() {
        check_goto_definition(
            r#"
def _foo_impl(ctx):
    pass

foo = rule(
    implementation = _foo_impl,
    attrs = {
        "bar": attr.string(),
        #^^^^
    },
)

foo(
    name = "foo",
    b$0ar = "baz",
)
"#,
        );
    }

    #[test]
    fn test_struct_field() {
        check_goto_definition(
            r#"
s = struct(foo = "bar")
           #^^

s.f$0oo
"#,
        )
    }

    #[test]
    fn test_provider_field() {
        check_goto_definition(
            r#"
GoInfo = provider(
    fields = {
        "foo": "The foo field",
        #^^^^
    },
)
info = GoInfo(foo = 123)
info.fo$0o
"#,
        )
    }

    #[test]
    fn test_imported_field_origins() {
        for (definition, usage) in [
            (
                "value = struct(foo = 1, foo = 2)\n               #^^\n",
                "alias.fo$0o",
            ),
            (
                r#"value = provider(fields = {
    "\x66oo": "first",
    "foo": "second",
    #^^^^
})
"#,
                "alias().fo$0o",
            ),
            (
                r#"value = rule(attrs = {
    "foo": None,
    "foo": attr.string(),
    #^^^^
})
"#,
                "alias(fo$0o = 1)",
            ),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = Fixture::new(&mut analysis.db);
            fixture.add_file(&mut analysis.db, "//:defs.bzl", definition);
            fixture.add_file_with_options(
                &mut analysis.db,
                "BUILD.bazel",
                &format!("load(\"//:defs.bzl\", alias = \"value\")\n{usage}"),
                Dialect::Bazel,
                Some(Bazel {
                    api_context: APIContext::Build,
                    is_external: false,
                }),
            );
            loader.add_files_from_fixture(&fixture);
            check_goto_definition_from_fixture(analysis, fixture, false);
        }
    }

    #[test]
    fn supplied_parameter_origins() {
        check_goto_definition(
            r#"
P = provider(fields=["foo"])
                     #^^^^
P(fo$0o=1)
"#,
        );
        check_goto_definition(
            r#"
P, raw = provider(fields=["foo"], init=lambda **kwargs: kwargs)
                          #^^^^
raw(fo$0o=1)
"#,
        );
        check_goto_definition(
            r#"
def initialize(foo):
               #^^
    return {"foo": foo}
P, raw = provider(fields=["foo"], init=initialize)
P(fo$0o=1)
"#,
        );
    }

    #[test]
    fn selected_load_binding_survives_later_reassignment() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        fixture.add_file(&mut analysis.db, "//:defs.bzl", "value = 1\n#^^^^");
        fixture.add_file(
            &mut analysis.db,
            "//:main.bzl",
            "load(\"//:defs.bzl\", \"va$0lue\")\nvalue = 2",
        );
        loader.add_files_from_fixture(&fixture);
        check_goto_definition_from_fixture(analysis, fixture, false);
    }

    #[test]
    fn imported_field_origins_follow_source_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let original = "value = struct(field=1)";
        let dependency = fixture.add_file(&mut analysis.db, "//:defs.bzl", original);
        let main = fixture.add_file(
            &mut analysis.db,
            "//:main.bzl",
            "load(\"//:defs.bzl\", \"value\")\nvalue.fi$0eld",
        );
        loader.add_files_from_fixture(&fixture);
        let (_, pos) = fixture.cursor_pos.unwrap();
        for (revision, source) in [
            original,
            "# moved\nvalue = struct(field='edited')",
            original,
        ]
        .into_iter()
        .enumerate()
        {
            analysis.update_file(dependency, source.into());
            let snapshot = analysis.snapshot();
            let position = FilePosition { file_id: main, pos };
            if revision == 1 {
                let hover = snapshot.hover(position.clone()).unwrap().unwrap();
                assert!(
                    hover.contents.value.contains("edited"),
                    "{}",
                    hover.contents.value
                );
            }
            let locations = snapshot.goto_definition(position, false).unwrap().unwrap();
            let [LocationLink::Local {
                origin_selection_range: _,
                target_range,
                target_selection_range,
                target_file_id,
            }] = locations.as_slice()
            else {
                panic!("expected one source field: {locations:?}");
            };
            assert_eq!(*target_file_id, dependency.source);
            let start = source.find("field").unwrap() as u32;
            assert_eq!(u32::from(target_range.start()), start);
            assert_eq!(u32::from(target_range.end()), start + 5);
            assert_eq!(target_range, target_selection_range);
        }
    }

    #[test]
    fn test_incomplete_struct_field() {
        check_goto_definition(
            r#"
s = struct(foo = )
           #^^
s.fo$0o
"#,
        );
    }

    #[test]
    fn test_parameter_range_includes_default() {
        check_goto_definition(
            r#"
def f(abc = 123):
      #^^^^^^^^
    pass
f(ab$0c = 0)
"#,
        );
    }

    #[test]
    fn test_re_export_assignment_shapes() {
        for (definition, declaration) in [
            ("foo = 1\n#^^", "foo = (_foo)"),
            ("foo = 1\n#^^", "(foo) = _foo"),
            ("foo = 1", "foo, other = _foo\n#^^"),
            ("foo = 1", "_foo = 0\nfoo = _foo\n#^^"),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = Fixture::new(&mut analysis.db);
            fixture.add_file(&mut analysis.db, "//:defs.bzl", definition);
            fixture.add_file(
                &mut analysis.db,
                "//:middle.bzl",
                &format!("load(\"//:defs.bzl\", _foo = \"foo\")\n{declaration}"),
            );
            fixture.add_file(
                &mut analysis.db,
                "//:main.bzl",
                "load(\"//:middle.bzl\", \"foo\")\nf$0oo",
            );
            loader.add_files_from_fixture(&fixture);
            check_goto_definition_from_fixture(analysis, fixture, true);
        }
    }

    #[test]
    fn test_load_stmt() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        fixture.add_file(
            &mut analysis.db,
            "//:foo.bzl",
            r#"
def foo():
    #^^
    pass
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "//:bar.bzl",
            r#"
load("//:foo.bzl", "foo")

f$0oo()
"#,
        );
        loader.add_files_from_fixture(&fixture);
        check_goto_definition_from_fixture(analysis, fixture, false);
    }

    #[test]
    fn test_load_stmt_re_export() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        fixture.add_file(
            &mut analysis.db,
            "//:foo.bzl",
            r#"
foo = 123
#^^
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "//:bar.bzl",
            r#"
load("//:foo.bzl", _foo = "foo")

foo = _foo
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "//:baz.bzl",
            r#"
load("//:bar.bzl", "foo")

f$0oo
"#,
        );
        loader.add_files_from_fixture(&fixture);
        check_goto_definition_from_fixture(analysis, fixture, true);
    }

    #[test]
    fn test_load_stmt_re_export_load_item() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        fixture.add_file(
            &mut analysis.db,
            "//:foo.bzl",
            r#"
foo = 123
#^^
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "//:bar.bzl",
            r#"
load("//:foo.bzl", _foo = "foo")

foo = _foo
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "//:baz.bzl",
            r#"
load("//:bar.bzl", "f$0oo")
"#,
        );
        loader.add_files_from_fixture(&fixture);
        check_goto_definition_from_fixture(analysis, fixture, true);
    }

    #[test]
    fn test_load_stmt_re_export_short_circuit() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        fixture.add_file(
            &mut analysis.db,
            "//:foo.bzl",
            r#"
foo = 123
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "//:bar.bzl",
            r#"
load("//:foo.bzl", _foo = "bar")

foo = _foo
#^^
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "//:baz.bzl",
            r#"
load("//:bar.bzl", "foo")

f$0oo
"#,
        );
        loader.add_files_from_fixture(&fixture);
        check_goto_definition_from_fixture(analysis, fixture, true);
    }

    #[test]
    fn test_prelude_variable() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        fixture.add_prelude_file(
            &mut analysis.db,
            r#"
FOO = 123
#^^
"#,
        );
        fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD.bazel",
            r#"
F$0OO
"#,
            Dialect::Bazel,
            Some(Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        check_goto_definition_from_fixture(analysis, fixture, false);
    }

    #[test]
    fn test_prelude_function_definition() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        fixture.add_prelude_file(
            &mut analysis.db,
            r#"
def foo():
    #^^
    pass
"#,
        );
        fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD.bazel",
            r#"
f$0oo()
"#,
            Dialect::Bazel,
            Some(Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        check_goto_definition_from_fixture(analysis, fixture, false);
    }

    #[test]
    fn test_prelude_load_stmt() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        fixture.add_prelude_file(
            &mut analysis.db,
            r#"
load("//:defs.bzl", "java_library")
"#,
        );
        fixture.add_file(
            &mut analysis.db,
            "//:defs.bzl",
            r#"
def java_library():
    #^^^^^^^^^^^
    pass
"#,
        );
        fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD.bazel",
            r#"
j$0ava_library()
"#,
            Dialect::Bazel,
            Some(Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        check_goto_definition_from_fixture(analysis, fixture, false);
    }
}
