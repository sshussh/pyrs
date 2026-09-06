//! String offsets must be Unicode code points, as in CPython.
//!
//! Before this milestone `PyrsStr` was a UTF-8 byte buffer and every operation
//! but `ord`/`chr`/`ascii` treated it as a plain byte array, so a string with
//! any non-ASCII character disagreed with CPython:
//!
//! * `len("héllo")` was `6`, not `5`; `len("🐍")` was `4`, not `1`.
//! * `"héllo"[1]` yielded one byte of a two-byte character, and `for c in s`
//!   yielded those fragments one at a time.
//! * `"héllo".find("l")` reported a byte offset, so it returned `3`, not `2`.
//! * `str.center`/`ljust`/`zfill` and f-string widths padded to a byte count.
//!
//! Case transforms and the `is*` predicates are deliberately *not* covered
//! here: they remain the documented ASCII-only contract until the Unicode
//! data tables land. What this suite pins is that nothing mixes byte and
//! character offsets any more.

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

/// Differential check at every optimization level, comparing stdout and exit
/// status against the CPython oracle.
fn matches_python(tag: &str, source: &str) {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-unicode-{tag}-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let src = dir.0.join("prog.py");
    fs::write(&src, source).unwrap();

    let expected = Command::new("python3")
        .arg(&src)
        .output()
        .expect("failed to spawn CPython");
    assert!(
        expected.status.success(),
        "CPython failed for {tag}: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i"])
            .arg(&src)
            .output()
            .expect("failed to spawn PyRs");
        assert!(
            actual.status.success(),
            "PyRs {tag} at -O{opt} failed:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout differs for {tag} at -O{opt}"
        );
    }
}

// ---------------------------------------------------------------------------
// len, across every UTF-8 width
// ---------------------------------------------------------------------------

#[test]
fn len_counts_code_points_not_bytes() {
    matches_python(
        "len-widths",
        r#"
print(len(""))
print(len("hello"))
print(len("héllo"))
print(len("naïve café"))
print(len("λx"))
print(len("日本語"))
print(len("🐍"))
print(len("a🐍b"))
print(len("é" * 100))
"#,
    );
}

#[test]
fn len_counts_combining_sequences_as_separate_code_points() {
    // CPython counts code points, not grapheme clusters: "e" + U+0301 is 2.
    matches_python(
        "len-combining",
        r#"
s = "école"
print(len(s))
print(len("é"))
print("é" == "é")
"#,
    );
}

#[test]
fn len_counts_an_embedded_nul_as_one_character() {
    // Written with chr(0) because the lexer does not accept \xNN escapes.
    matches_python(
        "len-nul",
        r#"
nul = chr(0)
s = "a" + nul + "é"
print(len(s))
print(len(nul))
print(s[1] == nul, s[2])
print(len(s.split(nul)))
"#,
    );
}

// ---------------------------------------------------------------------------
// Indexing
// ---------------------------------------------------------------------------

#[test]
fn indexing_returns_whole_characters() {
    matches_python(
        "index-chars",
        r#"
s = "héllo"
print(s[0])
print(s[1])
print(s[4])
print(s[-1])
print(s[-5])
print("🐍"[0])
print("a🐍b"[1])
print("日本語"[2])
"#,
    );
}

#[test]
fn out_of_range_index_uses_the_character_count() {
    // "héllo" is 6 bytes but 5 characters, so index 5 must raise.
    matches_python(
        "index-bounds",
        r#"
s = "héllo"
try:
    print(s[5])
except IndexError as e:
    print("IndexError", e)
try:
    print(s[-6])
except IndexError as e:
    print("IndexError", e)
"#,
    );
}

// ---------------------------------------------------------------------------
// Slicing
// ---------------------------------------------------------------------------

#[test]
fn slicing_uses_character_bounds() {
    matches_python(
        "slice-bounds",
        r#"
s = "héllo"
print(s[1:3])
print(s[:2])
print(s[2:])
print(s[:])
print(s[-2:])
print(s[:-2])
print(s[3:1])
print(s[10:])
print("a🐍b"[1:2])
print("日本語"[1:3])
"#,
    );
}

