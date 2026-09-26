# Fixed Starlark contracts replacing these classes in pinned builtins.pyi.
# Imports and type variables belong to that module, not to user Starlark source.
# https://github.com/bazelbuild/starlark/blob/master/spec.md#built-in-methods
# Special methods encode operations for Ty and are omitted from completion.

from typing import Collection, TYPE_CHECKING

# Every Starlark value is an object; this top type guarantees no attributes.
class object: ...

@final
@disjoint_base
class bool:
    @type_check_only
    def __new__(cls, value: object = False, /) -> Self: ...
    @type_check_only
    def __bool__(self) -> bool: ...
    @type_check_only
    def __int__(self) -> int: ...
    @type_check_only
    def __float__(self) -> float: ...
    @type_check_only
    def __lt__(self, other: bool, /) -> bool: ...
    @type_check_only
    def __le__(self, other: bool, /) -> bool: ...
    @type_check_only
    def __gt__(self, other: bool, /) -> bool: ...
    @type_check_only
    def __ge__(self, other: bool, /) -> bool: ...

@final
@disjoint_base
class str:
    if TYPE_CHECKING:
        # Disable implicit iteration through the indexing protocol.
        __iter__: ClassVar[None]
    @type_check_only
    def __new__(cls, object: object = "", /) -> Self: ...
    @type_check_only
    def __len__(self) -> int: ...
    @type_check_only
    def __getitem__(self, index: int | slice, /) -> str: ...
    @type_check_only
    def __add__(self, other: str, /) -> str: ...
    @type_check_only
    def __mul__(self, count: int, /) -> str: ...
    @type_check_only
    def __rmul__(self, count: int, /) -> str: ...
    @type_check_only
    def __mod__(self, values: object, /) -> str: ...
    @type_check_only
    def __contains__(self, value: str, /) -> bool: ...
    @type_check_only
    def __lt__(self, other: str, /) -> bool: ...
    @type_check_only
    def __le__(self, other: str, /) -> bool: ...
    @type_check_only
    def __gt__(self, other: str, /) -> bool: ...
    @type_check_only
    def __ge__(self, other: str, /) -> bool: ...
    def capitalize(self) -> str:
        """Return the string with its first character uppercase and the rest lowercase."""
    def count(self, sub: str, start: int | None = 0, end: int | None = None, /) -> int:
        """Count occurrences of sub within the selected string slice."""
    def elems(self) -> Sequence[str]:
        """Return a sequence of successive one-character substrings."""
    def endswith(self, suffix: str | tuple[str, ...], start: int | None = 0, end: int | None = None, /) -> bool:
        """Test whether the selected slice ends with one of the supplied suffixes."""
    def find(self, sub: str, start: int | None = 0, end: int | None = None, /) -> int:
        """Return the first matching index, or -1 when the substring is absent."""
    def format(self, *args: object, **kwargs: object) -> str:
        """Substitute positional and named values into replacement fields."""
    def index(self, sub: str, start: int | None = 0, end: int | None = None, /) -> int:
        """Return the first matching index, failing when the substring is absent."""
    def isalnum(self) -> bool:
        """Test whether the nonempty string contains only letters and digits."""
    def isalpha(self) -> bool:
        """Test whether the nonempty string contains only letters."""
    def isdigit(self) -> bool:
        """Test whether the nonempty string contains only digits."""
    def islower(self) -> bool:
        """Test whether every cased letter is lowercase and at least one is present."""
    def isspace(self) -> bool:
        """Test whether the nonempty string contains only whitespace."""
    def istitle(self) -> bool:
        """Test whether the string's words use title case."""
    def isupper(self) -> bool:
        """Test whether every cased letter is uppercase and at least one is present."""
    def join(self, elements: Iterable[str], /) -> str:
        """Join string elements, inserting this string between each pair."""
    def lower(self) -> str:
        """Return a copy with letters converted to lowercase."""
    def lstrip(self, chars: str | None = None, /) -> str:
        """Remove leading characters in chars, or whitespace when chars is None."""
    def partition(self, sep: str, /) -> tuple[str, str, str]:
        """Split at the first separator into the preceding text, separator, and remainder."""
    def removeprefix(self, prefix: str, /) -> str:
        """Remove prefix if the string starts with it."""
    def removesuffix(self, suffix: str, /) -> str:
        """Remove suffix if the string ends with it."""
    def replace(self, old: str, new: str, count: int = -1, /) -> str:
        """Replace occurrences of old with new, stopping after count when nonnegative."""
    def rfind(self, sub: str, start: int | None = 0, end: int | None = None, /) -> int:
        """Return the last matching index in the selected slice, or -1 if absent."""
    def rindex(self, sub: str, start: int | None = 0, end: int | None = None, /) -> int:
        """Return the last matching index in the selected slice, or fail if absent."""
    def rpartition(self, sep: str, /) -> tuple[str, str, str]:
        """Split at the last separator into the preceding text, separator, and remainder."""
    def rsplit(self, /, sep: str, maxsplit: int = -1) -> list[str]:
        """Split at sep from the right, making at most maxsplit splits when nonnegative."""
    def rstrip(self, chars: str | None = None, /) -> str:
        """Remove trailing characters in chars, or whitespace when chars is None."""
    def split(self, /, sep: str, maxsplit: int = -1) -> list[str]:
        """Split at sep, making at most maxsplit splits when nonnegative."""
    def splitlines(self, keepends: bool = False, /) -> list[str]:
        """Split into lines, retaining line endings only when keepends is true."""
    def startswith(self, prefix: str | tuple[str, ...], start: int | None = 0, end: int | None = None, /) -> bool:
        """Test whether the selected slice starts with a supplied prefix."""
    def strip(self, chars: str | None = None, /) -> str:
        """Remove leading and trailing characters in chars, or whitespace when None."""
    def title(self) -> str:
        """Return a copy with words converted to title case."""
    def upper(self) -> str:
        """Return a copy with letters converted to uppercase."""

