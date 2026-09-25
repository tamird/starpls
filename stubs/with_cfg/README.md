# with_cfg stubs

`with_cfg_stubs` 0.1.0 declares the fluent builder contract for
[with_cfg.bzl 0.14.6](https://github.com/fmeum/with_cfg.bzl/tree/v0.14.6).
`build()` returns a callable and an optional rule. Bazel requires global
bindings for freshly created rules, including private bindings.

Add the source and stub modules to the consumer's `MODULE.bazel`:

```starlark
bazel_dep(name = "with_cfg.bzl", version = "0.14.6")
bazel_dep(name = "with_cfg_stubs", version = "0.1.0")

local_path_override(
    module_name = "with_cfg_stubs",
    path = "/path/to/starpls/stubs/with_cfg",
)
```

Select the manifest in `starpls.toml`:

```toml
[[stub-packages]]
manifest = "@with_cfg_stubs//:stubs.toml"
```

The manifest checks the selected source version. Starpls uses the trusted
contract to check callers; `starpls check --validate-stubs` checks the
implementation separately. The private module interface declares
`is_executable` and `get_implicit_targets`; implementation validation checks
their bodies. The fluent builder's gradual signature and recursive return
values can still leave its full validation incomplete.

Setting values depend on Bazel's selected configuration. The declarations
check supported scalar and list forms; list element conversion and builder
lifecycle checks are enforced when Bazel evaluates the builder.

Licensed under the [MIT license](LICENSE-MIT).