#[test]
fn strided_slicing_selects_whole_characters() {
    matches_python(
        "slice-step",
        r#"
s = "héllo"
print(s[::-1])
print(s[::2])
print(s[1::2])
print(s[::-2])
print("aébéc"[::2])
print("🐍x🐍"[::-1])
"#,
    );
}

// ---------------------------------------------------------------------------
// Iteration
// ---------------------------------------------------------------------------

#[test]
fn iteration_yields_whole_characters() {
    matches_python(
        "iter-chars",
        r#"
for c in "héllo":
    print(c, len(c))
for c in "a🐍b":
    print(c, ord(c))
count = 0
for c in "日本語":
    count += 1
print(count)
"#,
    );
}

#[test]
fn index_loops_over_long_non_ascii_text_stay_correct() {
    // Also the regression guard for the sequential-access memo: a forward walk
    // must stay linear and must not be perturbed by intervening allocations.
    matches_python(
        "iter-long",
        r#"
s = "é" * 2000
total = 0
for i in range(len(s)):
    total += ord(s[i])
print(total)
print(len(s), s[0], s[1999])
seen = []
for i in range(len(s)):
    if i % 500 == 0:
        seen.append(s[i] + str(i))
print(seen)
"#,
    );
}

// ---------------------------------------------------------------------------
// Search: find / rfind / index / count, with and without bounds
// ---------------------------------------------------------------------------

#[test]
fn find_reports_character_offsets() {
    matches_python(
        "find-offsets",
        r#"
s = "héllo"
print(s.find("l"))
print(s.find("é"))
print(s.find("h"))
print(s.find("z"))
print(s.rfind("l"))
print(s.index("o"))
print("a🐍b🐍c".find("b"))
print("a🐍b🐍c".rfind("🐍"))
"#,
    );
}

#[test]
fn find_accepts_character_bounds() {
    matches_python(
        "find-bounds",
        r#"
s = "héllo héllo"
print(s.find("l", 4))
print(s.find("l", 0, 3))
print(s.find("é", 2))
print(s.rfind("l", 0, 4))
print(s.index("h", 1))
print(s.find("", 3))
print(s.find("", 20))
"#,
    );
}

#[test]
fn count_and_membership_use_characters() {
    matches_python(
        "count-chars",
        r#"
s = "héllo"
print(s.count("l"))
print(s.count("é"))
print(s.count(""))
print("aéaéa".count("é"))
print("aéaéa".count("a", 1, 4))
print("é" in s, "z" in s)
"#,
    );
}

#[test]
fn startswith_and_endswith_take_character_bounds() {
    matches_python(
        "affix-bounds",
        r#"
s = "héllo"
print(s.startswith("hé"), s.endswith("lo"))
print(s.startswith("é", 1))
print(s.startswith("l", 2, 4))
print(s.endswith("é", 0, 2))
"#,
    );
}

// ---------------------------------------------------------------------------
// Splitting, stripping, partitioning
// ---------------------------------------------------------------------------

#[test]
fn splitting_keeps_characters_intact() {
    matches_python(
        "split-chars",
        r#"
print("a,é,b".split(","))
print("aébéc".split("é"))
print("aébéc".rsplit("é", 1))
print("é λ 日".split())
print("héllo".partition("l"))
print("héllo".rpartition("l"))
print("aébéc".partition("é"))
print("a\né\nb".splitlines())
print(list("héllo"))
"#,
    );
}

#[test]
fn stripping_a_character_set_never_splits_a_character() {
    matches_python(
        "strip-chars",
        r#"
print("xéx".strip("x"))
print("ééaéé".strip("é"))
print("éaé".lstrip("é"), "éaé".rstrip("é"))
print("🐍a🐍".strip("🐍"))
print("aébé".strip("éb"))
print("  é  ".strip())
"#,
    );
}

// ---------------------------------------------------------------------------
// Width-based operations
// ---------------------------------------------------------------------------

