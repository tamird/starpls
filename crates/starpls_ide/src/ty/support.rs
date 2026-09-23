//! Fixed language declarations in the canonical Ty support universe.
//!
//! Replace only the owned classes in pinned typeshed inputs, retaining imports,
//! support classes, generic identities, and Ty's ordinary inference machinery.
//! These are immutable database inputs, assembled before any file is interned.
//! User source is never rewritten. Boolean values have their own nominal type;
//! the remaining numeric operator declarations retain the pinned Python contracts.
//! Hiding their presentation does not establish full operator compatibility.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use anyhow::bail;
use anyhow::Context;
use ruff_db::vendored::FileType;
use ruff_db::vendored::VendoredFileSystem;
use ruff_db::vendored::VendoredFileSystemBuilder;
use ruff_db::vendored::VendoredPathBuf;
use ruff_python_ast as ast;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use zip::CompressionMethod;

const PRIMITIVES: &str = include_str!("primitives.pyi");
const COLLECTIONS: &str = include_str!("collections.pyi");
const SCALARS: [&str; 4] = ["int", "float", "tuple", "range"];

pub(crate) fn file_system() -> &'static VendoredFileSystem {
    static FILE_SYSTEM: LazyLock<VendoredFileSystem> = LazyLock::new(|| {
        build(ty_vendored::file_system()).expect("bundled Starlark support declarations are valid")
    });
    &FILE_SYSTEM
}

fn build(base: &VendoredFileSystem) -> anyhow::Result<VendoredFileSystem> {
    let mut replacements = BTreeMap::new();
    for (path, declarations, scalars) in [
        ("stdlib/builtins.pyi", PRIMITIVES, SCALARS.as_slice()),
        ("stdlib/typing.pyi", COLLECTIONS, [].as_slice()),
    ] {
        let original = base.read_to_string(path)?;
        let contents = assemble(&original, declarations, scalars).with_context(|| path)?;
        replacements.insert(path, contents);
    }
    let mut builder = VendoredFileSystemBuilder::new(CompressionMethod::Stored);
    let mut directories = vec![VendoredPathBuf::new()];
    while let Some(directory) = directories.pop() {
        for entry in base.read_directory(&directory) {
            match entry.file_type() {
                FileType::Directory => {
                    builder.add_directory(entry.path())?;
                    directories.push(entry.into_path());
                }
                FileType::File => {
                    let contents = match replacements.remove(entry.path().as_str()) {
                        Some(contents) => contents,
                        None => base.read_to_string(entry.path())?,
                    };
                    builder.add_file(entry.path(), &contents)?;
                }
            }
        }
    }
    anyhow::ensure!(
        replacements.is_empty(),
        "Missing support declaration entries"
    );
    let archive = builder.finish()?;
    Ok(archive)
}

fn assemble(original: &str, replacements: &str, scalars: &[&str]) -> anyhow::Result<String> {
    let replacement_module = ruff_python_parser::parse_module(replacements)?;
    let mut classes = BTreeMap::new();
    let mut imports = String::new();
    for statement in &replacement_module.syntax().body {
        let class = match statement {
            ast::Stmt::ClassDef(class) => class,
            ast::Stmt::ImportFrom(import) => {
                imports.push_str(&replacements[import.range()]);
                imports.push('\n');
                continue;
            }
            _ => bail!("Replacement declarations must contain only classes and imports"),
        };
        match classes.entry(class.name.as_str()) {
            Entry::Occupied(entry) => bail!("Duplicate replacement class {}", entry.key()),
            Entry::Vacant(entry) => {
                entry.insert(&replacements[decorated_range(class.range(), &class.decorator_list)]);
            }
        }
    }
    let original_module = ruff_python_parser::parse_module(original)?;
    let mut seen = BTreeMap::new();
    let mut edits = Vec::new();
    if !imports.is_empty() {
        let offset = original_module.syntax().body.first().and_then(|statement| {
            let ast::Stmt::Expr(expression) = statement else {
                return None;
            };
            matches!(expression.value.as_ref(), ast::Expr::StringLiteral(_))
                .then_some(expression.end())
        });
        if offset.is_some() {
            imports.insert(0, '\n');
        }
        let offset = offset.unwrap_or_default();
        edits.push((TextRange::empty(offset), imports));
    }
    for statement in &original_module.syntax().body {
        let ast::Stmt::ClassDef(class) = statement else {
            continue;
        };
        let name = class.name.as_str();
        if let Some(replacement) = classes.get(name) {
            count_owner(&mut seen, name)?;
            edits.push((
                decorated_range(class.range(), &class.decorator_list),
                (*replacement).to_owned(),
            ));
        } else if scalars.contains(&name) {
            count_owner(&mut seen, name)?;
            // Core Starlark values cannot be subclassed, even when their
            // retained Python declarations permit it.
            let is_final = class.decorator_list.iter().any(|decorator| {
                let ast::Expr::Name(name) = &decorator.expression else {
                    return false;
                };
                name.id == "final"
            });
            if !is_final {
                edits.push((TextRange::empty(class.start()), "@final\n".to_owned()));
            }
            scalar_members(original, &class.body, &mut edits)?;
        }
    }
    for name in classes.keys().copied().chain(scalars.iter().copied()) {
        if !seen.contains_key(name) {
            bail!("Missing top-level support class {name}");
        }
    }
    edits.sort_by_key(|(range, _)| range.start());
    let mut output = String::with_capacity(original.len());
    let mut offset = 0;
    for (range, replacement) in edits {
        let start = usize::from(range.start());
        let end = usize::from(range.end());
        anyhow::ensure!(start >= offset, "Overlapping support declaration edits");
        output.push_str(&original[offset..start]);
        output.push_str(&replacement);
        offset = end;
    }
    output.push_str(&original[offset..]);
    ruff_python_parser::parse_module(&output).context("Invalid assembled support declarations")?;
    Ok(output)
}

