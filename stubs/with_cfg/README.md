# with_cfg stubs

`with_cfg_stubs` 0.1.0 declares the fluent builder contract for
[with_cfg.bzl 0.14.6](https://github.com/fmeum/with_cfg.bzl/tree/v0.14.6).
`build()` returns a callable and an optional rule. Bazel requires global
bindings for freshly created rules, including private bindings.
The builder exposes readonly callable fields with named parameters.

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
`get_rule_name`, `is_executable`, `is_test`, `_is_native`,
`_supports_inheritance`, `_supports_extension`, `get_implicit_targets`, and
`_all_providers`; implementation validation checks their bodies. The collector
accepts iterable values and exposes its result as a readonly sequence of
objects. Provider-specific operations require a more specific element contract.
Private builder contracts cover `_reset_on_attrs` and `_resettable`. Both
use the shared `Builder` type for their receiver and result.
`_clone_value_deeply(object) -> object` copies lists and returns other values.
The utility contracts declare `is_label`, `is_string`, and `is_list` as type
guards. A true result narrows the input to `Label`, `str`, or the readonly
`Sequence[object]` view, respectively. In the true branch, the list guard
replaces any more specific list type with this readonly view. The other four
utility predicates accept `object` and return `bool`. The setting
interface declares `make_valid_identifier(str) -> str` and
`validate_and_get_attr_name(str | Label) -> str`.
`_get_type_as_attr_type(object)` returns `None` or one of `string`,
`string_list`, `label`, `label_list`, `int`, `int_list`, and `bool`.
It selects the name from the value or the first element of a nonempty
list; `None` and empty lists have no selected name.
The frontend interface declares `get_frontend` with a callable returning
`None` and `_frontend_default` with a closed set of optional alias attributes. That
set covers the common attributes forwarded by the wrapper and excludes
`exec_properties` and `exec_group_compatible_with`, which `alias` does not
accept. Callers must satisfy the chosen frontend's parameters.
The source's `_frontend_impl` annotation declares `None` despite
returning a list; implementation validation reports that mismatch.
The select parser interface describes successful string, scalar, list, and
shallow dictionary results, paired with integer cursor positions. It covers
`_consume_string`, `consume_single_value`, `consume_list`,
`_consume_list_or_single_value`, and `_consume_compound_value`.
`decompose_select_elements` preserves each Boolean tag with its payload:
a true tag carries a dictionary of parsed compound values. `_apply_func`
accepts a callback that takes `object`. Each call specializes the dictionary's
key and value types and the callback result independently. It retains the tag
and returns a fresh dictionary with the same key type for true-tagged items.
Mapped payloads use the callback's result type. `_is_dict_element` accepts the
parser's mapped item domain: true tags carry dictionaries with the parser's
key union and `object` values, and false tags carry `object`. It returns `bool`.
These contracts describe successful results; select mapping and recombination
remain unproved.
The transition interface declares `_get_settings_key(str | Label) -> str`,
using the validated Label and string predicates. It also declares
`_encode_settings(dict[str, object]) -> str`: successful settings encoding
returns a JSON string. Transition construction remains unproved.
`RuleInfo` declares its eight readonly fields and required constructor
arguments. `providers` exposes a readonly sequence of objects.
The fluent builder's gradual body types and recursive return values can still
leave its full validation incomplete.

Setting values depend on Bazel's selected configuration. The declarations
check supported scalar and list forms; list element conversion and builder
lifecycle checks are enforced when Bazel evaluates the builder.

Licensed under the [MIT license](LICENSE-MIT).