#[test]
fn padding_widths_count_characters() {
    matches_python(
        "pad-widths",
        r#"
e = "é"
for got in [e.center(5, "*"), e.ljust(4, "-"), e.rjust(4, "-"),
            "héllo".center(9), e.zfill(4), "-é".zfill(5),
            "🐍".center(5, "."), e.center(5, "λ")]:
    print(len(got), got)
"#,
    );
}

#[test]
fn expandtabs_counts_columns_in_characters() {
    matches_python(
        "expandtabs",
        r#"
for got in ["a\té".expandtabs(4), "é\tb".expandtabs(4),
            "éé\tb".expandtabs(4), "a\né\tb".expandtabs(8)]:
    print(len(got), got)
"#,
    );
}

#[test]
fn format_spec_widths_and_precision_count_characters() {
    matches_python(
        "format-widths",
        r#"
s = "héllo"
print(f"[{s:>8}]")
print(f"[{s:<8}]")
print(f"[{s:^9}]")
print(f"[{s:.3}]")
print(f"[{s:8.2}]")
print(f"[{'🐍':>4}]")
"#,
    );
}

// ---------------------------------------------------------------------------
// Replace, join, translate
// ---------------------------------------------------------------------------

#[test]
fn replace_and_join_preserve_characters() {
    matches_python(
        "replace-join",
        r#"
print("héllo".replace("é", "e"))
print("héllo".replace("l", "λ"))
print("aébéc".replace("é", ""))
print("ab".replace("", "-"))
print("é".replace("", "*"))
print("--".join(["é", "λ", "🐍"]))
print("é".join(["a", "b", "c"]))
print(len("é".join(["a", "b"])))
"#,
    );
}

#[test]
fn translate_maps_code_points() {
    matches_python(
        "translate",
        r#"
table = str.maketrans("éλ", "elc"[0:2])
print("héllo λx".translate(table))
print("aéb".translate(str.maketrans("é", "z")))
"#,
    );
}

// ---------------------------------------------------------------------------
// ord / chr and round trips
// ---------------------------------------------------------------------------

#[test]
fn ord_and_chr_round_trip_every_width() {
    matches_python(
        "ord-chr",
        r#"
for cp in [65, 233, 955, 128013, 0x10FFFF]:
    c = chr(cp)
    print(cp, len(c), ord(c), c == chr(ord(c)))
print(ord("é"), ord("🐍"))
try:
    print(ord("ab"))
except TypeError as e:
    print("TypeError")
"#,
    );
}

#[test]
fn concatenation_and_repetition_track_the_character_count() {
    matches_python(
        "concat-repeat",
        r#"
a = "hé"
b = "🐍o"
print(a + b, len(a + b))
print(a * 3, len(a * 3))
print(len(a * 0))
print((a + b)[2], (a + b)[1:3])
"#,
    );
}

// ---------------------------------------------------------------------------
// I/O round trip
// ---------------------------------------------------------------------------

