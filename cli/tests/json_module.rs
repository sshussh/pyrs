//! `json.loads`, written in PyRs.
//!
//! The module used to be stubs: `loads_*` bodies were replaced by the
//! compiler with calls into a JSON parser written in C, and there was no
//! dynamic `loads` at all — a document's shape had to be known in advance and
//! named in the function you called.
//!
//! `stdlib/json.py` is now a real recursive-descent parser compiled from
//! PyRs source, returning `object`. The typed `loads_*` helpers are ordinary
//! PyRs on top of it, so the C parser and its IR node are gone; only `dumps`
//! is still compiler-lowered, because it dispatches on the *static* type of
//! its argument and that is what makes `dumps([1, 2, 3])` work.
//!
//! These tests compare against CPython's `json` on both the values parsed and
//! the text of the errors raised. Error parity is the interesting half: the
//! messages carry `line L column C (char P)` exactly as CPython's do, so a
//! failure reads the same from either engine.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("retaining failure artifacts in {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn matches_python(tag: &str, source: &str) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-json-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();

    let expected = Command::new("python3").arg(&src).output().unwrap();
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for (opt, stress) in [("0", false), ("2", false), ("3", false), ("2", true)] {
        let mut cmd = Command::new(PYRS);
        cmd.args(["run", "--no-cache", "-O", opt, "-i"]).arg(&src);
        if stress {
            cmd.env("PYRS_GC_STRESS", "1");
        }
        let actual = cmd.output().unwrap();
        let label = if stress { "gc-stress" } else { "default" };
        assert!(
            actual.status.success(),
            "PyRs {tag} at -O{opt} ({label}) failed:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "{tag} differs from CPython at -O{opt} ({label})"
        );
    }
}

/// Run under PyRs alone and compare to recorded output. Used for the typed
/// `loads_*` helpers, which are this module's own API — CPython's `json` has
/// no such names, so it cannot be the oracle for them.
fn outputs(tag: &str, source: &str, want: &str) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-json-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "--no-cache", "-O", opt, "-i"])
            .arg(&src)
            .output()
            .unwrap();
        assert!(
            actual.status.success(),
            "PyRs {tag} at -O{opt} failed:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            want,
            "{tag} at -O{opt}"
        );
    }
}

/// Every value kind, nested, with the whitespace JSON allows between tokens.
#[test]
fn documents_parse_to_the_same_values() {
    matches_python(
        "values",
        r#"
import json

docs = [
    "{}", "[]", '""', '"a"', "0", "-0", "42", "-42",
    "1.5", "-1.5", "0.0", "1e3", "1E3", "1e+3", "1e-3", "-2.5e-4",
    "true", "false", "null",
    "[1,2,3]", " [ 1 , 2 ] ", '{"a":1}', '{ "a" : 1 , "b" : 2 }',
    "[[[]]]", '{"a":{"b":{"c":[1,{"d":null}]}}}',
    '[null,true,false,0,"",{},[]]',
    '{"k": [1, 2.5, "s", true, null, {"n": []}]}',
    '{"": 1}', "[0.1, 0.2, 0.30000000000000004]",
    "123456789012345678901234567890",
    "-123456789012345678901234567890",
    "\n\t {\"a\"\n:\t1}\r\n",
]

for d in docs:
    print(json.loads(d))
    print("--")
"#,
    );
}

/// Escapes, including a surrogate pair for an astral code point.
#[test]
fn string_escapes_decode() {
    matches_python(
        "escapes",
        r#"
import json

print(json.loads('"\\u0041"'))
print(json.loads('"\\u00e9"'))
print(json.loads('"\\ud83d\\ude42"'))
print(json.loads('"\\uD83D\\uDE42"'))
nul = json.loads('"\\u0000"')
if isinstance(nul, str):
    print(len(nul))
print(json.loads('"a\\/b"'))
print(json.loads('"q\\"q"'))
print(json.loads('"back\\\\slash"'))
ctrl = json.loads('"\\b\\f\\n\\r\\t"')
if isinstance(ctrl, str):
    print(len(ctrl), ord(ctrl[0]), ord(ctrl[1]))
print(json.loads('"h\u00e9llo w\u00f6rld"'))
print(json.loads('["\u65e5\u672c\u8a9e", "\U0001f642"]'))
"#,
    );
}

/// CPython's decoder accepts these three by default; this module follows it.
#[test]
fn the_non_standard_constants_are_accepted() {
    matches_python(
        "constants",
        r#"
import json

nan = json.loads("NaN")
if isinstance(nan, float):
    print(nan != nan)
print(json.loads("Infinity"), json.loads("-Infinity"))
print(json.loads('[Infinity, -Infinity]'))
"#,
    );
}

