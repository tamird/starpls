# Rules Java stubs

`rules_java_stubs` 0.1.0 declares the private namespace factories and their
helpers in Rules Java 9.3.0's Bazel implementation.

Add the source and stub modules to the consumer's `MODULE.bazel`:

```starlark
bazel_dep(name = "rules_java", version = "9.3.0")
bazel_dep(name = "rules_java_stubs", version = "0.1.0")

local_path_override(
    module_name = "rules_java_stubs",
    path = "/path/to/starpls/stubs/rules_java",
)
```

Select the manifest in the consumer's `starpls.toml`:

```toml
[[stub-packages]]
manifest = "@rules_java_stubs//:stubs.toml"
```

The manifest checks the selected source version. Starpls uses these contracts
to check callers; `starpls check --validate-stubs` checks all seven selected
function bodies against the declarations.

`get_internal_java_common` returns a partial namespace with the zero-argument
`google_legacy_api_enabled` Boolean callback. `_make_java_common` returns a
partial namespace with the `BootClassPathInfo`, `JavaRuntimeInfo`, and
`JavaToolchainInfo` provider keys. Both namespaces combine named readonly
fields with gradual access through other names. Bazel enforces native API
availability, access permissions, and provider identity.

The pinned source sets `semantics.IS_BAZEL` to true. On that branch,
`_get_message_bundle_info`, `_set_annotation_processing`, and
`_java_toolchain_label` return `None`. `_get_constraints` returns a fresh empty
list, described as `list[str]`. `_add_constraints` returns its input unchanged;
its generic declaration preserves the input type in the result. Inputs ignored
by these Bazel branches accept `object`. Parameter names and defaults follow
the source.

The declarations use the [MIT license](LICENSE-MIT).
