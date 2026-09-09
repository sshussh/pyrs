# JSON, written in PyRs.
#
# `loads` is a real recursive-descent parser compiled from this file — not a
# compiler intrinsic. It returns `object`, the dynamic value type, so a
# document's shape does not have to be known in advance; consume the result
# with `isinstance` narrowing, or through the typed `loads_*` helpers below,
# which are themselves ordinary PyRs written on top of `loads`.
#
# The first draft needed an unreachable `return` after every call to a helper
# that always raises. That was a missing feature, and v0.139 added it: a call
# to a never-returning function terminates, inferred from the body.
#
# The two `items: list[object] = value` lines below are not that. `object` can
# hold a `list[int]` as well as a `list[object]` — different runtime encodings
# — and `isinstance(v, list)` is true for both, so nothing static can choose
# between them. The annotation says which one is expected and is checked at
# run time. Making it implicit was tried and reverted: it turned `print(v)` on
# a concretely-typed list into a trap.
#
# `dumps` is the one function here the compiler still lowers itself, and
# deliberately: it dispatches on the *static* type of its argument, which is
# what lets `dumps([1, 2, 3])` work on a `list[int]`. A version written here
# could only take `object`, and a concrete `list[int]` boxed into `object`
# carries a different runtime tag than the `list[object]` such a body would
# have to read it back as — so it would refuse exactly the calls that matter.
#
# Differences from CPython's json, both deliberate:
#   * A lone surrogate escape (`"\ud800"` with no low surrogate following) is
#     an error rather than a lone surrogate in the result, because a PyRs
#     `str` is well-formed UTF-8 and has no way to hold one. A *pair* is
#     decoded to the astral code point it denotes, as CPython does.
#   * Nesting is capped at `MAX_DEPTH` to turn a deeply nested document into
#     an exception rather than a native stack overflow.

from typing import NoReturn

MAX_DEPTH: int = 200


class JSONDecodeError(ValueError):
    pass


