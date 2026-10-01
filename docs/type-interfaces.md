# Starlark stubs

A *stub file* (`.bzli`) declares types for a Starlark module (`.bzl`) or variables
in a BUILD file. A *stub package* contains stub files and a manifest mapping
them to source files.

## Stub files

```starlark
DEFAULT_TIMEOUT: int

def fetch(name: string, timeout: int = ...) -> list[string]: ...
```

A stub consists of variable annotations, function declarations, provider,
TypedDict and protocol classes, loads, docstrings, and placeholders. Variable
initializers and parameter defaults are omitted or `...`. A function body
consists of an optional docstring followed by `...` or `pass`. Public variables,
functions, and classes declared in the stub define its exports; loaded names
are available in annotation expressions.

For each name loaded from a mapped `.bzl` module, Starpls uses the stub declaration
when present and source inference otherwise. Annotations are resolved in the
stub's scope. Stubs are trusted contracts; implementation validation is a
separate check.

### Overloaded functions

Use `@overload` on consecutive declarations with the same name to describe
multiple call signatures. `overload` is available in the annotation namespace:

```starlark
@overload
def identity(value: int) -> int: ...

@overload
def identity(value: str) -> str: ...
```

Calls use the complete overload family. Each declaration has its own parameter
and result annotations and may declare type parameters. Overload families must
contain at least two declarations. Ty checks the declarations and resolves calls
using its ordinary overload rules.

### Shared declarations

Stub loads accept `.bzl` and `.bzli` labels. For example, `shared.bzli` can
declare a public protocol:

```starlark
class Builder(Protocol):
    def build(self) -> str: ...
```

Other stubs can import it under a private local name:

```starlark
load(":shared.bzli", _Builder = "Builder")

def builder() -> _Builder: ...
```

## BUILD annotations

An explicit mapping such as `BUILD.bazel=BUILD.bzli` applies variable annotations
to the BUILD file's module assignments, including private names:

```starlark
class _Case(TypedDict):
    name: str
    enabled: bool

_CASES: list[_Case]
```

Each annotated variable requires one direct assignment to that name in the BUILD
file and one declaration in the stub. Missing, unsupported, and ambiguous
bindings are errors. Helper classes and loads supply annotation types; top-level
function declarations are unsupported. Source type comments take precedence.
Ty checks the initializer, subsequent uses, and mutations against the annotation
in the BUILD host context.

Implementation validation applies to `.bzl` contracts. BUILD annotations
participate in ordinary source checking.

## Providers

A provider export is declared as a class with readonly fields and an explicit
constructor:

```starlark
class FilesInfo:
    files: Final[depset[File]]

    def __init__(self, *, files: depset[File]) -> None: ...
```

`Final[T]` declares a field with value type `T`. Every declared field is present
on an instance. `T | None` permits a `None` value. Constructors declare accepted
arguments using ordinary function annotations. A provider class contains field
annotations, `__init__`, docstrings, and placeholders; its identity is distinct
from every other provider class. Field and constructor annotations resolve in
the stub scope, including references to the provider itself.

Starpls follows source aliases and reexports to pair the class with a unique
`provider(...)` declaration. The paired class supplies the nominal identity for
source instances, callers, annotations, and `Target` lookups. Multiple distinct
classes claiming the same declaration are an error. A stub may expose a subset
of the fields allowed by the source provider.

Validation of plain provider calls compares each stored value with its declared
readonly type using the output constraints described below. Callback input
domains and mutable nested storage must satisfy their declared requirements.

For a provider with an initializer, `__init__` describes the initializer's public
arguments. An exported raw constructor has its own declaration:

```starlark
class FilesInfo:
    files: Final[depset[File]]

    def __init__(self, files: list[File]) -> None: ...

def raw_files(*, files: depset[File]) -> FilesInfo: ...
```