@disjoint_base
class bytes:
    if TYPE_CHECKING:
        __iter__: ClassVar[None]
    @type_check_only
    def __new__(cls, value: str | bytes | Iterable[int], /) -> Self: ...
    @type_check_only
    def __len__(self) -> int: ...
    @overload
    @type_check_only
    def __getitem__(self, index: int, /) -> int: ...
    @overload
    @type_check_only
    def __getitem__(self, index: slice, /) -> bytes: ...
    @type_check_only
    def __contains__(self, value: int | bytes, /) -> bool: ...
    @type_check_only
    def __add__(self, other: bytes, /) -> bytes: ...
    @type_check_only
    def __mul__(self, count: int, /) -> bytes: ...
    @type_check_only
    def __rmul__(self, count: int, /) -> bytes: ...
    @type_check_only
    def __lt__(self, other: bytes, /) -> bool: ...
    @type_check_only
    def __le__(self, other: bytes, /) -> bool: ...
    @type_check_only
    def __gt__(self, other: bytes, /) -> bool: ...
    @type_check_only
    def __ge__(self, other: bytes, /) -> bool: ...
    def elems(self) -> Iterable[int]:
        """Return an opaque iterable of byte values, each an integer from 0 to 255."""

@final
@disjoint_base
class list(Sequence[_T]):
    if TYPE_CHECKING:
        __hash__: ClassVar[None]
    @type_check_only
    def __init__(self, iterable: Iterable[_T] = (), /) -> None: ...
    @type_check_only
    def __len__(self) -> int: ...
    @type_check_only
    def __iter__(self) -> Iterator[_T]: ...
    @overload
    @type_check_only
    def __getitem__(self, index: int, /) -> _T: ...
    @overload
    @type_check_only
    def __getitem__(self, index: slice, /) -> list[_T]: ...
    @type_check_only
    def __setitem__(self, index: int, value: _T, /) -> None: ...
    @type_check_only
    def __add__(self, other: list[_S], /) -> list[_T | _S]: ...
    @type_check_only
    def __iadd__(self, other: list[_T], /) -> Self: ...
    @type_check_only
    def __mul__(self, count: int, /) -> list[_T]: ...
    @type_check_only
    def __rmul__(self, count: int, /) -> list[_T]: ...
    @type_check_only
    def __imul__(self, count: int, /) -> Self: ...
    @type_check_only
    def __contains__(self, value: object, /) -> bool: ...
    @type_check_only
    def __lt__(self, other: list[_S], /) -> bool: ...
    @type_check_only
    def __le__(self, other: list[_S], /) -> bool: ...
    @type_check_only
    def __gt__(self, other: list[_S], /) -> bool: ...
    @type_check_only
    def __ge__(self, other: list[_S], /) -> bool: ...
    def append(self, value: _T, /) -> None:
        """Append a value to the list."""
    def clear(self) -> None:
        """Remove every element from the list."""
    def extend(self, elements: Iterable[_T], /) -> None:
        """Append every element from the iterable."""
    def index(self, value: object, start: int = 0, end: int = ..., /) -> int:
        """Return the first equal element's index in the selected slice, or fail."""
    def insert(self, index: int, value: _T, /) -> None:
        """Insert a value before the selected index."""
    def pop(self, index: int = -1, /) -> _T:
        """Remove and return an element; fail if the index is out of range."""
    def remove(self, value: object, /) -> None:
        """Remove the first equal element, or fail when no element matches."""