fn count_owner<'a>(seen: &mut BTreeMap<&'a str, ()>, name: &'a str) -> anyhow::Result<()> {
    match seen.entry(name) {
        Entry::Occupied(entry) => bail!("Ambiguous top-level support class {}", entry.key()),
        Entry::Vacant(entry) => {
            entry.insert(());
        }
    }
    Ok(())
}

fn decorated_range(range: TextRange, decorators: &[ast::Decorator]) -> TextRange {
    TextRange::new(
        decorators.first().map_or(range.start(), Ranged::start),
        range.end(),
    )
}

/// Keep pinned scalar operation declarations without exposing Python methods.
/// Fail on new declaration shapes so a typeshed update requires an owner audit.
fn scalar_members(
    source: &str,
    statements: &[ast::Stmt],
    edits: &mut Vec<(TextRange, String)>,
) -> anyhow::Result<()> {
    for statement in statements {
        match statement {
            ast::Stmt::FunctionDef(function) => {
                let range = decorated_range(function.range(), &function.decorator_list);
                let name = function.name.as_str();
                if name.starts_with("__") && name.ends_with("__") {
                    let start = usize::from(range.start());
                    let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
                    let indent = &source[line_start..start];
                    anyhow::ensure!(indent.trim().is_empty(), "Unsupported scalar member layout");
                    edits.push((
                        TextRange::empty(range.start()),
                        format!("@type_check_only\n{indent}"),
                    ));
                } else {
                    // Keep conditional suites syntactically nonempty.
                    edits.push((range, "pass".to_owned()));
                }
            }
            ast::Stmt::If(branch) => {
                scalar_members(source, &branch.body, edits)?;
                for clause in &branch.elif_else_clauses {
                    scalar_members(source, &clause.body, edits)?;
                }
            }
            ast::Stmt::Expr(expression) => {
                anyhow::ensure!(
                    matches!(expression.value.as_ref(), ast::Expr::StringLiteral(_)),
                    "Unexpected expression in scalar support class"
                );
            }
            _ => bail!("Unsupported declaration in scalar support class: {statement:?}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ruff_db::vendored::FileType;
    use ruff_db::vendored::VendoredPathBuf;
    use ruff_python_ast as ast;
    use starpls_common::Dialect;
    use ty_python_semantic::Db;
    use ty_python_semantic::HasType;
    use ty_python_semantic::SemanticModel;

    use super::assemble;
    use crate::Analysis;

    #[test]
    fn support_archive_preserves_base_entries() {
        let base = ty_vendored::file_system();
        let archive = super::file_system();
        let mut directories = vec![VendoredPathBuf::new()];
        let mut replacements = 0;
        while let Some(directory) = directories.pop() {
            let entries = base.read_directory(&directory).collect::<Vec<_>>();
            assert_eq!(
                entries,
                archive.read_directory(&directory).collect::<Vec<_>>()
            );
            for entry in entries {
                match entry.file_type() {
                    FileType::Directory => directories.push(entry.into_path()),
                    FileType::File => {
                        let original = base.read_to_string(entry.path()).unwrap();
                        let actual = archive.read_to_string(entry.path()).unwrap();
                        let expected = match entry.path().as_str() {
                            "stdlib/builtins.pyi" => {
                                replacements += 1;
                                assemble(&original, super::PRIMITIVES, &super::SCALARS).unwrap()
                            }
                            "stdlib/typing.pyi" => {
                                replacements += 1;
                                assemble(&original, super::COLLECTIONS, &[]).unwrap()
                            }
                            _ => original,
                        };
                        assert_eq!(actual, expected, "{}", entry.path());
                    }
                }
            }
        }
        assert_eq!(replacements, 2);
    }

    #[test]
    fn assembly_requires_unambiguous_declared_owners() {
        for source in ["", "class owned: ...\nclass owned: ...\n"] {
            assert!(assemble(source, "class owned: ...\n", &[]).is_err());
        }
        assert!(assemble(
            "class owned: ...\n",
            "class owned: ...\nclass owned: ...\n",
            &[]
        )
        .is_err());
        assert!(assemble("class scalar:\n    public: int\n", "", &["scalar"]).is_err());
        let source = r#"class scalar:
    @property
    def public(self): ...
    if True:
        def __add__(self, other): ...
    else:
        def python_only(self): ...

class unrelated:
    def preserved(self): ...
"#;
        let output = assemble(source, "", &["scalar"]).unwrap();
        assert!(!output.contains("public"), "{output}");
        assert!(!output.contains("python_only"), "{output}");
        assert!(
            output.contains("@type_check_only\n        def __add__"),
            "{output}"
        );
        assert!(output.contains("def preserved(self)"), "{output}");
    }

    #[test]
    fn starlark_primitives_use_shared_nominal_generics() {
        let source = r#"
def first(values):
    # type: (Sequence[int]) -> int
    return values[0]

def consume(values):
    # type: (Iterable[int]) -> None
    pass

values = [1, 2]
mapping = {"one": 1}
first(values)
first((1, 2))
first(range(2))
consume(values)
consume(mapping.values())
items = mapping.items()
keys = mapping.keys()
results = mapping.values()
elements = "abc".elems()
split = "a,b".split(sep=",", maxsplit=1)
reverse_split = "a,b".rsplit(sep=",", maxsplit=1)
offset = "abc".find("a", None, None)
compared = [1] < [2]
lookup = mapping.get
found = lookup("one", default="missing")
mapping.update([["two", 2]])
mapping.update([["two", 2]], three=3)
mapping.update(three=3)
copied = dict([["three", 3]])
copied_keywords = dict([["one", 1]], two=2)
mixed_keys = dict([(1, 2)], x=3)
append = values.append
append(3)
element = values.pop()
"#;
        let (mut analysis, _) = Analysis::new_for_test();
        let file = analysis
            .open_document(
                Path::new("/primitives.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let file = db.starlark_program_file(file);
        let model = SemanticModel::new(db, file);
        let module = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
        let mut actual = std::collections::BTreeMap::new();
        for statement in module.suite() {
            let ast::Stmt::Assign(assignment) = statement else {
                continue;
            };
            let [target] = assignment.targets.as_slice() else {
                panic!("expected one target");
            };
            let ast::Expr::Name(target) = target else {
                panic!("expected a name target");
            };
            let ty = assignment.value.inferred_type(&model).unwrap();
            actual.insert(
                target.id.as_str(),
                ty.display(db, &model.program_environment()).to_string(),
            );
        }
        for (name, expected) in [
            ("values", "list[int]"),
            ("mapping", "dict[str, int]"),
            ("items", "list[tuple[str, int]]"),
            ("keys", "list[str]"),
            ("results", "list[int]"),
            ("elements", "Sequence[str]"),
            ("split", "list[str]"),
            ("reverse_split", "list[str]"),
            ("offset", "int"),
            ("compared", "bool"),
            ("found", "int | Literal[\"missing\"]"),
            ("copied", "dict[int | str, int | str]"),
            ("copied_keywords", "dict[str | int, str | int]"),
            ("mixed_keys", "dict[str | int, int]"),
            ("element", "int"),
        ] {
            assert_eq!(
                actual.get(name).map(String::as_str),
                Some(expected),
                "{name}: {actual:?}"
            );
        }
        let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn booleans_are_ordered_without_numeric_operations() {
        let source = r#"
def integer(value):
    # type: (int) -> None
    pass

def boolean(value):
    # type: (bool) -> None
    pass

def inspect(flag):
    # type: (bool) -> None
    boolean(flag)
    boolean(True)
    integer(1)
    int(flag)
    float(flag)
    bool(0)
    False < True
    flag <= False
    True >= flag
    not flag
    flag and True
    boolean(1)
    integer(flag)
    integer(True)
    +flag
    ~True
    flag + 1
    True * 2
    flag & False
    [0][flag]
    "x" * True
    flag < 1
"#;
        let (mut analysis, _) = Analysis::new_for_test();
        let file = analysis
            .open_document(
                Path::new("/booleans.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let file = db.starlark_program_file(file);
        let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
        let mut rejected_lines: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                let start =
                    usize::from(diagnostic.primary_span().unwrap().range().unwrap().start());
                let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
                source[line_start..].lines().next().unwrap().trim()
            })
            .collect();
        rejected_lines.sort_unstable();
        let mut expected = [
            "boolean(1)",
            "integer(flag)",
            "integer(True)",
            "+flag",
            "~True",
            "flag + 1",
            "True * 2",
            "flag & False",
            "[0][flag]",
            "\"x\" * True",
            "flag < 1",
        ];
        expected.sort_unstable();
        assert_eq!(rejected_lines, expected, "{diagnostics:?}");
    }

    #[test]
    fn text_requires_explicit_element_iteration() {
        let source = r#"
def inspect(text, data):
    # type: (string, bytes) -> None
    for item in text:
        pass
    for item in "abc":
        pass
    for item in data:
        pass
    for item in text.elems():
        pass
    for item in "abc".elems():
        pass
    for item in data.elems():
        pass
    text[0]
    "abc"[0]
    data[0]
"#;
        let (mut analysis, _) = Analysis::new_for_test();
        let file = analysis
            .open_document(
                Path::new("/iteration.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let file = db.starlark_program_file(file);
        let model = SemanticModel::new(db, file);
        let module = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
        let [statement] = module.suite().as_slice() else {
            panic!("expected one statement");
        };
        let ast::Stmt::FunctionDef(function) = statement else {
            panic!("expected the inspected function");
        };
        let indexed: Vec<_> = function
            .body
            .iter()
            .filter_map(|statement| match statement {
                ast::Stmt::Expr(expression) => Some(
                    expression
                        .value
                        .inferred_type(&model)
                        .unwrap()
                        .display(db, &model.program_environment())
                        .to_string(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(indexed, ["str", "Literal[\"a\"]", "int"]);
        let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.id().to_string())
            .collect();
        assert_eq!(ids, ["not-iterable"; 3], "{diagnostics:?}");
        let mut rejected: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                let range = diagnostic.primary_span().unwrap().range().unwrap();
                &source[range]
            })
            .collect();
        rejected.sort_unstable();
        assert_eq!(rejected, ["\"abc\"", "data", "text"], "{diagnostics:?}");
    }

    #[test]
    fn canonical_mapping_and_sequence_keep_constraints() {
        // Support declarations use Python syntax and the same canonical nominal
        // universe as contextual source files and generated native declarations.
        let source = r#"
from collections.abc import Hashable, Iterable, Mapping, Sequence

def sequence(values: Sequence[int]) -> int:
    return values[0]

def mapping(values: Mapping[str, int]) -> int:
    return values["one"]

def iterable(values: Iterable[int]) -> None:
    pass

def hashable(value: Hashable) -> None:
    pass

sequence([1])
mapping({"one": 1})
iterable([1])
sequence(["bad"])
mapping({"one": "bad"})
iterable(["bad"])
hashable([1])
hashable({"one": 1})
"#;
        let (mut analysis, _) = Analysis::new_for_test();
        let file = analysis
            .open_document(
                Path::new("/support.py"),
                Dialect::Standard,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let file = db.program_file(file.source);
        let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
        let ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.id().to_string())
            .collect();
        assert_eq!(ids, ["invalid-argument-type"; 5], "{diagnostics:?}");
        let mut rejected_lines: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                let start =
                    usize::from(diagnostic.primary_span().unwrap().range().unwrap().start());
                let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
                source[line_start..].lines().next().unwrap()
            })
            .collect();
        rejected_lines.sort_unstable();
        assert_eq!(
            rejected_lines,
            [
                "hashable([1])",
                "hashable({\"one\": 1})",
                "iterable([\"bad\"])",
                "mapping({\"one\": \"bad\"})",
                "sequence([\"bad\"])",
            ],
            "{diagnostics:?}"
        );
    }
}
