use std::collections::HashSet;

use starpls_bazel::env::make_build_builtins;
use starpls_bazel::env::make_bzl_builtins;
use starpls_bazel::APIContext;
use starpls_common::Dialect;
use starpls_common::FileInfo;
use starpls_test_util::FixtureFile;

use crate::def::resolver::Resolver;
use crate::test_database::TestDatabase;
use crate::typeck::intrinsics::intrinsic_functions;
use crate::Db as _;

fn check_scope(fixture: &str, expected: &[&str]) {
    check_scope_full(fixture, expected, None)
}

fn check_scope_full(fixture: &str, expected: &[&str], prelude: Option<&str>) {
    let mut test_db: TestDatabase = Default::default();
    let fixture = FixtureFile::parse(fixture);
    let file = starpls_common::open_document(
        &mut test_db,
        std::path::Path::new("BUILD"),
        Dialect::Bazel,
        Some(FileInfo::Bazel {
            api_context: APIContext::Build,
            is_external: false,
        }),
        fixture.contents,
        0,
    )
    .unwrap();

    if let Some(prelude) = prelude {
        let prelude_file_id = starpls_common::open_document(
            &mut test_db,
            std::path::Path::new("prelude_bazel"),
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Prelude,
                is_external: false,
            }),
            prelude.to_string(),
            0,
        )
        .unwrap();
        test_db.set_bazel_prelude_file(prelude_file_id);
    }

    // Filter out intrinsic function names as well as the hardcoded `BUILD.bazel` and `.bzl`
    // builtins, which are always added when `APIContext::Build` is the current API context.
    let names_to_filter = intrinsic_functions(&test_db)
        .functions
        .keys()
        .map(|name| name.to_string())
        .chain(
            make_bzl_builtins()
                .global
                .into_iter()
                .map(|global| global.name),
        )
        .chain(
            make_build_builtins()
                .global
                .into_iter()
                .map(|global| global.name),
        )
        .collect::<HashSet<_>>();

    let resolver = Resolver::new_for_offset(&test_db, file, fixture.cursor_pos.unwrap());
    let names = resolver.names();
    let mut actual = names
        .keys()
        .filter(|name| !names_to_filter.contains(name.as_str()))
        .map(|name| name.as_str())
        .collect::<Vec<_>>();
    actual.sort();
    assert_eq!(expected, &actual[..]);
}

#[test]
fn smoke_test() {
    check_scope(
        r"
g = 0
def foo():
    x = 1
    y = 2
    $0

def bar():
    pass
",
        &["bar", "foo", "g", "x", "y"],
    )
}

#[test]
fn test_empty_scope() {
    check_scope(
        r"
        $0
    ",
        &[],
    )
}

#[test]
fn test_assign() {
    check_scope(
        r"
a = 0
b, c = 1, 2
d, e = 3, 4
[f, g] = 5, 6
$0
        ",
        &["a", "b", "c", "d", "e", "f", "g"],
    )
}

#[test]
fn test_params() {
    check_scope(
        r"
def foo(x, *args, **kwargs):
    print(x)
    $0
",
        &["args", "foo", "kwargs", "x"],
    )
}

#[test]
fn test_loop_variables() {
    check_scope(
        r"
for x, y in 1, 2, 3:
    print(x, y)
    $0
    ",
        &["x", "y"],
    )
}

#[test]
fn test_lambda() {
    check_scope(
        r"
a = 1
f = lambda x: x + 1$0
    ",
        &["a", "x"],
    )
}

#[test]
fn test_def() {
    check_scope(
        r"def foo():
    x = 1
$0",
        &["foo", "x"],
    )
}

#[test]
fn test_list_comprehension() {
    check_scope(
        r"
[x*y$0 for x in range(5) for y in range(5)]
        ",
        &["x", "y"],
    )
}

#[test]
fn test_list_comprehension_clause1() {
    check_scope(
        r"
[x*y for x in range(5) for y in range(5) if x*y$0 > 10] 
        ",
        &["x", "y"],
    )
}

#[test]
fn test_list_comprehension_clause2() {
    check_scope(
        r"
[x*y for x in range(5) if x$0yz > 2 for y in range(5) if x*y > 10]
        ",
        &["x"],
    )
}

#[test]
fn test_load() {
    check_scope(
        r#"
load("foo.star", "go_binary")
$0
    "#,
        &["go_binary"],
    )
}

#[test]
fn test_param_defaults() {
    check_scope(
        r#"
_tsc = ""

def ts_project(tsc = _t$0sc):
    pass
    "#,
        &["_tsc"],
    )
}