@final
@disjoint_base
class set(AbstractSet[_T]):
    if TYPE_CHECKING:
        __hash__: ClassVar[None]
    @type_check_only
    def __init__(self, elements: Iterable[_T] = (), /) -> None: ...
    @type_check_only
    def __len__(self) -> int: ...
    @type_check_only
    def __iter__(self) -> Iterator[_T]: ...
    @type_check_only
    def __contains__(self, value: object, /) -> bool: ...
    @type_check_only
    def __or__(self, other: set[_S], /) -> set[_T | _S]: ...
    @type_check_only
    def __ior__(self, other: AbstractSet[_T], /) -> Self: ...
    @type_check_only
    def __and__(self, other: set[_S], /) -> set[_T]: ...
    @type_check_only
    def __iand__(self, other: set[_S], /) -> Self: ...
    @type_check_only
    def __sub__(self, other: set[_S], /) -> set[_T]: ...
    @type_check_only
    def __isub__(self, other: set[_S], /) -> Self: ...
    @type_check_only
    def __xor__(self, other: set[_S], /) -> set[_T | _S]: ...
    @type_check_only
    def __ixor__(self, other: AbstractSet[_T], /) -> Self: ...
    def add(self, value: _T, /) -> None:
        """Add a value to the set."""
    def clear(self) -> None:
        """Remove every element from the set."""
    def difference(self, *others: Collection[object]) -> set[_T]:
        """Return a new set excluding elements found in the other collections."""
    def difference_update(self, *others: Collection[object]) -> None:
        """Remove elements found in the other collections."""
    def discard(self, value: object, /) -> None:
        """Remove a value if present."""
    def intersection(self, *others: Collection[object]) -> set[_T]:
        """Return a new set containing elements present in every collection."""
    def intersection_update(self, *others: Collection[object]) -> None:
        """Remove elements absent from any of the other collections."""
    def isdisjoint(self, other: Collection[object], /) -> bool:
        """Test whether the collections have no common elements."""
    def issubset(self, other: Collection[object], /) -> bool:
        """Test whether every element is present in the other collection."""
    def issuperset(self, other: Collection[object], /) -> bool:
        """Test whether every element of the other collection is present."""
    def pop(self) -> _T:
        """Remove and return the first element; fail if the set is empty."""
    def remove(self, value: object, /) -> None:
        """Remove a value, failing if it is absent."""
    def symmetric_difference(self, other: Collection[_S], /) -> set[_T | _S]:
        """Return a new set of elements present in exactly one collection."""
    def symmetric_difference_update(self, other: Collection[_T], /) -> None:
        """Keep elements present in exactly one of the two collections."""
    @overload
    def union(self) -> set[_T]:
        """Return a new set containing elements from every collection."""
    @overload
    def union(self, other: Collection[_S], /, *others: Collection[_S]) -> set[_T | _S]: ...
    def update(self, *others: Collection[_T]) -> None:
        """Add elements from the other collections."""

