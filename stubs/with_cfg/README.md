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
`_clone_value_deeply` preserves the selected canonical type: `str`, `Label`,
`int`, `bool`, `list[Any]`, or `None`. It copies lists and returns other
supported values unchanged. List inputs select `list[Any]` as the result.
The extension interface declares `_initializer_base` with string-keyed
keyword arguments and a read-only mapping of setting values keyed by strings
or labels. The mapping accepts dictionaries with more specific value types.
The helper returns a fresh string-keyed dictionary of objects, with validated
setting names overriding matching keyword arguments.
The factory declarations require the local 0.14.6 source patch that extracts
`_make_initializer`. Its callback accepts arbitrary keyword objects and returns
`dict[str, object]`; the constructor captures a read-only settings mapping.
`make_transitioned_rule` takes that mapping, a `RuleInfo`, and a transition,
and returns a rule. Implementation validation checks the factory, constructor,
and initializer body. Bazel still enforces extension eligibility and the
initializer's allowed attribute names and values.
The wrapper interface declares `_replace_single_dep` with an `object`
input, a Label-to-string memo dictionary, and an integer counter. Its
callback takes the keyword arguments `name: str` and `exports: Label` and
returns `None`. The helper validates the input as a string or Label and
returns the replacement as a string or Label.
The utility contracts declare `is_label` and `is_string` with `TypeGuard`.
A true result narrows the input to `Label` or `str`, respectively.
`is_list` uses `TypeIs[list[Any]]` to narrow list membership on both outcomes
while preserving a caller's existing list element types. `is_select` uses
`TypeIs[select[object]]` to distinguish selectors while preserving their
existing value types. Implementation validation checks both implications of
these native type comparisons. The other three utility predicates accept
`object` and return `bool`. The setting
interface declares `make_valid_identifier(str) -> str` and
`validate_and_get_attr_name(str | Label) -> str`.
`_get_type_as_attr_type(object)` returns `None` or one of `string`,
`string_list`, `label`, `label_list`, `int`, `int_list`, and `bool`.
It selects the name from the value or the first element of a nonempty
list; `None` and empty lists have no selected name.
`get_attr_type(object)` returns one of the same seven names, using
`string_list` when no value selects a name. Its body currently validates
with a local 0.14.6 source patch that traverses the parsed select values
directly and returns the first selected name. The unpatched release's
captured result cell leaves the return proof incomplete.
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