#[test]
fn test_prelude() {
    check_scope_full(
        r#"
foo = 123
$0   
"#,
        &["bar", "f", "foo"],
        Some(
            r#"
bar = "abc"

def f():
    pass
"#,
        ),
    )
}

#[test]
fn native_nodes_follow_recovered_syntax() {
    let mut db = TestDatabase::default();
    let file = starpls_common::open_document(
        &mut db,
        std::path::Path::new("main.bzl"),
        Dialect::Bazel,
        Some(FileInfo::Bazel {
            api_context: APIContext::Bzl,
            is_external: false,
        }),
        String::new(),
        0,
    )
    .unwrap();
    for source in [
        "x = ((a + b))\ny = a and b or c\nz = a < b < c\n",
        "def f(x=(1+2), *, y=0, **kwargs):\n    # type: (int) -> int\n    return x\n",
        "[x for (x,) in xs if x for y in ys if y]\n",
        "load(\":defs.bzl\", alias=\"name\", \"other\")\nf(x, key=(y))\n",
        "x = {\"😀\": lambda a: a}\nif x:\n    pass\nelif y:\n    z = 1\n",
        "def broken()\nx = {\"k\": 1}\ny = 2\n",
    ] {
        let prefixes = source
            .char_indices()
            .map(|(offset, _)| source[..offset].to_owned());
        let deletions = source.char_indices().map(|(offset, character)| {
            let mut edited = source.to_owned();
            edited.replace_range(offset..offset + character.len_utf8(), "");
            edited
        });
        for input in std::iter::once(source.to_owned())
            .chain(prefixes)
            .chain(deletions)
        {
            starpls_common::update_file(&mut db, file, input.clone());
            let map = crate::source_map(&db, file);
            let parsed = starpls_common::parsed_module(&db, file).load(&db);
            for (node, expr) in &map.expr_nodes {
                use ruff_text_size::Ranged;
                let native = parsed.get_by_index(*node).range();
                let lowered = map.expr_map_back[expr];
                assert!(
                    u32::from(lowered.start()) <= u32::from(native.start())
                        && u32::from(native.end()) <= u32::from(lowered.end()),
                    "{input}"
                );
            }
        }
    }
}

#[test]
fn native_declarations_preserve_editor_ranges() {
    use starpls_syntax::ast::AstNode;
    use starpls_syntax::ast::{self};
    let source = "def f(x=(1+2), *, y=0):\n    if x:\n        pass\n    elif y:\n        pass\n    else:\n        pass\n        # trailing suite comment\n\nnext = f\n";
    let mut db = TestDatabase::default();
    let file = starpls_common::open_document(
        &mut db,
        std::path::Path::new("main.bzl"),
        Dialect::Standard,
        None,
        source.to_owned(),
        0,
    )
    .unwrap();
    let map = crate::source_map(&db, file);
    let tree = starpls_common::parse(&db, file).syntax();
    for node in tree.descendants() {
        if (node.kind() == starpls_syntax::SyntaxKind::IF_STMT
            || node.parent().is_some_and(|parent| {
                matches!(
                    parent.kind(),
                    starpls_syntax::SyntaxKind::MODULE | starpls_syntax::SyntaxKind::SUITE
                )
            }))
            && ast::Statement::cast(node.clone()).is_some()
        {
            assert!(
                map.stmt_map_back
                    .values()
                    .any(|range| *range == node.text_range()),
                "{} at {:?}",
                node,
                node.text_range()
            );
        }
        if ast::Parameter::cast(node.clone()).is_some() {
            assert!(
                map.param_map_back
                    .values()
                    .any(|range| *range == node.text_range()),
                "{node}"
            );
        }
    }
}