#[test]
fn file_round_trip_preserves_characters() {
    let dir = TempDir(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("pyrs-unicode-io-{}", std::process::id())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let data = dir
        .0
        .join("t.txt")
        .display()
        .to_string()
        .replace('\\', "\\\\");
    let source = format!(
        r#"
path = "{data}"
f = open(path, "w")
f.write("héllo 🐍\n")
f.write("λx\n")
f.close()
g = open(path, "r")
text = g.read()
g.close()
print(len(text))
print(text[6], text[0:5])
h = open(path, "r")
for line in h:
    print(len(line), line.strip())
h.close()
"#
    );
    let src = dir.0.join("prog.py");
    fs::write(&src, &source).unwrap();

    let expected = Command::new("python3").arg(&src).output().unwrap();
    assert!(
        expected.status.success(),
        "CPython failed: {}",
        String::from_utf8_lossy(&expected.stderr)
    );
    for opt in ["0", "2", "3"] {
        let actual = Command::new(PYRS)
            .args(["run", "-O", opt, "-i"])
            .arg(&src)
            .output()
            .unwrap();
        assert!(
            actual.status.success(),
            "PyRs failed at -O{opt}:\n{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&actual.stdout),
            String::from_utf8_lossy(&expected.stdout),
            "stdout differs at -O{opt}"
        );
    }
}

// ---------------------------------------------------------------------------
// ASCII must be untouched
// ---------------------------------------------------------------------------

#[test]
fn ascii_strings_behave_exactly_as_before() {
    matches_python(
        "ascii-unchanged",
        r#"
s = "hello world"
print(len(s), s[0], s[-1], s[2:5], s[::-1], s[::2])
print(s.find("o"), s.rfind("o"), s.count("l"), s.index("w"))
print(s.upper(), s.lower(), s.title(), s.capitalize(), s.swapcase())
print(s.split(), s.replace("l", "L"), s.strip("hd"))
print(s.center(15, "*"), s.zfill(15), f"{s:>15}")
print(s.startswith("hel"), s.endswith("rld"), s.partition(" "))
print("-".join(["a", "b"]), "a\tb".expandtabs(4))
"#,
    );
}

// ---------------------------------------------------------------------------
// Unicode case transforms and character classes
//
// Driven by the tables in codegen/runtime/unicode_data.c, generated from the
// CPython oracle by scripts/gen_unicode_tables.py. Before 0.91 these were
// ASCII-only: `"ß".upper()` was `"ß"`, `"naïve".upper()` was `"NAïVE"`, and
// `"é".isalpha()` was False.
// ---------------------------------------------------------------------------

#[test]
fn upper_and_lower_follow_unicode() {
    matches_python(
        "case-basic",
        r#"
print("naïve café".upper())
print("ÉCOLE".lower())
print("λx".upper(), "ΛX".lower())
print("Ünïcödé".upper(), "ÜNÏCÖDÉ".lower())
print("日本語".upper(), "日本語".lower())
print("hello".upper(), "HELLO".lower())
print("".upper(), "".lower())
"#,
    );
}

#[test]
fn case_mapping_may_change_length() {
    // U+00DF upper-cases to two characters, U+FB01 to two, U+0390 to three.
    matches_python(
        "case-expanding",
        r#"
for s in ["ß", "ﬁ", "ΐ", "ﬄ", "ǰ", "ẞ"]:
    u = s.upper()
    print(len(s), len(u), u)
print("straße".upper(), len("straße".upper()))
print("ßß".upper())
"#,
    );
}

#[test]
fn casefold_folds_beyond_lowercasing() {
    matches_python(
        "casefold",
        r#"
print("ß".casefold(), "ss".casefold())
print("ß".casefold() == "ss".casefold())
print("ẞ".casefold(), "Σ".casefold(), "ς".casefold())
print("ΣΊΣΥΦΟΣ".casefold())
print("HELLO".casefold())
"#,
    );
}

#[test]
fn title_and_capitalize_use_titlecase_and_word_boundaries() {
    matches_python(
        "case-title",
        r#"
print("héllo wörld".title())
print("ǅungla".title(), "ǅungla".capitalize())
print("ǆ".title(), "ǆ".upper(), "ǆ".lower())
print("hello world".title(), "hello world".capitalize())
print("a'b c-d".title())
print("ÉCOLE normale".title(), "ÉCOLE normale".capitalize())
print("".title(), "".capitalize())
"#,
    );
}

#[test]
fn swapcase_follows_unicode_case() {
    matches_python(
        "case-swap",
        r#"
print("héllo WÖRLD".swapcase())
print("λΛ".swapcase())
print("ǅ".swapcase())
print("123 日本".swapcase())
"#,
    );
}

#[test]
fn dotted_and_dotless_i_match_cpython() {
    matches_python(
        "case-turkish",
        r#"
print(len("İ".lower()), "İ".lower() == "i")
print("ı".upper(), "I".lower())
print("İ".upper(), "ı".title())
"#,
    );
}

#[test]
fn alpha_digit_and_numeric_classes_follow_unicode() {
    matches_python(
        "class-alpha",
        r#"
for s in ["é", "λ", "日", "abc", "a1", "1", "²", "½", "٣", "Ⅷ", "", " "]:
    print(s, s.isalpha(), s.isdigit(), s.isdecimal(), s.isnumeric(), s.isalnum())
"#,
    );
}

#[test]
fn space_and_printable_classes_follow_unicode() {
    matches_python(
        "class-space",
        r#"
nbsp = chr(0x00A0)
enq = chr(0x2003)
zwsp = chr(0x200B)
for s in [" ", "\t", nbsp, enq, zwsp, "a", ""]:
    print(s.isspace(), s.isprintable())
print("a b".isprintable(), (chr(7)).isprintable())
"#,
    );
}

#[test]
fn case_predicates_handle_titlecase_characters() {
    matches_python(
        "class-case",
        r#"
for s in ["é", "É", "ǅ", "Ǆ", "ǆ", "aB", "AB", "ab", "1", "", "É1"]:
    print(s.isupper(), s.islower(), s.istitle())
print("Hello World".istitle(), "Héllo Wörld".istitle(), "HELLO".istitle())
print("ǅungla".istitle())
"#,
    );
}

#[test]
fn isidentifier_accepts_unicode_identifiers() {
    matches_python(
        "class-ident",
        r#"
for s in ["café", "π", "_x", "x1", "2x", "a-b", "", "日本語", "ᵃ", "x" + chr(0x0301)]:
    print(s.isidentifier())
"#,
    );
}

#[test]
fn whitespace_stripping_and_splitting_follow_unicode() {
    matches_python(
        "space-strip",
        r#"
nbsp = chr(0x00A0)
enq = chr(0x2003)
s = enq + "a" + enq + "b" + enq
print(s.strip())
print(s.split())
print(s.lstrip(), s.rstrip())
print((nbsp + "x").strip())
print(("a" + enq + "b").split())
print(("a" + nbsp + "b").split(), len(("a" + nbsp + "b").split()))
"#,
    );
}

#[test]
fn splitlines_uses_the_unicode_boundary_set() {
    matches_python(
        "space-lines",
        r#"
s = "a\nb" + chr(0x2028) + "c" + chr(0x0085) + "d"
print(s.splitlines())
print(s.splitlines(True))
print("a\r\nb".splitlines())
print(("a" + chr(0x0B) + "b").splitlines())
"#,
    );
}

#[test]
fn repr_escapes_by_unicode_printability() {
    matches_python(
        "repr-printable",
        r#"
zwsp = chr(0x200B)
for s in ["café", "λ", "🐍", zwsp, chr(7), chr(0x0085)]:
    print(f"{s!r}")
print([chr(0x200B), "café"])
"#,
    );
}

// ---------------------------------------------------------------------------
// String literal escapes
//
// The lexer previously recognised only \n \t \r \0 \\ \' \", so `"\x00"`
// survived as the four characters `\`, `x`, `0`, `0` and `len` reported 4.
// ---------------------------------------------------------------------------

#[test]
fn numeric_escapes_decode_to_code_points() {
    matches_python(
        "escape-numeric",
        r#"
print("\x41\x42", "é", "\U0001F40D")
print(len("\x00"), len("a\x00b"), len("\U0001F40D"))
print("é" == chr(0xE9), "\U0001F40D" == chr(0x1F40D))
print("\xe9".upper(), len("中文"))
"#,
    );
}

#[test]
fn octal_and_control_escapes_decode() {
    matches_python(
        "escape-octal",
        r#"
print("\101\102", "\0" == chr(0))
print("\a" == chr(7), "\b" == chr(8), "\f" == chr(12), "\v" == chr(11))
print(len("\t\n\r"))
"#,
    );
}

#[test]
fn unknown_escapes_stay_verbatim() {
    matches_python(
        "escape-unknown",
        r#"
print("\\d" == "\\" + "d")
print(len("\\q"), "\\q"[0] == "\\")
"#,
    );
}

#[test]
fn escapes_decode_inside_triple_quoted_and_f_strings() {
    matches_python(
        "escape-contexts",
        "\ns = \"\"\"a\\x41b\"\"\"\nprint(s, len(s))\nn = 5\nprint(f\"\\u00e9{n}\\x21\")\n",
    );
}
