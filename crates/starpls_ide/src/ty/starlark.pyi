"""Starlark declarations whose contracts differ from Python's builtins.

These fixed language declarations use Ty's ordinary generic call inference.
They do not describe or rewrite executable Starlark source.
"""

import builtins as _builtins
import typing as _typing
from _typeshed import SupportsRichComparison as _Comparable, SupportsRichComparisonT as _ComparableT

_T = _typing.TypeVar("_T")
_U = _typing.TypeVar("_U")
_V = _typing.TypeVar("_V")

@_typing.overload
def type(x: _builtins.bool, /) -> _typing.Literal["bool"]: ...
@_typing.overload
def type(x: _builtins.int, /) -> _typing.Literal["int"]: ...
@_typing.overload
def type(x: _builtins.str, /) -> _typing.Literal["string"]: ...
@_typing.overload
def type(x: _builtins.list[_typing.Any], /) -> _typing.Literal["list"]: ...
@_typing.overload
def type(x: _builtins.dict[_typing.Any, _typing.Any], /) -> _typing.Literal["dict"]: ...
@_typing.overload
def type(x: _builtins.tuple[_typing.Any, ...], /) -> _typing.Literal["tuple"]: ...
@_typing.overload
def type(x: _builtins.set[_typing.Any], /) -> _typing.Literal["set"]: ...
@_typing.overload
def type(x: _builtins.range, /) -> _typing.Literal["range"]: ...
@_typing.overload
def type(x: None, /) -> _typing.Literal["NoneType"]: ...
# Ty's float annotation includes integers, so it uses the general result.
@_typing.overload
def type(x: _builtins.object, /) -> _builtins.str: ...

def fail(
    *args: _builtins.object,
    msg: _builtins.object = None,
    attr: _builtins.str | None = None,
    sep: _builtins.str = " ",
) -> _typing.Never: ...

def print(*args: _builtins.object, sep: _builtins.str = " ") -> None: ...

def enumerate(
    list: _typing.Iterable[_T], start: _builtins.int = 0
) -> _builtins.list[_builtins.tuple[_builtins.int, _T]]: ...

def reversed(sequence: _typing.Iterable[_T], /) -> _builtins.list[_T]: ...

def sorted(
    iterable: _typing.Iterable[_T],
    /,
    key: _typing.Callable[[_T], _typing.Any] | None = None,
    *,
    reverse: _builtins.bool = False,
) -> _builtins.list[_T]: ...

@_typing.overload
def min(first: _ComparableT, second: _ComparableT, /, *args: _ComparableT, key: None = None) -> _ComparableT: ...
@_typing.overload
def min(first: _T, second: _T, /, *args: _T, key: _typing.Callable[[_T], _Comparable]) -> _T: ...
@_typing.overload
def min(iterable: _typing.Iterable[_ComparableT], /, *, key: None = None) -> _ComparableT: ...
@_typing.overload
def min(iterable: _typing.Iterable[_T], /, *, key: _typing.Callable[[_T], _Comparable]) -> _T: ...

@_typing.overload
def max(first: _ComparableT, second: _ComparableT, /, *args: _ComparableT, key: None = None) -> _ComparableT: ...
@_typing.overload
def max(first: _T, second: _T, /, *args: _T, key: _typing.Callable[[_T], _Comparable]) -> _T: ...
@_typing.overload
def max(iterable: _typing.Iterable[_ComparableT], /, *, key: None = None) -> _ComparableT: ...
@_typing.overload
def max(iterable: _typing.Iterable[_T], /, *, key: _typing.Callable[[_T], _Comparable]) -> _T: ...

@_typing.overload
def zip() -> _builtins.list[_builtins.tuple[()]]: ...
@_typing.overload
def zip(first: _typing.Iterable[_T], /) -> _builtins.list[_builtins.tuple[_T]]: ...
@_typing.overload
def zip(
    first: _typing.Iterable[_T], second: _typing.Iterable[_U], /
) -> _builtins.list[_builtins.tuple[_T, _U]]: ...
@_typing.overload
def zip(
    first: _typing.Iterable[_T],
    second: _typing.Iterable[_U],
    third: _typing.Iterable[_V],
    /,
) -> _builtins.list[_builtins.tuple[_T, _U, _V]]: ...
@_typing.overload
def zip(*args: _typing.Iterable[_typing.Any]) -> _builtins.list[_builtins.tuple[_typing.Any, ...]]: ...
