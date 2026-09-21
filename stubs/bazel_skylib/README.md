# Bazel Skylib stubs

`bazel_skylib_stubs` 0.1.0 provides Starpls annotations for Bazel Skylib 1.9.0:

- [`copy_file`](https://github.com/bazelbuild/bazel-skylib/blob/1.9.0/rules/copy_file.bzl), following its [implementation](https://github.com/bazelbuild/bazel-skylib/blob/1.9.0/rules/private/copy_file_private.bzl).
- [`write_file`](https://github.com/bazelbuild/bazel-skylib/blob/1.9.0/rules/write_file.bzl), following its [implementation](https://github.com/bazelbuild/bazel-skylib/blob/1.9.0/rules/private/write_file_private.bzl).

The stub package has its own release version. Its manifest lists compatible
selected Skylib versions.

## Use a local checkout

Add the source and stub modules to the consumer's `MODULE.bazel`:

```starlark
bazel_dep(name = "bazel_skylib", version = "1.9.0")
bazel_dep(name = "bazel_skylib_stubs", version = "0.1.0")

local_path_override(
    module_name = "bazel_skylib_stubs",
    path = "/path/to/starpls/stubs/bazel_skylib",
)
```

Select the manifest in the consumer's `starpls.toml`:

```toml
[[stub-packages]]
manifest = "@bazel_skylib_stubs//:stubs.toml"
```

Starpls uses these declarations for loads from the public Skylib modules in
batch checking and the language server. `starpls check --validate-stubs` also
checks their implementations and reports contracts that cannot be proved.

Licensed under the [MIT license](LICENSE-MIT).
