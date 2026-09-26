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
Attribute interception and Python class lifecycle methods use ordinary member
requirements.

Other method declarations also require the member on the value's class. A property getter
uses the same declaration body as a method. Only a single bare `@property`
decorator is supported; provider declarations and runtime files do not support
decorators.

`struct[T]` bounds the values of existing fields. Required protocol fields need
independent presence evidence, such as explicit constructor keywords or required
keys in an unpacked dictionary. Every Starlark value is assignable to `object`;
attribute access requires a more specific type.

## Implementation validation

`starpls check --validate-stubs` checks the selected implementations against their
stub declarations. Missing function annotations come from the stub's scope. Source
annotations, parameter names, parameter kinds, and defaults determine the
implementation signature. Exported functions are followed through explicit
reexports to check their bodies.

Private function declarations also supply contracts when the implementation
defines the same name. Private helper types and unmatched private names are
local to the stub.

Missing exports and incompatible types are errors. Compatibility that depends on
dynamic types and unsupported function correspondence produce an
incomplete-validation error. Matching
variadic parameters are supported. A fully annotated generic function declaration
can describe an unannotated implementation:

```python
# source.bzli
def identity[T](value: T) -> T: ...
```

```python
# source.bzl
def identity(value):
    return value
```

The type parameters retain their declaration scope, and each call specializes
them independently. Generic contracts with missing stub annotations or source
annotations produce an incomplete result, as do overloaded and conflicting
function contracts. `--ignore_pattern` selects implementation
files and reexported bodies to exclude. Callers use the trusted stub contracts
independently of validation results.

Equivalent function signatures may include `Any` when their other type
components are fully known. Ordinary inferred return types must establish the
declared output contract, including the parameter domains of returned callbacks.
An explicit `Any` return annotation admits every result type.

Function validation checks operations on module globals using conservative
value bounds. For example, a global `list[Any]` can be read as objects and copied
into a fresh `list[object]`. Mutations and calls must be valid under those bounds.
Other body expressions require static types, including the signatures of
callable values.

Ordinary type errors retain their source diagnostics. Failed conservative
checks, unresolved inference, and suppressed checking failures produce an
incomplete result. Parameter defaults require static expression evidence or a
fresh literal construction whose contents are proved independently of the
parameter type. Default expressions and provider initializers also require
checking without unresolved or suppressed failures.

Module variables with one simple assignment can borrow a fully static stub
annotation. The source module must leave the variable unread and unmodified,
including in nested functions. Fresh literal containers and statically typed
literal values provide independent evidence for checking the initializer.
Source annotations and type comments take precedence.

Fresh container initializers can include named list leaves whose references are
confined to that initializer. Each leaf must have one unannotated simple
assignment to a nonempty list of static scalar literals. These lists use their
independently inferred types while the root initializer receives the stub
context. Starlark freezes module values before import.

Structural checking enforces required fields and declared value types.
Implicitly open `TypedDict` contracts allow additional fields; `closed=True`
forbids them and `extra_items` specifies their types. Other variable contracts
use independently inferred source types; validation is incomplete when those
types cannot establish the required dictionary fields, including through
containers.

Provider validation checks the original source's allowed fields and constructor
inputs. A constructor that stores fields directly must require every declared
field and accept only keywords; unrestricted source schemas also accept keyword
variadic parameters. Initializer parameters borrow missing annotations from
`__init__`, and Ty checks every return against the required field mapping.
Known source-only fields are permitted in that mapping.

An initializer proof requires static expression and called-function types, and
nominal field contracts composed through containers and unions. Structural
contracts, dynamic evidence, explicit source return annotations, unrestricted
initializer schemas, and ambiguous correspondence produce an incomplete result.
Exported provider instances also produce an incomplete result when the original
source inference lacks evidence for their stored values. Ordinary body errors
and constructor errors are reported alongside these results.

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
