# Bazel Features stubs

`bazel_features_stubs` 0.1.0 declares the version parser and comparison helpers
for Bazel Features 1.50.0. The parser produces numeric version segments, a
release flag, and tagged numeric or string prerelease identifiers. The six
version predicates return booleans, which give feature flags concrete types.

Add the source and stub modules to the consumer's `MODULE.bazel`:

```starlark
bazel_dep(name = "bazel_features", version = "1.50.0")
bazel_dep(name = "bazel_features_stubs", version = "0.1.0")

local_path_override(
    module_name = "bazel_features_stubs",
    path = "/path/to/starpls/stubs/bazel_features",
)
```

Select the manifest in the consumer's `starpls.toml`:

```toml
[[stub-packages]]
manifest = "@bazel_features_stubs//:stubs.toml"
```

The manifest checks the selected source version. Starpls uses the declarations
to check callers. Public feature records are inferred from their source
using the helper contracts.

The declarations in this package use the [MIT license](LICENSE-MIT).
