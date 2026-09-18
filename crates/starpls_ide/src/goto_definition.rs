use ruff_python_ast::find_node::covering_node;
use ruff_text_size::Ranged;
use starpls_common::File;
use starpls_common::InFile;
use starpls_hir::LoadItem;
use starpls_hir::Name;
use starpls_hir::ScopeDef;
use starpls_hir::Semantics;
use starpls_syntax::source::string_value;
use starpls_syntax::TextRange;

use crate::selection::Selection;
use crate::util::navigation_token;
use crate::util::text_range;
use crate::Database;
use crate::FilePosition;
use crate::LocationLink;
use crate::ResolvedPath;

struct GotoDefinitionHandler<'a> {
    sema: Semantics<'a>,
    file: File,
    origin: TextRange,
    skip_re_exports: bool,
}

impl GotoDefinitionHandler<'_> {
    fn handle(&self, selection: Selection<'_>, source: &str) -> Option<Vec<LocationLink>> {
        let Self {
            sema,
            file,
            origin,
            skip_re_exports: _,
        } = self;
        match selection {
            Selection::Reference(name) => {
                let scope = sema.scope_for_expr(*file, name.into())?;
                Some(
                    scope
                        .resolve_name(&Name::from(name.id.as_str()))
                        .into_iter()
                        .filter_map(|def| {
                            if let ScopeDef::LoadItem(item) = def {
                                self.load_item_location(&item)
                            } else {
                                self.def_to_location_link(def)
                            }
                        })
                        .collect(),
                )
            }
            Selection::Attribute(expr) => {
                let ty = sema.type_of_expr(*file, expr.value.as_ref().into())?;
                Some(vec![location_link(
                    ty.field_definition(expr.attr.as_str())?,
                )])
            }
            Selection::Keyword { keyword, call } => {
                let callable = sema.resolve_call_expr(*file, call)?;
                Some(vec![location_link(
                    callable.keyword_definition(keyword.arg.as_ref()?.as_str())?,
                )])
            }
            Selection::LoadModule(call) => Some(vec![LocationLink::Local {
                origin_selection_range: Some(*origin),
                target_range: Default::default(),
                target_selection_range: Default::default(),
                target_file_id: sema.resolve_load_stmt(*file, call)?,
            }]),
            Selection::LoadItem { call: _, item } => {
                let item = sema.resolve_load_item(*file, item)?;
                self.load_item_location(&item)
                    .map(|location| vec![location])
            }
            Selection::String(expr) => {
                // The node must belong to Starlark lowering, not to an ignored
                // Python-only annotation or unsupported statement subtree.
                if !sema.contains_expr(*file, expr.into()) {
                    return None;
                }
                let (value, _) = string_value(&source[expr.range()])?;
                self.string_location(&value)
            }
            Selection::Definition(_) => None,
            Selection::Parameter(_) => None,
        }
    }

    fn load_item_location(&self, item: &LoadItem<'_>) -> Option<LocationLink> {
        let Self {
            sema: _,
            file: _,
            origin: _,
            skip_re_exports,
        } = self;
        if *skip_re_exports {
            self.try_resolve_re_export(item)
        } else {
            self.def_to_location_link(item.definition()?)
        }
    }

    fn try_resolve_re_export(&self, load_item: &LoadItem<'_>) -> Option<LocationLink> {
        let def = load_item.definition()?;
        if let ScopeDef::Variable(variable) = &def {
            if let Some(item) = variable.re_export() {
                if let Some(location) = self.try_resolve_re_export(&item) {
                    return Some(location);
                }
            }
        }
        self.def_to_location_link(def)
    }

    fn string_location(&self, value: &str) -> Option<Vec<LocationLink>> {
        let Self {
            sema,
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
                    target_file_id: build_file,
                }])
            }
        }
    }

    fn def_to_location_link(&self, def: ScopeDef<'_>) -> Option<LocationLink> {
        Some(location_link(def.definition_range()?))
    }
}

fn location_link(InFile { file, value: range }: InFile<TextRange>) -> LocationLink {
    LocationLink::Local {
        origin_selection_range: None,
        target_range: range,
        target_selection_range: range,
        target_file_id: file,
    }
}

pub(crate) fn goto_definition(
    db: &Database,
    FilePosition { file_id: file, pos }: FilePosition,
    skip_re_exports: bool,
) -> Option<Vec<LocationLink>> {
    let sema = Semantics::new(db);
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
        sema,
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
        analysis: Analysis,
        fixture: Fixture,
        skip_re_exports: bool,
    ) {
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
        assert_eq!(fixture.selected_ranges, actual);
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
    #^^^^^^^
    "foo": "second",
})
"#,
                "alias().fo$0o",
            ),
            (
                r#"value = rule(attrs = {
    "foo": None,
    #^^^^
    "foo": attr.string(),
})
"#,
                "alias(fo$0o = 1)",
            ),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = Fixture::new(&mut analysis.db);
            fixture.add_file(&mut analysis.db, "//:defs.bzl", definition);
            fixture.add_file(
                &mut analysis.db,
                "//:main.bzl",
                &format!("load(\"//:defs.bzl\", alias = \"value\")\n{usage}"),
            );
            loader.add_files_from_fixture(&fixture);
            check_goto_definition_from_fixture(analysis, fixture, false);
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
            ("foo = 1", "foo = (_foo)\n#^^"),
            ("foo = 1", "(foo) = _foo\n #^^"),
            ("foo = 1", "foo, other = _foo\n#^^"),
            ("foo = 1\n#^^", "_foo = 0\nfoo = _foo"),
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