@final
@disjoint_base
class dict(Mapping[_KT, _VT]):
    if TYPE_CHECKING:
        __hash__: ClassVar[None]
    @overload
    @type_check_only
    def __init__(self) -> None: ...
    @overload
    @type_check_only
    def __init__(self, pairs: SupportsKeysAndGetItem[_KT, _VT] | Iterable[tuple[_KT, _VT]], /) -> None: ...
    @overload
    @type_check_only
    def __init__(self: dict[_T, _T], pairs: Iterable[list[_T]], /) -> None: ...
    @overload
    @type_check_only
    def __init__(self: dict[str, _VT], /, **kwargs: _VT) -> None: ...
    @overload
    @type_check_only
    def __init__(self: dict[_KT | str, _VT | _S], pairs: SupportsKeysAndGetItem[_KT, _VT] | Iterable[tuple[_KT, _VT]], /, **kwargs: _S) -> None: ...
    @overload
    @type_check_only
    def __init__(self: dict[_T | str, _T | _S], pairs: Iterable[list[_T]], /, **kwargs: _S) -> None: ...
    @type_check_only
    def __len__(self) -> int: ...
    @type_check_only
    def __iter__(self) -> Iterator[_KT]: ...
    @type_check_only
    def __getitem__(self, key: _KT, /) -> _VT: ...
    @type_check_only
    def __setitem__(self, key: _KT, value: _VT, /) -> None: ...
    @type_check_only
    def __contains__(self, key: object, /) -> bool: ...
    @type_check_only
    def __or__(self, other: dict[_T1, _T2], /) -> dict[_KT | _T1, _VT | _T2]: ...
    @type_check_only
    def __ior__(self, other: dict[_KT, _VT], /) -> Self: ...
    def clear(self) -> None:
        """Remove every dictionary entry."""
    @overload
    def get(self, key: object, /, default: None = None) -> _VT | None:
        """Return the value for key, or default when the key is absent."""
    @overload
    def get(self, key: object, /, default: _T) -> _VT | _T: ...
    def items(self) -> list[tuple[_KT, _VT]]:
        """Return a new list of key/value pairs in iteration order."""
    def keys(self) -> list[_KT]:
        """Return a new list of keys in iteration order."""
    @overload
    def pop(self, key: object, /) -> _VT:
        """Remove and return the value for key, or use the supplied default if absent."""
    @overload
    def pop(self, key: object, /, default: _T) -> _VT | _T: ...
    def popitem(self) -> tuple[_KT, _VT]:
        """Remove and return a key/value pair; fail for an empty dictionary."""
    @overload
    def setdefault(self: dict[_KT, _VT | None], key: _KT, /) -> _VT | None:
        """Return the value for key, inserting and returning default when absent."""
    @overload
    def setdefault(self, key: _KT, /, default: _VT) -> _VT: ...
    @overload
    def update(self, pairs: SupportsKeysAndGetItem[_KT, _VT] | Iterable[tuple[_KT, _VT]] = (), /) -> None:
        """Copy entries from a mapping or iterable of pairs, then named entries."""
    # Tuple entries retain separate key/value types. List entries have one
    # homogeneous element type, so their update overload remains gradual.
    @overload
    def update(self, pairs: Iterable[list[Any]], /) -> None: ...
    @overload
    def update(self: dict[_KT | str, _VT], pairs: SupportsKeysAndGetItem[_KT, _VT] | Iterable[tuple[_KT, _VT]] = (), /, **kwargs: _VT) -> None: ...
    @overload
    def update(self: dict[_KT | str, _VT], pairs: Iterable[list[Any]], /, **kwargs: _VT) -> None: ...
    def values(self) -> list[_VT]:
        """Return a new list of values in iteration order."""