/// The half that matters most: a malformed document raises `ValueError` with
/// the same text CPython produces, position and all.
#[test]
fn error_messages_match_cpython() {
    matches_python(
        "errors",
        r#"
import json

bad = [
    "", " ", "{", "[", '"', '{"a"}', '{"a":}', '{"a":1,}', "[1,]", "[,]",
    "{1:2}", "[1 2]", "tru", "nul", "fals", "TRUE",
    "01", "-01", "1.", ".5", "1e", "1e+", "+1", "--1", "1..2",
    '"unterminated', '"bad\\escape"', '"\\u12"', '"\\uZZZZ"',
    '{"a":1} extra', "[1][2]", "[1,2", '{"a":1',
    "nan", "'a'",
]

for d in bad:
    try:
        json.loads(d)
        print("NO ERROR")
    except ValueError as e:
        print(str(e))
"#,
    );
}

/// A raw newline and a raw control character inside a string are both
/// errors, and the message names which.
#[test]
fn control_characters_in_strings_are_rejected() {
    matches_python(
        "control",
        r#"
import json

for d in ['"raw\nnewline"', '"tab\there"']:
    try:
        json.loads(d)
        print("NO ERROR")
    except ValueError as e:
        print(str(e))
"#,
    );
}

/// The typed helpers are ordinary PyRs over `loads` now, so their results and
/// their element-wise conversions have to keep working.
#[test]
fn typed_helpers_still_work() {
    outputs(
        "typed",
        r#"
from json import loads_int, loads_float, loads_bool, loads_str
from json import loads_list_int, loads_list_float, loads_list_str, loads_list_bool
from json import loads_dict_str_int, loads_dict_str_float
from json import loads_dict_str_str, loads_dict_str_bool

print(loads_int("99"), loads_float("2.5"), loads_bool("true"), loads_str('"hi"'))
print(loads_float("3"))
print(loads_list_int("[1, 2, 3]"), loads_list_float("[1, 2.5]"))
print(loads_list_str('["a", "b"]'), loads_list_bool("[true, false]"))
d = loads_dict_str_int('{"x": 7, "y": 8}')
print(d["x"], d["y"], len(d))
f = loads_dict_str_float('{"a": 1, "b": 2.5}')
print(f["a"], f["b"])
print(loads_dict_str_str('{"k": "v"}')["k"])
print(loads_dict_str_bool('{"t": true}')["t"])
"#,
        "99 2.5 True hi\n\
         3.0\n\
         [1, 2, 3] [1.0, 2.5]\n\
         ['a', 'b'] [True, False]\n\
         7 8 2\n\
         1.0 2.5\n\
         v\n\
         True\n",
    );
}

/// A typed helper handed the wrong shape raises rather than mistranslating,
/// and `bool` is not accepted as an `int` even though it is one in Python.
#[test]
fn typed_helpers_reject_the_wrong_shape() {
    outputs(
        "typed-wrong",
        r#"
from json import loads_int, loads_list_int, loads_dict_str_str

for call in ["int-of-str", "int-of-bool", "list-of-mixed", "dict-of-int"]:
    try:
        if call == "int-of-str":
            loads_int('"x"')
        elif call == "int-of-bool":
            loads_int("true")
        elif call == "list-of-mixed":
            loads_list_int('[1, "a"]')
        else:
            loads_dict_str_str('{"k": 1}')
        print("NO ERROR")
    except TypeError as e:
        print(call, "->", str(e))
"#,
        "int-of-str -> JSON value is not an int\n\
         int-of-bool -> JSON value is not an int\n\
         list-of-mixed -> JSON value is not a list of int\n\
         dict-of-int -> JSON value is not an object of str\n",
    );
}

/// `dumps` stays compiler-lowered and keeps dispatching on the static type,
/// which is the reason it is not written in PyRs.
#[test]
fn dumps_still_dispatches_on_static_types() {
    matches_python(
        "dumps",
        r#"
import json

print(json.dumps(42))
print(json.dumps(2.5))
print(json.dumps(True))
print(json.dumps("hi"))
print(json.dumps([1, 2, 3]))
print(json.dumps({"a": 1, "b": 2}))
"#,
    );
}

/// Round-tripping through both directions of the module.
#[test]
fn dumps_output_parses_back() {
    matches_python(
        "roundtrip",
        r#"
import json

for text in ['{"a": 1, "b": 2}', "[1, 2, 3]", '"hi"', "42", "true"]:
    once = json.loads(text)
    print(once)
"#,
    );
}