Raw constructors accept field values directly. Each declared field is a required
keyword argument. The source raw binding and the stub return type identify the
same provider.

## Dictionaries

A `TypedDict` describes string-keyed dictionaries whose values have different
types:

```starlark
class _Artifact(TypedDict):
    path: str
    checksum: NotRequired[str]

ARTIFACTS: list[_Artifact]

def artifact() -> _Artifact: ...
```

Fields are required unless annotated with `NotRequired[T]`. A field of type
`T | None` permits a `None` value; `NotRequired[T]` permits an absent key. Ty
checks field values and required keys, and preserves their types through
indexing, `get`, and keyword expansion. Private helper classes describe the
dictionary values of source exports.

`class _Artifact(TypedDict, closed=True)` limits keys to the declared fields.
The default open form permits additional fields when accepting existing values.
`extra_items=ReadOnly[object]` also accepts additional fields in literal
initializers. Their values have type `object`, and access through the contract
permits reads. Declared fields retain their individual types and mutability.

`ExactDict[K, V]` describes dictionaries allocated by dictionary literals,
comprehensions, `dict()`, and dictionary union. It preserves the same invariant
key and value types as `dict[K, V]` and can carry allocation information through
callback parameters and results. Ordinary `dict` and `TypedDict` annotations
cover dictionaries supplied by Bazel as well as these allocations. A native
`type(value) == "dict"` check identifies that broader category.

## Selectors

`select[T]` describes configurable values whose branches have type `T`.
Starpls also tracks the first branch's runtime kind when its value is a
supported literal and the dictionary keys are distinct string literals. It
also recognizes mappings whose values all have one runtime category: strings,
booleans, `None`, or lists and tuples. For these mappings, every possible first
branch has the same kind. The kind determines which selector operations Bazel
accepts. For example, selectors whose first branch is `None` combine with `+`,
including when later branches are
dictionaries; selectors whose first branch is a dictionary combine with `|`.

Inferred types display this information as a second parameter, such as
`select[dict[str, int] | None, Literal["none"]]`. List and tuple branches share
the `"list"` kind. Combined selectors preserve their kind and include the
nullable contributions of both operands in their branch type.

`select[T]` defaults the kind to `Any`, so annotations can describe branch
values while leaving runtime kind unspecified. Mappings with unknown or mixed
value categories retain an unspecified kind, as do integer and dictionary value
types with several native representations. Integer literals retain the `"int"`
category, and literal dictionary branches retain the `"dict"` kind. Bazel uses
several native integer representations, so integer operands carry an `Unknown`
compatibility requirement. Implementation validation refuses to prove operations
on integer-first selectors until their native representations can be distinguished.

A predicate testing for any selector uses `TypeIs[select[object, object]]`.
The second `object` covers every kind, so both outcomes of the predicate can
narrow its argument. A gradual kind describes an unspecified subset instead.

Bazel 9.2 distinguishes dictionary implementations that share Starlark's
`dict` type. An `ExactDict` branch establishes the `"dict"` selector kind, and
an `ExactDict` operand supports `|` with selectors of that kind. Operations
on a broadly annotated dictionary retain their ordinary inferred result;
implementation validation requires the allocation information as well.

## Rule contexts

`ctx[Value, Attrs]` specifies the build-setting value and the fields of
`ctx.attr`. A protocol describes the attribute fields a helper requires:

```starlark
class _Attrs(Protocol):
    @property
    def exports(self) -> Sequence[Target]: ...

def exported(context: ctx[object, _Attrs]) -> Sequence[Target]: ...
```

These contexts retain their native methods and work with native functions.
`ctx` and `ctx[Value]` use the native open struct view for attributes.
With context inference enabled, eligible rule and aspect registrations supply
schemas from their attribute descriptors.

## Protocols

A protocol describes values by their fields and methods:

```starlark
class _Builder(Protocol):
    def set(self, value: int) -> _Builder: ...
    def build(self) -> str: ...

def builder() -> _Builder: ...
```

