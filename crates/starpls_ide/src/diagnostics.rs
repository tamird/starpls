use starpls_common::Diagnostic;
use starpls_common::File;
use starpls_hir::diagnostics_for_file;

use crate::Database;

pub(crate) fn diagnostics(db: &Database, file_id: File) -> Vec<Diagnostic> {
    let file = file_id;

    let diagnostics = crate::ty::check(db, file);

    // Limit the amount of syntax errors we send, as this many syntax errors probably means something
    // is really wrong with the file being analyzed.
    diagnostics_for_file(db, file)
        .take(128)
        .chain(diagnostics)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use salsa::Setter;
    use starpls_hir::Db as _;
    use starpls_hir::Fixture;

    use crate::Analysis;
    use crate::InferenceOptions;
    use crate::SimpleFileLoader;

    fn native_analysis(source: &str) -> (Analysis, Fixture) {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                starpls_bazel::Builtins::default(),
            )
            .unwrap();
        (analysis, fixture)
    }

    #[test]
    fn native_function_annotations_check_bodies_and_build_calls() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let original = "def identity(value: int = 1) -> int:\n    # type: (string) -> string\n    return value\n";
        let definition = fixture.add_file(&mut analysis.db, "//:defs.bzl", original);
        let caller = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD.bazel",
            r#"load("//:defs.bzl", "identity")
