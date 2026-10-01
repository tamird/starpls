"""Dictionary implementation allocated by Starlark collection syntax and dict()."""

import builtins as _builtins
from typing import final

@final
class dict[_K, _V](_builtins.dict[_K, _V]): ...