class _Decoder:
    def __init__(self, source: str) -> None:
        self.source: str = source
        self.position: int = 0
        self.length: int = len(source)
        self.depth: int = 0

    # ---- position reporting -------------------------------------------
    #
    # CPython reports `msg: line L column C (char P)`, counting both from 1.
    # Matching it is what makes an error from this module readable next to
    # one from CPython's.

    def fail(self, message: str, position: int) -> NoReturn:
        line: int = 1
        column: int = 1
        index: int = 0

        while index < position and index < self.length:
            if self.source[index] == "\n":
                line += 1
                column = 1
            else:
                column += 1
            index += 1

        raise JSONDecodeError(
            f"{message}: line {line} column {column} (char {position})"
        )

    # ---- scanning ------------------------------------------------------

    def at_end(self) -> bool:
        return self.position >= self.length

    def current(self) -> str:
        if self.at_end():
            return ""
        return self.source[self.position]

    def skip_whitespace(self) -> None:
        while self.position < self.length:
            if self.source[self.position] not in " \t\n\r":
                break
            self.position += 1

    # ---- values --------------------------------------------------------

    def decode(self) -> object:
        self.skip_whitespace()
        value: object = self.value()
        self.skip_whitespace()

        if not self.at_end():
            self.fail("Extra data", self.position)

        return value

    def value(self) -> object:
        self.skip_whitespace()

        if self.at_end():
            self.fail("Expecting value", self.position)

        character: str = self.current()

        if character == "{":
            return self.object()

        if character == "[":
            return self.array()

        if character == '"':
            return self.string()

        # Before the number branch, which would otherwise claim the `-`.
        if self.source.startswith("-Infinity", self.position):
            self.position += 9
            return -float("inf")

        if character == "-" or character.isdigit():
            return self.number()

        if self.source.startswith("true", self.position):
            self.position += 4
            return True

        if self.source.startswith("false", self.position):
            self.position += 5
            return False

        if self.source.startswith("null", self.position):
            self.position += 4
            return None

        # Not JSON, but CPython's decoder accepts all three by default and
        # this module follows it.
        if self.source.startswith("NaN", self.position):
            self.position += 3
            return float("nan")

        if self.source.startswith("Infinity", self.position):
            self.position += 8
            return float("inf")

        self.fail("Expecting value", self.position)

    def enter(self) -> None:
        self.depth += 1
        if self.depth > MAX_DEPTH:
            self.fail("Too deeply nested", self.position)

    def object(self) -> dict[str, object]:
        self.enter()
        result: dict[str, object] = {}

        self.position += 1
        self.skip_whitespace()

        if self.current() == "}":
            self.position += 1
            self.depth -= 1
            return result

        while True:
            self.skip_whitespace()

            if self.current() != '"':
                self.fail(
                    "Expecting property name enclosed in double quotes", self.position
                )

            key: str = self.string()

            self.skip_whitespace()

            if self.current() != ":":
                self.fail("Expecting ':' delimiter", self.position)

            self.position += 1

            result[key] = self.value()

            self.skip_whitespace()

            character: str = self.current()

            if character == "}":
                self.position += 1
                self.depth -= 1
                return result

            if character != ",":
                self.fail("Expecting ',' delimiter", self.position)

            comma: int = self.position
            self.position += 1
            self.skip_whitespace()

            if self.current() == "}":
                self.fail("Illegal trailing comma before end of object", comma)

    def array(self) -> list[object]:
        self.enter()
        result: list[object] = []

        self.position += 1
        self.skip_whitespace()

        if self.current() == "]":
            self.position += 1
            self.depth -= 1
            return result

        while True:
            result.append(self.value())

            self.skip_whitespace()

            character: str = self.current()

            if character == "]":
                self.position += 1
                self.depth -= 1
                return result

            if character != ",":
                self.fail("Expecting ',' delimiter", self.position)

            comma: int = self.position
            self.position += 1
            self.skip_whitespace()

            if self.current() == "]":
                self.fail("Illegal trailing comma before end of array", comma)

    # ---- strings -------------------------------------------------------

    def string(self) -> str:
        start: int = self.position
        self.position += 1

        parts: list[str] = []

        while self.position < self.length:
            character: str = self.source[self.position]

            if character == '"':
                self.position += 1
                return "".join(parts)

            if character == "\\":
                self.position += 1
                parts.append(self.escape())
                continue

            if ord(character) < 0x20:
                self.fail("Invalid control character at", self.position)

            parts.append(character)
            self.position += 1

        self.fail("Unterminated string starting at", start)

    def escape(self) -> str:
        if self.at_end():
            self.fail("Unterminated string starting at", self.position)

        character: str = self.source[self.position]
        self.position += 1

        if character == '"':
            return '"'
        if character == "\\":
            return "\\"
        if character == "/":
            return "/"
        if character == "b":
            return "\b"
        if character == "f":
            return "\f"
        if character == "n":
            return "\n"
        if character == "r":
            return "\r"
        if character == "t":
            return "\t"
        if character == "u":
            return self.unicode_escape()

        self.fail("Invalid \\escape", self.position - 2)

    def hex4(self) -> int:
        if self.position + 4 > self.length:
            self.fail("Invalid \\uXXXX escape", self.position - 1)

        digits: str = self.source[self.position : self.position + 4]

        for digit in digits:
            if digit not in "0123456789abcdefABCDEF":
                self.fail("Invalid \\uXXXX escape", self.position - 1)

        self.position += 4
        return int(digits, 16)

    def unicode_escape(self) -> str:
        first: int = self.hex4()

        # A low surrogate on its own denotes nothing.
        if 0xDC00 <= first <= 0xDFFF:
            self.fail("Invalid \\uXXXX escape (unpaired low surrogate)", self.position - 6)

        if first < 0xD800 or first > 0xDBFF:
            return chr(first)

        # A high surrogate must be followed by its low half. CPython would
        # keep the lone half; a PyRs `str` is well-formed UTF-8 and cannot.
        if self.position + 2 > self.length or self.source[self.position] != "\\":
            self.fail("Invalid \\uXXXX escape (unpaired high surrogate)", self.position - 6)

        if self.source[self.position + 1] != "u":
            self.fail("Invalid \\uXXXX escape (unpaired high surrogate)", self.position - 6)

        self.position += 2
        second: int = self.hex4()

        if second < 0xDC00 or second > 0xDFFF:
            self.fail("Invalid \\uXXXX escape (unpaired high surrogate)", self.position - 12)

        return chr(0x10000 + (first - 0xD800) * 0x400 + (second - 0xDC00))

    # ---- numbers -------------------------------------------------------

    def digit_at(self, index: int) -> bool:
        return index < self.length and self.source[index].isdigit()

    def number(self) -> object:
        start: int = self.position

        if self.current() == "-":
            self.position += 1

        # An integer part is required, and a leading zero stands alone: in
        # `01` the number is `0` and the `1` is trailing junk, which is why
        # this scans a *maximal valid* number and lets `decode` report the
        # remainder as "Extra data" — the same shape CPython's scanner has.
        if self.at_end():
            self.fail("Expecting value", start)

        if self.current() == "0":
            self.position += 1
        elif self.current().isdigit():
            while self.digit_at(self.position):
                self.position += 1
        else:
            self.fail("Expecting value", start)

        is_float: bool = False

        # `.` only belongs to the number when a digit follows it, so `1.`
        # is the integer 1 followed by junk rather than a malformed float.
        if self.current() == "." and self.digit_at(self.position + 1):
            is_float = True
            self.position += 1
            while self.digit_at(self.position):
                self.position += 1

        # Likewise `e`: it needs a digit, optionally behind a sign.
        if self.current() == "e" or self.current() == "E":
            exponent: int = self.position + 1

            if exponent < self.length and (
                self.source[exponent] == "+" or self.source[exponent] == "-"
            ):
                exponent += 1

            if self.digit_at(exponent):
                is_float = True
                self.position = exponent
                while self.digit_at(self.position):
                    self.position += 1

        text: str = self.source[start : self.position]

        if is_float:
            return float(text)

        return int(text)