#[test]
fn native_type_comments_keep_attachment_boundaries() {
    use crate::def::Stmt;
    use crate::def::TypeCommentOwner;
    let source = r#"a = 1; b = 2 # type: string

def f(
    x = (
        1 # type: bool
    ),
    # ordinary comment

    # type: int
    *, # type: float
    y, # type: string
):
    # type: bool
    # type: (string, string, string) -> int
    pass
"#;
    let mut db = TestDatabase::default();
    let file = starpls_common::open_document(
        &mut db,
        std::path::Path::new("main.bzl"),
        Dialect::Standard,
        None,
        source.to_owned(),
        0,
    )
    .unwrap();
    let info = crate::lower(&db, file);
    let mut owners = info
        .source_map
        .type_comment_owners
        .iter()
        .map(|(range, owner)| {
            let name = match owner {
                TypeCommentOwner::Statement(stmt) => match &info.module.stmts[*stmt] {
                    Stmt::Def { func, stmts: _ } => {
                        assert!(
                            func.ret_type_ref.is_none(),
                            "later specification must not replace the first comment"
                        );
                        func.name.as_str()
                    }
                    Stmt::Assign {
                        lhs,
                        rhs: _,
                        op: _,
                        type_ref: _,
                    } => {
                        let crate::def::Expr::Name { name } = &info.module.exprs[*lhs] else {
                            panic!("expected assignment name");
                        };
                        name.as_str()
                    }
                    other => panic!("unexpected comment owner {other:?}"),
                },
                TypeCommentOwner::Parameter(param) => info.module.params[*param].name().as_str(),
            };
            (
                u32::from(range.start()),
                &source[usize::from(range.start())..usize::from(range.end())],
                name,
            )
        })
        .collect::<Vec<_>>();
    owners.sort_by_key(|(start, _, _)| *start);
    let owners = owners
        .into_iter()
        .map(|(_, comment, owner)| (comment, owner))
        .collect::<Vec<_>>();
    assert_eq!(
        owners,
        [
            ("# type: string", "b"),
            ("# type: int", "x"),
            ("# type: float", "[missing name]"),
            ("# type: string", "y"),
            ("# type: bool", "f"),
        ]
    );
}

#[test]
fn native_operators_and_recovered_slots() {
    use starpls_syntax::ast::AssignOp;
    use starpls_syntax::ast::BitwiseAssignOp;

    use crate::def::Expr;
    use crate::def::Stmt;
    let mut db = TestDatabase::default();
    let source = "a <<= 1\nb >>= 2\nvalues = [1, *unsupported, 2]\na = b = c\n";
    let file = starpls_common::open_document(
        &mut db,
        std::path::Path::new("main.bzl"),
        Dialect::Standard,
        None,
        source.to_owned(),
        0,
    )
    .unwrap();
    let module = crate::module(&db, file);
    let [left, right, list, chained] = module.top_level.as_ref() else {
        panic!("expected four assignments");
    };
    for (stmt, expected) in [(left, BitwiseAssignOp::Shl), (right, BitwiseAssignOp::Shr)] {
        let Stmt::Assign {
            lhs: _,
            rhs: _,
            op,
            type_ref: _,
        } = module[*stmt]
        else {
            panic!("expected assignment");
        };
        assert_eq!(op, Some(AssignOp::Bitwise(expected)));
    }
    let Stmt::Assign {
        lhs: _,
        rhs,
        op: _,
        type_ref: _,
    } = module[*list]
    else {
        panic!("expected assignment");
    };
    let Expr::List { exprs } = &module[rhs] else {
        panic!("expected list");
    };
    let [_, missing, _] = exprs.as_ref() else {
        panic!("unsupported element must retain its slot");
    };
    assert_eq!(module[*missing], Expr::Missing);
    let Stmt::Assign {
        lhs: _,
        rhs,
        op: _,
        type_ref: _,
    } = module[*chained]
    else {
        panic!("expected assignment");
    };
    let Expr::Name { name } = &module[rhs] else {
        panic!("expected chained assignment value");
    };
    assert_eq!(name.as_str(), "c");
}

#[test]
fn native_function_docs_select_direct_literals() {
    let mut db = TestDatabase::default();
    let file = starpls_common::open_document(
        &mut db,
        std::path::Path::new("main.bzl"),
        Dialect::Standard,
        None,
        String::new(),
        0,
    )
    .unwrap();
    for (body, expected) in [
        ("    (\"ignored\")\n    \"actual\"\n", Some("actual")),
        ("    (1)\n    \"actual\"\n", Some("actual")),
        (
            "    \"unsupported\" \"concatenation\"\n    \"actual\"\n",
            Some("actual"),
        ),
        ("    1\n    \"ignored\"\n", None),
        ("    pass\n    \"actual\"\n", Some("actual")),
    ] {
        starpls_common::update_file(&mut db, file, format!("def f():\n{body}"));
        let module = crate::module(&db, file);
        let [stmt] = module.top_level.as_ref() else {
            panic!("expected a function");
        };
        let crate::def::Stmt::Def { func, stmts: _ } = &module[*stmt] else {
            panic!("expected a function");
        };
        assert_eq!(func.doc.as_deref(), expected, "{body}");
    }
}