Protocol bases are names resolved in the stub's scope. Method signatures use
ordinary function annotations and declaration bodies. Ty checks compatibility
structurally. Private helper protocols describe return values and parameters
without declaring a corresponding source export.

Readonly properties describe fields that callers can read but cannot assign.
A named callback protocol preserves the keyword parameters of a callable field:

```starlark
class _Set(Protocol):
    def __call__(self, value: int) -> _Builder: ...

class _Builder(Protocol):
    @property
    def set(self) -> _Set: ...
```

Undecorated special methods for Starlark operations specify their call
signatures: for example, `__call__` describes `value(...)`, `__getitem__`
describes indexing, and `__len__` describes `len(value)`. The same rule applies
to iteration, containment, conversions, and Starlark unary and binary operators.
A readonly property specifies an immutable stored field, including callable
fields and fields named `__call__`.
Undecorated attribute interception and Python class lifecycle methods use
ordinary member requirements.

An explicit `@type_check_only` method supplies the signature used to check an
operation. Explicit runtime attribute access follows the value's fields. A
partial namespace combines named fields with gradual access through other
names:

```starlark
class _Namespace(Protocol):
    @property
    def known(self) -> int: ...

    @type_check_only
    def __getattr__(self, name: str) -> Any: ...
```

Named properties establish the presence of required fields. The getter bounds
values from successful lookups, and `getattr` includes a supplied default in its
result.
Other method declarations require the member on the value's class. Interface
methods accept one bare `@property` or `@type_check_only` decorator.

`struct[T]` bounds the values of existing fields. Required protocol fields need
independent presence evidence, such as explicit constructor keywords or required
keys in an unpacked dictionary. Every Starlark value is assignable to `object`;
attribute access requires a more specific type.

## Packaging

Projects obtain source and stub repositories through Bazel dependencies and
select stub packages in `starpls.toml` at the workspace root:

```toml
[[stub-packages]]
manifest = "@rules_foo_stubs//:stubs.toml"
```

Manifest labels use the main repository's mapping. A local manifest can be
specified by a path relative to the configuration file, such as
`stubs/rules_foo/stubs.toml`.

The package manifest defines the source repository, supported versions, and
file mappings:

```toml
# stubs.toml
format-version = 1

[source]
repository = "@rules_foo"
module = "rules_foo"
versions = ["1.4.0", "1.4.1"]

[files]
"foo/defs.bzl" = "foo/defs.bzli"
```

The package declares the Bazel dependencies referenced by its manifest and stub
files. `source.repository` resolves in the package's repository mapping.
`files` maps source paths, relative to the source repository root, to stub paths
relative to the manifest directory. Paths identify installed files within their
respective repositories, including files and directories mounted by symlinks.

## Resolution

Registrations identify a canonical Bazel repository and source file. Each
selected repository instance has its own registrations. Loads within a stub use
its repository's mapping; a load of its mapped implementation resolves the
source definitions.

Each source file may have one registered stub. Packages covering different
files compose. Multiple registrations for the same file are a configuration
error, including identical mappings or stubs declaring different exports.
This rule applies to package manifests and direct CLI mappings. The error
identifies the source file and both registrations.

## Version compatibility

`format-version` identifies the manifest schema. Stub packages have independent
Bazel release versions. `source.versions` is a nonempty list of accepted source
module versions.

Bazel's selected module name must equal `source.module`, and its selected
version must appear in `source.versions`. Bazel dependency declarations specify
minimum versions; compatibility is checked against the resolved module graph.

For a source with no selected module version, a `stub-packages` entry may set
`allow-unversioned = true`. Any available module name must match `source.module`.
Otherwise, an unverifiable version is a configuration error.

Incompatible versions, unsupported manifest schemas, unknown fields, unresolved
repositories, unreadable files, and failed Bazel queries are configuration errors.