def loads(source: str) -> object:
    """Parse a JSON document into `object` (dict, list, str, int, float, bool or None)."""
    decoder: _Decoder = _Decoder(source)
    return decoder.decode()


# ---- typed helpers ------------------------------------------------------
#
# Ordinary PyRs over `loads`, for a document whose shape *is* known: they
# save the caller the `isinstance` narrowing and give a `TypeError` naming
# what was found instead. Element-wise rather than a retype, because a
# `list[object]` and a `list[int]` are different runtime encodings.


def _wrong(expected: str) -> NoReturn:
    raise TypeError(f"JSON value is not {expected}")


def loads_int(s: str) -> int:
    value: object = loads(s)
    if isinstance(value, bool):
        _wrong("an int")
    if isinstance(value, int):
        return value
    _wrong("an int")


def loads_float(s: str) -> float:
    value: object = loads(s)
    if isinstance(value, bool):
        _wrong("a float")
    if isinstance(value, float):
        return value
    if isinstance(value, int):
        return float(value)
    _wrong("a float")


def loads_bool(s: str) -> bool:
    value: object = loads(s)
    if isinstance(value, bool):
        return value
    _wrong("a bool")


def loads_str(s: str) -> str:
    value: object = loads(s)
    if isinstance(value, str):
        return value
    _wrong("a str")


def _elements(s: str, expected: str) -> list[object]:
    value: object = loads(s)
    if isinstance(value, list):
        # Not a restatement the compiler could infer away: `object` can hold
        # a `list[int]` as well as a `list[object]`, and `isinstance` is true
        # for both. This says which encoding is expected, and is checked.
        items: list[object] = value
        return items
    _wrong(expected)


def loads_list_int(s: str) -> list[int]:
    out: list[int] = []
    for item in _elements(s, "a list of int"):
        if isinstance(item, bool):
            _wrong("a list of int")
        if isinstance(item, int):
            out.append(item)
        else:
            _wrong("a list of int")
    return out


def loads_list_float(s: str) -> list[float]:
    out: list[float] = []
    for item in _elements(s, "a list of float"):
        if isinstance(item, bool):
            _wrong("a list of float")
        if isinstance(item, float):
            out.append(item)
        elif isinstance(item, int):
            out.append(float(item))
        else:
            _wrong("a list of float")
    return out


def loads_list_str(s: str) -> list[str]:
    out: list[str] = []
    for item in _elements(s, "a list of str"):
        if isinstance(item, str):
            out.append(item)
        else:
            _wrong("a list of str")
    return out


def loads_list_bool(s: str) -> list[bool]:
    out: list[bool] = []
    for item in _elements(s, "a list of bool"):
        if isinstance(item, bool):
            out.append(item)
        else:
            _wrong("a list of bool")
    return out


def _entries(s: str, expected: str) -> dict[str, object]:
    value: object = loads(s)
    if isinstance(value, dict):
        table: dict[str, object] = value
        return table
    _wrong(expected)


def loads_dict_str_int(s: str) -> dict[str, int]:
    out: dict[str, int] = {}
    table: dict[str, object] = _entries(s, "an object of int")
    for key in table.keys():
        item: object = table[key]
        if isinstance(item, bool):
            _wrong("an object of int")
        if isinstance(item, int):
            out[key] = item
        else:
            _wrong("an object of int")
    return out


def loads_dict_str_float(s: str) -> dict[str, float]:
    out: dict[str, float] = {}
    table: dict[str, object] = _entries(s, "an object of float")
    for key in table.keys():
        item: object = table[key]
        if isinstance(item, bool):
            _wrong("an object of float")
        if isinstance(item, float):
            out[key] = item
        elif isinstance(item, int):
            out[key] = float(item)
        else:
            _wrong("an object of float")
    return out


def loads_dict_str_str(s: str) -> dict[str, str]:
    out: dict[str, str] = {}
    table: dict[str, object] = _entries(s, "an object of str")
    for key in table.keys():
        item: object = table[key]
        if isinstance(item, str):
            out[key] = item
        else:
            _wrong("an object of str")
    return out


def loads_dict_str_bool(s: str) -> dict[str, bool]:
    out: dict[str, bool] = {}
    table: dict[str, object] = _entries(s, "an object of bool")
    for key in table.keys():
        item: object = table[key]
        if isinstance(item, bool):
            out[key] = item
        else:
            _wrong("an object of bool")
    return out


def dumps(x: str) -> str:
    # Compiler-lowered: it dispatches on the static type of the argument, so
    # `dumps(42)`, `dumps([1, 2, 3])` and `dumps({"a": 1.0})` all work. The
    # annotation here is only a placeholder for signature lookup; see the
    # module docstring for why this one is not written in PyRs.
    return x
