# printf-style `%` on a dynamic str

**Status: implemented in 0.146.0; completed in 0.152.0.** Item B of
[the remaining dynamism brief](2026-09-10-remaining-dynamism-work.md).

## What shipped first

`"x=%d" % 5` on an `object` named itself as unimplemented. 0.146.0 implemented
`%s %d %i %u %f %e %g %r %x %o %%`, width, precision, the `- + 0 space #`
flags, and a tuple right-hand side, with CPython's error wordings. Mapping keys,
`*` width and `%c` still named themselves as gaps.

Review diffed 41 success cases against CPython, including banker's rounding
(`%.0f` of 2.5 is `'2'`, of 3.5 is `'4'`), `%#o` (`'0o10'`), `%#g` and negative
hex. All matched.

## What review found

One message broke the error contract in the direction that is easy to miss.
`"%(k)s" % 5` said "not supported yet", which is reserved for things CPython
*accepts*. CPython rejects that call, with `format requires a mapping`. The rule
is only half about naming gaps; the other half is giving CPython's own error
when CPython refuses too.

## Completion

The remaining forms were implemented rather than the one message corrected:

- **Mapping keys** look the key up in a dict argument, with CPython's nested
  parenthesis counting (`%((k))s` names the key `(k)`).
- **`*` width and precision** take the next positional argument; a negative
  width means left-align, and anything but an int is `* wants int`.
- **`%c`** takes an int code point or a one-character str.
- **`%a`** is `repr` with every non-ASCII character escaped.

CPython's corners here were measured rather than assumed:

| Input | CPython |
|---|---|
| `"%(k)s" % [1]` | `list indices must be integers or slices, not str` — a list passes the mapping check |
| `"%(k)s %s" % {"k": 1}` | `not enough arguments` — no positional arguments remain after a mapping key |
| `"%s" % {"k": 1}` | `"{'k': 1}"` — a dict is still a single positional argument |
| `"%c" % 1114112` | `OverflowError: %c arg not in range(0x110000)` |

A dict used through its keys is not a leftover argument, so
`not all arguments converted` does not fire for it.

## Verification

24 success forms and 16 error forms diffed byte-identical against CPython;
`cli/tests/dynamic_operators.rs` pins the extended forms and eleven error
cases.
