# Starpls
`starpls` is a language server for [Starlark](https://github.com/bazelbuild/starlark), the configuration language used by Bazel and Buck2.

## Installation

### VSCode

Make sure you have at least the [0.10.0](https://github.com/bazelbuild/vscode-bazel/releases/tag/0.10.0) version of the [vscode-bazel](https://github.com/bazelbuild/vscode-bazel) extension installed, as it adds support for launching a language server.

If you're on a Mac with Apple Silicon, then you can install `starpls` with Homebrew and skip ahead to the section about configuring VSCode:

```sh
brew install withered-magic/brew/starpls
```

Otherwise, you can grab a release from the [releases page](https://github.com/withered-magic/starpls/releases). Make sure to download the appropriate version for your OS and architecture! After downloading the binary, make sure to adjust its permissions to make it executable, e.g.

```sh
chmod +x starpls-darwin-arm64
```

Additionally, on Mac OS, you may see an error similar to

```
“starpls-darwin-arm64” can’t be opened because Apple cannot check it for malicious software.
```

To fix this, click `Show in Finder`, then right-click on the `starpls-darwin-arm64` executable, click `Open`, and select `Open` in the warning that comes up. This will cause the `com.apple.quarantine` xattr to be removed from the executable and will stop the warning from appearing further.

Either way, at this point you can put the executable somewhere on your `$PATH`.

Once done, add the following to your VSCode configuration and reload VSCode for it to take effect:

```json
{
  "bazel.lsp.command": "starpls"
}
```

Experimental features are enabled through flags on the `starpls server` subcommand. For example:

```jsonc
{
    "bazel.lsp.command": "starpls",
    // Note the first argument is "server", which is required because the flags exist only
    // on the "starpls server" subcommand (and not the top-level "starpls" command).
    "bazel.lsp.args": ["server", "--experimental_infer_ctx_attributes"]
}
```

If `starpls` is outside `$PATH`, set `bazel.lsp.command` to its absolute path.
Bazel metadata loads in the background while local hover, completion, and
navigation are available. Diagnostics wait for configuration loading to
finish; workspace references and rename require a valid configuration.

Alternatively, you can build `starpls` with Bazel:

```
bazel run -c opt //editors/code:copy_starpls
```

This builds the executable and copies it to `<repository_root>/editors/code/bin/starpls`. From there, you can add it to the `$PATH` or copy it to a different directory, remembering to update the extension settings as detailed above.

### Zed

Install the [zed-starlark](https://github.com/zaucy/zed-starlark) extension.

### Neovim via nvim-lspconfig
Make sure you've installed and configured [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig) in a way that works for you.

Install using homebrew as described above, then do the following in your init.lua:
```lua
require("lspconfig").starpls.setup { }
```

You can see the config info [here](https://github.com/neovim/nvim-lspconfig/blob/master/lua/lspconfig/configs/starpls.lua).

## Tips and Tricks

### Editor features

In VS Code or Cursor, use **Find All References**, **Rename Symbol**, and
**Go to Definition** on Starlark names. References include unopened Bazel
files in the workspace, loaded dependencies, and configured stubs. An
initial search can take longer while Bazel resolves dependencies.

Rename updates workspace declarations, callers, and uniquely paired `.bzli`
declarations. Renaming an explicit `load` alias changes its local uses;
renaming an export changes its imported spelling and unaliased uses.
External repositories are read-only. Ambiguous stub correspondence, escaped
load spellings, and possible binding collisions produce an error. Dynamic
provider and context fields support navigation; rename requires a source
binding.

Semantic highlighting, selection expansion, folding, and document highlights
use the editor's standard controls. To display inferred types and argument
names inline, enable inlay hints in the editor settings:

```json
{
  "editor.inlayHints.enabled": "on"
}
```

Make sure to use [PEP 484 type comments](https://peps.python.org/pep-0484/#type-comments) to document your function signatures. This helps a ton with autocomplete for situations like `rule` implementation functions. For example, if you add a type comment as in the following...

```python
def _impl(ctx):
    # type: (ctx) -> Unknown
    ctx.
    #  ^ and this period was just typed...
```

then you'll get autocomplete suggestions for the attributes on `ctx`, like `ctx.actions`, `ctx.attr`, and so on!

Type diagnostics and `# type: ignore` use Ty's rules. For a diagnostic spanning multiple lines,
put the suppression on the first or last line of the diagnostic's range. A comment on an interior
line does not suppress the entire diagnostic.

Python-only syntax is diagnosed and its containing statement is omitted from analysis. Valid
neighboring statements are still checked; names introduced only by an omitted statement remain
undefined.

## Batch checking

Run `starpls check` from the Bazel workspace with source files or directories:

```sh
starpls check --bazel-only --files-from files.txt --progress --report coverage.json
```

`--files-from` reads one path per line; `-` reads standard input. Relative
paths use the current directory. `--bazel-only` selects recognized Bazel
sources and `.bzli` interfaces and records other inputs as exclusions.
Recursive discovery stops at nested repository roots. Explicit paths use
their existing repository context, including Bazel's external directory.

Load discovery fetches missing external repositories through Bazel and
retries their dependencies. Each repository is attempted once; missing
files within an existing repository remain load failures.

`--progress` reports load discovery, repository mapping batches, fetches,
and file checking on stderr. The JSON report separates selected files,
completed checks, loaded dependencies, exclusions, input failures, and
unresolved loads. Configured implementation validation adds its checked
source files to the completed checks. Repository names are canonical;
the empty name denotes the main repository, and `null` denotes a source
without a known Bazel repository context.

`complete` means every selected file was checked and every discovered load
resolved. Deliberate scope exclusions appear separately. Type errors,
failed input paths, and unresolved loads produce a failing exit status.
Failed Bzlmod fetches leave coverage incomplete, even if they created
partial files. Without Bzlmod, a best-effort repository query may report
unrelated package errors after creating readable sources; coverage then
depends on whether the requested loads resolve. Native errors appear on
stderr in either case.

## Stub files

See the [stub specification](docs/type-interfaces.md) for declarations,
package selection, conflict handling, and versioning.

Select packages in `starpls.toml` at the Bazel workspace root. Batch checking
and the language server read this configuration at startup:

```toml
[[stub-packages]]
manifest = "@rules_foo_stubs//:stubs.toml"
```

The language server reloads saved configuration, selected manifests, and Bazel module
and workspace inputs. It watches Starlark files in discovered repositories; saving a
`.bzl` file revalidates selected packages. Restart the server after changes to
`.bazelrc`, `.bazelversion`, or other files read by repository extensions.

Use a `.bzli` stub file to provide types for a `.bzl` module:

```starlark
# types/vendor.bzli
DEFAULT_TIMEOUT: int

def fetch(name: string, timeout: int = ...) -> list[string]:
    """Fetch the named resources."""
    ...
```

Configure the same mapping for batch checking or the language server:

```sh
starpls check --type_interface third_party/vendor.bzl=types/vendor.bzli BUILD.bazel
starpls server --type_interface third_party/vendor.bzl=types/vendor.bzli
```

Repeat `--type_interface SOURCE=INTERFACE` for additional modules. Relative paths use the main
Bazel workspace root; both files must exist and be readable. Duplicate source mappings are errors.
Names loaded from the mapped `.bzl` module use stub declarations when present and source
inference otherwise. Stubs may load provider types for use in annotations. Function bodies
use `...` or `pass`, and optional defaults use `= ...`.

Validate the registered implementations with `starpls check --validate-stubs`.
The check borrows missing function annotations from each stub, checks the body,
and compares exported types. Errors distinguish incompatible implementations
from contracts that cannot be proved, including dynamic types and unsupported
parameter correspondence. `--ignore_pattern` excludes matching implementation
files or directories. Caller checking continues to use the selected stubs.

## Experimental features

Starpls has a number of experimental features that can be enabled via command-line arguments:

### `--experimental_infer_ctx_attributes`

Infer `ctx.attr`, `ctx.files`, `ctx.file`, `ctx.executable`, `ctx.outputs`,
and `ctx.split_attr` from a rule's attribute declarations. Completion, hover,
and navigation use those declarations. A callback registered by exactly one
rule receives this context; repository rule callbacks receive `ctx.attr`
alongside native repository methods.

```python
def _foo_impl(ctx):
    ctx.attr.bar # type: int

foo = rule(
    implementation = _foo_impl,
    attrs = {
        "bar": attr.int(),
    },
)
```

### `--experimental_use_code_flow_analysis`

Report unreachable code and uses of possibly unbound variables. Type inference always uses code
flow analysis, regardless of this option.

```python
def example():
    return
    print("unreachable") # Reported when this option is enabled.
```

### `--experimental_enable_label_completions`

Enables completions for labels within Bazel files. For example, given the following `BUILD.bazel` file at the repository root:

```python
my_rule(
    name = "foo"
)

my_rule(
    name = "bar",
    srcs = ["//:"],
              # ^ ... If the cursor is here, "foo" will be suggested.
)
```

## Roadmap

- Parsing
    - [x] Error resilient Starlark parser
    - [x] Syntax error reporting
- Semantic highlighting
    - [x] Unbound variables
    - [x] Type mismatches
    - [x] Function call argument validation
- Auto-completion
    - [x] Variables/function parameters
    - [x] Builtin type fields
    - [x] Rule attributes
    - [x] Custom provider fields
    - [x] Custom struct fields
- Hover
    - [x] Variable types
    - [x] Function signatures
    - [x] Function/method docs
- Go to definition
    - [x] Variables (including `load`ed symbols)
    - [x] Function definitions
    - [x] Struct fields
    - [x] Provider fields
    - [x] Labels and targets
    - [ ] Rule attributes
- Document symbols
    - [x] Variables, functions
    - [x] Bazel targets
- Type inference
    - [x] Basic type inference
    - [ ] Dataflow analysis
    - [x] PEP-484 type comments
        - [x] Variables
        - [x] Parameters (only basic types currently supported)
        - [x] Other constructs where type comments are supported
- Third-party integrations
    - [x] Bazel builtins (partial, Bazel builtins are supported but still need to handle a number of edge cases)
    - Special handling for various Bazel constructs
        - [x] `struct`s (autocomplete fields)
        - [x] providers (autocomplete and validate fields)
        - [x] rules defined with `rule` and `repository_rule` (autocomplete and validate attributes)
- Projects
    - [x] Type inference across multiple files
    - [x] `load` support
        - [x] Relative paths
        - [x] Bazel workspace
    - [x] Bazel external repositories
    - [ ] Nested local repositories

## Development

`starpls` uses the Rust version pinned in `rust-toolchain.toml` and `MODULE.bazel`.

### Prerequisites

- `pnpm`, for managing Node dependencies
- `protoc`, for compiling `builtin.proto`

Steps to get up and running:
1. Run `pnpm install` in `editors/code`.
2. Open VSCode, `Run and Debug > Run Extension (Debug Build)`.
3. In the extension development host, open a `.star` file and enjoy syntax highlighting and error messages!

## Known Issues

- Type guards are not supported.
- Type checker shows some false positives, especially when the definitions from the builtins proto are incorrect.
    - Because of these two issues, some type checking diagnostics are currently set to display as warnings.
- Type checking + goto definition for symbols loaded from external dependencies will only work if those dependencies have already been fetched. If you see `Could not resolve module` warnings in `load` statements, make sure to run `bazel fetch //...` to make sure the external output base is up-to-date.
- When `--enable-bzlmod` is set, type checking/goto definition may be slow for a given file the first time it is loaded. This is because resolution of repo mappings, done with `bazel mod dump_repo_mappings`, is done lazily.
    - Additionally, when new dependencies are added, the language server needs to be restarted to refresh the mappings. This is due to the fact that repo mappings are cached, which is necessary to avoid slow type checking.

## Acknowledgements

- `starpls` is heavily based on the [rust-analyzer](https://github.com/rust-lang/rust-analyzer/tree/master) codebase; one might consider it a vastly simplified version of rust-analyzer that works on Starlark files! As such, major thanks to the rust-analyzer team, especially [Aleksey Kladov](https://matklad.github.io/), whose [Explaining rust-analyzer](https://www.youtube.com/playlist?list=PLhb66M_x9UmrqXhQuIpWC5VgTdrGxMx3y) series on YouTube proved invaluable as a learning resource!
- `starpls`'s mechanism for carrying out type inference is heavily derived from that of [Pyright](https://github.com/microsoft/pyright).
