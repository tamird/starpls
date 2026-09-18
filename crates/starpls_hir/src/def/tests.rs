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
fn source_ranges_remain_distinct_during_edits() {
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
            assert_eq!(map.expr_map.len(), map.expr_map_back.len(), "{input}");
            assert_eq!(map.stmt_map.len(), map.stmt_map_back.len(), "{input}");
            assert_eq!(map.param_map.len(), map.param_map_back.len(), "{input}");
            assert_eq!(
                map.load_item_map.len(),
                map.load_item_map_back.len(),
                "{input}"
            );
        }
    }
}
