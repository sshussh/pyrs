# Subset of CPython's json module.
# Function bodies are replaced by the compiler with runtime helpers.
# `dumps` is polymorphic: the compiler chooses a path from the argument type.
# `loads` is not dynamic — use the typed loads_* helpers.


def dumps(x: str) -> str:
    return x


def loads_int(s: str) -> int:
    return 0


def loads_float(s: str) -> float:
    return 0.0


def loads_bool(s: str) -> bool:
    return False


def loads_str(s: str) -> str:
    return s


def loads_list_int(s: str) -> list[int]:
    return []


def loads_list_float(s: str) -> list[float]:
    return []


def loads_list_str(s: str) -> list[str]:
    return []


def loads_list_bool(s: str) -> list[bool]:
    return []


def loads_dict_str_int(s: str) -> dict[str, int]:
    return {}


def loads_dict_str_float(s: str) -> dict[str, float]:
    return {}


def loads_dict_str_str(s: str) -> dict[str, str]:
    return {}


def loads_dict_str_bool(s: str) -> dict[str, bool]:
    return {}