answer = identity(1)
"#,
            starpls_common::Dialect::Bazel,
            Some(starpls_common::FileInfo::Bazel {
                api_context: starpls_bazel::APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        for file in [definition, caller] {
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        }
        for (source, expected) in [
            (
                original.replace("= 1", "= 'bad'"),
                "invalid-parameter-default",
            ),
            (
                original.replace("return value", "return 'bad'"),
                "invalid-return-type",
            ),
            (
                original.replace("return value", "return value + 'bad'"),
                "unsupported-operator",
            ),
        ] {
            analysis.update_file(definition, source.clone());
            let diagnostics = analysis.snapshot().diagnostics(definition).unwrap();
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert_eq!(
                diagnostics[0].id().as_str(),
                expected,
                "{source}: {diagnostics:?}"
            );
        }
        for (source, valid) in [
            (original.to_owned(), true),
            (
                "def identity(value: string = 'ok') -> string:\n    return value\n".to_owned(),
                false,
            ),
            (original.to_owned(), true),
        ] {
            analysis.update_file(definition, source);
            let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
            assert_eq!(diagnostics.is_empty(), valid, "{diagnostics:?}");
            if !valid {
                assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
                assert_eq!(diagnostics[0].id().as_str(), "invalid-argument-type");
            }
        }
    }

    #[test]
    fn native_assignments_check_initialization_and_reassignment() {
        let source = "value: int = 1 # type: string\nvalue = 2\nvalues: list[int] = []\nvalues.append(value)\n";
        let (mut analysis, fixture) = native_analysis(source);
        let file = fixture.main_file();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for (statement, expected) in [
            ("value: int = 'bad'", "invalid-assignment"),
            ("value: int = 1\nvalue = 'bad'", "invalid-assignment"),
            ("values: list[int] = ['bad']", "invalid-assignment"),
            (
                "values: list[int] = []\nvalues.append('bad')",
                "invalid-argument-type",
            ),
        ] {
            analysis.update_file(file, statement.to_owned());
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(diagnostics.len(), 1, "{statement}: {diagnostics:?}");
            assert_eq!(
                diagnostics[0].id().as_str(),
                expected,
                "{statement}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn native_annotations_keep_host_types_and_nominal_shadowing() {
        let source = r#"
First = provider(fields=["value"])
Second = provider(fields=["value"])
api = struct(Info=First)
def consume(value: api.Info, items: list[int | string], pairs: tuple[int, ...]) -> api.Info:
    return value
def label(value: Label) -> Label:
    return value
def local():
    Label = First
    def identity(value: Label) -> Label:
        return value
    return identity(First(value=1))
consume(First(value=1), [1, "two"], (1, 2))
label(Label("//:target"))
"#;
        let (mut analysis, fixture) = native_analysis(source);
        let file = fixture.main_file();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for call in [
            "consume(Second(value=1), [], ())",
            "consume(First(value=1), [None], ())",
            "label(First(value=1))",
        ] {
            analysis.update_file(file, format!("{source}\n{call}\n"));
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(diagnostics.len(), 1, "{call}: {diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), "invalid-argument-type");
        }
        analysis.update_file(file, format!("{source}\nruntime_name = string\n"));
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].id().as_str(), "unresolved-reference");
    }

    #[test]
    fn native_annotations_remain_a_bzl_policy() {
        let (mut analysis, _) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        for (path, dialect, context) in [
            (
                "defs.bzl",
                starpls_common::Dialect::Bazel,
                starpls_bazel::APIContext::Bzl,
            ),
            (
                "defs.scl",
                starpls_common::Dialect::Bazel,
                starpls_bazel::APIContext::Bzl,
            ),
            (
                "prelude.bzl",
                starpls_common::Dialect::Bazel,
                starpls_bazel::APIContext::Prelude,
            ),
            (
                "deploy.star",
                starpls_common::Dialect::Standard,
                starpls_bazel::APIContext::Bzl,
            ),
            (
                "BUILD.bazel",
                starpls_common::Dialect::Bazel,
                starpls_bazel::APIContext::Build,
            ),
        ] {
            let file = fixture.add_file_with_options(
                &mut analysis.db,
                path,
                "value: int = 1\ndef identity(value: int) -> int:\n    return value\n",
                dialect,
                Some(starpls_common::FileInfo::Bazel {
                    api_context: context,
                    is_external: false,
                }),
            );
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(
                diagnostics.is_empty(),
                path == "defs.bzl",
                "{path}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn macro_attributes_use_shared_call_checking() {
        let source = r#"
def implementation(**kwargs):
    pass
constructor = macro
miniature = constructor(
    implementation=implementation,
    attrs={
        "optional": attr.string(),
        "required": attr.string(mandatory=True),
        "flag": attr.bool(),
        "disabled": None,
    },
)
miniature(required="ok", optional="ok", inherited="ok")
miniature(**{"required": "ok"})
miniature(required="ok", flag=0)
miniature(required="ok", flag=1)
miniature(required="ok", flag=True)
miniature(**{"required": "ok", "flag": False})
example_rule = rule(implementation=implementation, attrs={"flag": attr.bool()})
example_rule(name="zero", flag=0)
example_rule(name="one", flag=1)
example_rule(name="true", flag=True)
example_rule(name="false", flag=False)
def shadowed():
    def macro(value):
        # type: (int) -> int
        return value
    return macro(1)
"#;
        let (mut analysis, fixture) = native_analysis(source);
        let file = fixture.main_file();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        for (call, expected) in [
            (
                "miniature(required=\"ok\", optional=1)",
                "invalid-argument-type",
            ),
            ("miniature(optional=\"ok\")", "missing-argument"),
            (
                "miniature(required=\"ok\", disabled=\"bad\")",
                "invalid-argument-type",
            ),
            (
                "miniature(**{\"required\": \"ok\", \"disabled\": \"bad\"})",
                "invalid-argument-type",
            ),
            ("miniature(required=\"ok\", flag=2)", "invalid-argument-type"),
            (
                "def generic(value):\n    # type: (int) -> None\n    miniature(required=\"ok\", flag=value)",
                "invalid-argument-type",
            ),
            ("example_rule(name=\"bad\", flag=2)", "invalid-argument-type"),
            (
                "def generic(value):\n    # type: (int) -> None\n    example_rule(name=\"bad\", flag=value)",
                "invalid-argument-type",
            ),
            ("attr.bool(mandatory=1)", "invalid-argument-type"),
        ] {
            analysis.update_file(file, format!("{source}\n{call}\n"));
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(diagnostics.len(), 1, "{call}: {diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), expected, "{call}");
            assert!(diagnostics[0].range().unwrap().start().to_usize() >= source.len());
        }
    }

    #[test]
    fn qualified_provider_comments_keep_nominal_identity() {
        let source = r#"
First = provider(fields=["value"])
Second = provider(fields=["value"])
api = struct(Info=First)
def consume(value):
    # type: (api.Info) -> api.Info
    return value
accepted = consume(First(value=1)) # type: api.Info
"#;
        let (mut analysis, fixture) = native_analysis(source);
        let file = fixture.main_file();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        analysis.update_file(file, format!("{source}\nconsume(Second(value=1))\n"));
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].id().as_str(), "invalid-argument-type");
    }

    #[test]
    fn collection_operators_keep_starlark_contracts() {
        let source = r#"
items = [1] + ["x"] # type: list[int | string]
mapping = {"x": 1} | {2: "y"} # type: dict[int | string, int | string]
forward = "x" * 2 # type: string
reverse = 2 * "x" # type: string
repeated = 2 * [1] # type: list[int]
"example".startswith(("e", "x"))
"#;
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        let file = fixture.main_file();
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        for (statement, expected) in [
            ("narrow = items # type: list[int]", "invalid-assignment"),
            (
                "narrow = mapping # type: dict[string, int]",
                "invalid-assignment",
            ),
            ("narrow = reverse # type: int", "invalid-assignment"),
            (
                "\"example\".startswith((\"e\", 1))",
                "invalid-argument-type",
            ),
        ] {
            analysis.update_file(file, format!("{source}\n{statement}\n"));
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(diagnostics.len(), 1, "{statement}: {diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), expected, "{statement}");
        }
    }

    #[test]
    fn flow_reachability_is_invalidated_after_edits() {
        let options = InferenceOptions {
            use_code_flow_analysis: true,
            ..Default::default()
        };
        let mut analysis = Analysis::with_system(
            Arc::new(SimpleFileLoader::default()),
            options,
            ruff_db::system::InMemorySystem::default(),
        );
        let original = "fail()\nx = 1\n";
        let (fixture, _) = Fixture::from_single_file(&mut analysis.db, original);

        for (source, unreachable) in [
            (original, true),
            ("str()\nx = 1\n", false),
            (original, true),
        ] {
            analysis.update_file(fixture.main_file(), source.to_owned());

            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            assert_eq!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.headline_message() == "Code is unreachable"),
                unreachable,
                "{source}: {diagnostics:?}",
            );
        }
    }

    #[test]
    fn incomplete_headers_do_not_capture_later_statements() {
        for source in [
            "def broken()\nx = {\"k\": 1}\ny = 2\n",
            "if x\n    pass\ny = {\"a\": 1}\n",
            "if x:\nif y:\n    pass\nz = 1\n",
        ] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(source);
            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            assert!(!diagnostics.is_empty(), "{source}");
        }
    }
    #[test]
    fn type_ignore_only_suppresses_inference() {
        for input in ["\"x\" + 1", "fail(msg = \"x\")"] {
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(input);
            let file = fixture.main_file();
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert!(!diagnostics.is_empty(), "{input}");
            assert!(diagnostics
                .iter()
                .all(|diagnostic| diagnostic.id().is_lint()));
            analysis.update_file(file, format!("{input} # type: ignore\n"));
            assert!(analysis.snapshot().diagnostics(file).unwrap().is_empty());
        }
        let (analysis, fixture) = Analysis::from_single_file_fixture("1 = 2 # type: ignore\n");
        let diagnostics = analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap();
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.is_invalid_syntax()));

        for (source, suppressed) in [
            ("\"x\" + ( # type: ignore\n  1\n)\n", true),
            ("\"x\" + (\n  1\n) # type: ignore\n", true),
            ("\"x\" + (\n  1 # type: ignore\n)\n", false),
        ] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(source);
            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            assert_eq!(
                diagnostics.is_empty(),
                suppressed,
                "{source}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn unused_definitions_keep_starlark_visibility_policy() {
        let source = "public = 1\nvalues = [0 for item in [1]]\n_private = 1\n_ = 1\ndef public_function(parameter):\n    local = 1\n    _local = 1\n    used = 1\n    return used\ndef _private_function():\n    pass\n";
        for allow_unused_definitions in [false, true] {
            let mut analysis = Analysis::with_system(
                Arc::new(SimpleFileLoader::default()),
                InferenceOptions {
                    allow_unused_definitions,
                    ..Default::default()
                },
                ruff_db::system::InMemorySystem::default(),
            );
            let (fixture, _) = Fixture::from_single_file(&mut analysis.db, source);
            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            let mut names: Vec<_> = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.id().as_str() == "unused-definition")
                .map(|diagnostic| source[diagnostic.range().unwrap()].to_owned())
                .collect();
            names.sort();
            if allow_unused_definitions {
                assert!(names.is_empty(), "{diagnostics:?}");
            } else {
                assert_eq!(
                    names,
                    ["_local", "_private", "_private_function", "item", "local"]
                );
            }
            let used = source.replace("[0 for item", "[item for item").replace(
                "    return used",
                "    print(local, _local)\n    return used",
            ) + "\nprint(_private)\n_private_function()\n";
            analysis.update_file(fixture.main_file(), used);
            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.id().as_str() != "unused-definition"),
                "{diagnostics:?}"
            );
        }
    }

    #[test]
    fn host_diagnostics_share_type_ignore_suppression() {
        for source in [
            "_unused = 1 # type: ignore\n",
            "load(\"missing.bzl\") # type: ignore\n",
            "load(\"missing.bzl\", \"value\") # type: ignore\n",
            "def f():\n    return\n    print(1) # type: ignore\n",
        ] {
            let mut analysis = Analysis::with_system(
                Arc::new(SimpleFileLoader::default()),
                InferenceOptions {
                    use_code_flow_analysis: true,
                    ..Default::default()
                },
                ruff_db::system::InMemorySystem::default(),
            );
            let (fixture, _) = Fixture::from_single_file(&mut analysis.db, source);
            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| !diagnostic.id().is_lint()),
                "{source}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn invalid_comment_types_keep_the_original_ranges() {
        let source = "value = 1 # type: missing_assignment\ndef function(\n    parameter, # type: missing_parameter\n):\n    pass\n";
        let (analysis, fixture) = Analysis::from_single_file_fixture(source);
        let diagnostics = analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap();
        let mut unresolved: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.id().as_str() == "unresolved-reference")
            .map(|diagnostic| &source[diagnostic.range().unwrap()])
            .collect();
        unresolved.sort();
        assert_eq!(
            unresolved,
            ["missing_assignment", "missing_parameter"],
            "{diagnostics:?}"
        );
    }

    #[test]
    fn flow_and_unused_definitions_use_shared_binding_facts() {
        let source = "def outer(flag):\n    value = 1\n    value = 2\n    left, (middle, right) = 1, (2, 3)\n    def helper():\n        return value\n    helper()\n    if flag:\n        maybe = 1\n    print(maybe)\n    for item in [1]:\n        break\n        print(\"break\")\n    for item in [1]:\n        continue\n        print(\"continue\")\n    return left\n";
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
        for use_code_flow_analysis in [false, true, false, true] {
            analysis
                .db
                .environment()
                .set_options(&mut analysis.db)
                .to(InferenceOptions {
                    use_code_flow_analysis,
                    ..Default::default()
                });
            let diagnostics = analysis
                .snapshot()
                .diagnostics(fixture.main_file())
                .unwrap();
            let mut unused: Vec<_> = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.id().as_str() == "unused-definition")
                .map(|diagnostic| &source[diagnostic.range().unwrap()])
                .collect();
            unused.sort();
            assert_eq!(
                unused,
                ["item", "item", "middle", "right", "value"],
                "{diagnostics:?}"
            );
            assert_eq!(
                diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.id().as_str() == "possibly-unresolved-reference")
                    .count(),
                usize::from(use_code_flow_analysis),
                "{diagnostics:?}"
            );
            let unreachable: Vec<_> = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.id().as_str() == "unreachable-code")
                .map(|diagnostic| &source[diagnostic.range().unwrap()])
                .collect();
            let expected = if use_code_flow_analysis {
                vec!["print(\"break\")", "print(\"continue\")"]
            } else {
                Vec::new()
            };
            assert_eq!(unreachable, expected, "{diagnostics:?}");
        }
    }

    #[test]
    fn deprecated_arguments_follow_the_intrinsic_declaration() {
        let (analysis, fixture) = Analysis::from_single_file_fixture(
            "abort = fail\ndef first():\n    abort(msg = \"old\", attr = \"field\")\ndef second():\n    fail(\"message\", sep = \" \")\ndef user_fail(msg):\n    return msg\ndef third():\n    fail = user_fail\n    fail(msg = \"local\")\n",
        );
        let diagnostics = analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap();
        let deprecated: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.id().as_str() == "deprecated-argument")
            .collect();
        assert_eq!(deprecated.len(), 2, "{diagnostics:?}");
        assert!(deprecated
            .iter()
            .all(|diagnostic| diagnostic.severity() == ruff_db::diagnostic::Severity::Info));

        let (mut analysis, _) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        fixture.add_prelude_file(&mut analysis.db, "def fail(msg):\n    return msg\n");
        let build = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD",
            "fail(msg = \"from prelude\")\n",
            starpls_common::Dialect::Bazel,
            Some(starpls_common::FileInfo::Bazel {
                api_context: starpls_bazel::APIContext::Build,
                is_external: false,
            }),
        );
        let diagnostics = analysis.snapshot().diagnostics(build).unwrap();
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.id().as_str() != "deprecated-argument"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn syntax_limit_does_not_limit_inference() {
        let input = "if True: pass\n".repeat(129) + "unknown\n";
        let (analysis, fixture) = Analysis::from_single_file_fixture(&input);
        let diagnostics = analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap();
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.is_invalid_syntax())
                .count(),
            128
        );
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.id().is_lint())
                .count(),
            1
        );
    }

    #[test]
    fn renderer_uses_snapshot_source() {
        let (analysis, fixture) = Analysis::from_single_file_fixture("value = 1 # type: string\n");
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(fixture.main_file()).unwrap();
        let rendered = snapshot
            .render_diagnostics(
                &diagnostics,
                &ruff_db::diagnostic::DisplayDiagnosticConfig::new("starpls"),
            )
            .unwrap();
        expect_test::expect![[r#"
            error[invalid-assignment]: Object of type `Literal[1]` is not assignable to `str`
             --> main.bzl:1:9
              |
            1 | value = 1 # type: string
              |         ^         ------ Declared type
              |         |
              |         Incompatible value of type `Literal[1]`

        "#]]
        .assert_eq(&rendered);
    }
}
