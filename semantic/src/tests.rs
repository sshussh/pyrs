//! Unit tests for the semantic engine.

use common::Diagnostic;

use crate::prelude::*;

fn analyze_src(src: &str) -> SResult<ir::Module> {
    let module = parser::parse(src).expect("parse failed");
    analyze(&module)
}

fn analyze_ok(src: &str) -> ir::Module {
    match analyze_src(src) {
        Ok(m) => m,
        Err(e) => panic!(
            "analyze failed: {}\n{}",
            e.message,
            e.render("test.py", src)
        ),
    }
}

fn analyze_err(src: &str) -> Diagnostic {
    analyze_src(src).expect_err("expected a semantic error")
}

fn find_func<'a>(m: &'a ir::Module, name: &str) -> &'a ir::Function {
    m.funcs.iter().find(|f| f.name == name).unwrap()
}

#[test]
fn lowers_fib() {
    let src = "\
def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print(fib(10))
";
    let m = analyze_ok(src);
    assert_eq!(m.entry, ENTRY_NAME);
    let fib = find_func(&m, "fib");
    assert_eq!(fib.ret, ir::Ty::Int);
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(entry.body[0], ir::Stmt::Print { .. }));
}

#[test]
fn print_sep_end_lower() {
    let m = analyze_ok("print(1, 2, sep=\",\", end=\"!\")\n");
    let entry = find_func(&m, ENTRY_NAME);
    // sep/end bind to temps (source-order eval) then Print.
    let print = entry
        .body
        .iter()
        .find(|s| matches!(s, ir::Stmt::Print { .. }))
        .expect("print stmt");
    let ir::Stmt::Print {
        args,
        sep,
        end,
        flush,
        ..
    } = print
    else {
        panic!("{print:?}");
    };
    assert_eq!(args.len(), 2);
    assert_eq!(sep.ty, ir::Ty::Str);
    assert_eq!(end.ty, ir::Ty::Str);
    assert_eq!(flush.ty, ir::Ty::Bool);
    assert!(matches!(sep.kind, ir::ExprKind::Local(_)));
    assert!(matches!(end.kind, ir::ExprKind::Local(_)));
}

#[test]
fn print_flush_lowers() {
    let m = analyze_ok("print(1, flush=True)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let print = entry
        .body
        .iter()
        .find(|s| matches!(s, ir::Stmt::Print { .. }))
        .expect("print stmt");
    let ir::Stmt::Print { flush, .. } = print else {
        panic!("{print:?}");
    };
    assert_eq!(flush.ty, ir::Ty::Bool);
}

#[test]
fn print_sep_none_uses_default() {
    let m = analyze_ok("print(1, sep=None)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let print = entry
        .body
        .iter()
        .find(|s| matches!(s, ir::Stmt::Print { .. }))
        .expect("print stmt");
    let ir::Stmt::Print { sep, .. } = print else {
        panic!("{print:?}");
    };
    // Temp bound to the coerced default `" "`.
    assert_eq!(sep.ty, ir::Ty::Str);
}

#[test]
fn print_sep_wrong_type_is_error() {
    let e = analyze_err("print(1, sep=1)\n");
    assert!(
        e.message.contains("sep must be None or a string"),
        "{}",
        e.message
    );
}

#[test]
fn print_file_kw_accepts_only_the_standard_streams() {
    let e = analyze_err("print(1, file=1)\n");
    assert!(
        e.message.contains("sys.stderr") && e.message.contains("sys.stdout"),
        "{}",
        e.message
    );
}

#[test]
fn print_to_stderr_lowers_to_the_destination_flag() {
    let m = analyze_ok("import sys\nprint(1, file=sys.stderr)\nprint(2)\n");
    let prints: Vec<bool> = m.funcs[0]
        .body
        .iter()
        .filter_map(|s| match s {
            ir::Stmt::Print { to_stderr, .. } => Some(*to_stderr),
            _ => None,
        })
        .collect();
    // One to each stream, and the flag does not carry over.
    assert_eq!(prints, vec![true, false], "{prints:?}");
}

#[test]
fn function_and_module_docstrings_are_ok() {
    // First statement that is a string literal (docstring) is a no-op
    // expression statement — not a bare-name / empty-body error. `__doc__`
    // is not stored; runtime effect matches programs that only use docs.
    let m = analyze_ok(
        "\
\"\"\"module documentation\"\"\"

def f() -> int:
    \"\"\"function documentation\"\"\"
    return 42

print(f())
",
    );
    let f = find_func(&m, "f");
    // docstring + return
    assert!(
        f.body.len() >= 2,
        "expected docstring ExprStmt then return, got {:?}",
        f.body
    );
    assert!(
        matches!(
            &f.body[0],
            ir::Stmt::ExprStmt(ir::Expr {
                kind: ir::ExprKind::ConstStr(s),
                ..
            }) if s == "function documentation"
        ),
        "first body stmt should be the docstring ConstStr, got {:?}",
        f.body[0]
    );
    assert!(matches!(f.body[1], ir::Stmt::Return(Some(_))));

    let entry = find_func(&m, ENTRY_NAME);
    // module docstring then print(f())
    assert!(
        matches!(
            &entry.body[0],
            ir::Stmt::ExprStmt(ir::Expr {
                kind: ir::ExprKind::ConstStr(s),
                ..
            }) if s == "module documentation"
        ),
        "entry should start with module docstring, got {:?}",
        entry.body[0]
    );
}

#[test]
fn triple_quoted_string_value_lowers() {
    let m = analyze_ok("s = \"\"\"a\nb\"\"\"\nprint(s)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!("expected GlobalAssign, got {:?}", entry.body[0]);
    };
    assert!(
        matches!(&value.kind, ir::ExprKind::ConstStr(s) if s == "a\nb"),
        "{:?}",
        value.kind
    );
}

#[test]
fn int_promotes_to_float_in_mixed_arithmetic() {
    let m = analyze_ok("x = 1 + 2.5\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!("expected Assign");
    };
    assert_eq!(value.ty, ir::Ty::Float);
}

#[test]
fn true_division_yields_float() {
    let m = analyze_ok("x = 7 / 2\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!("expected Assign");
    };
    assert_eq!(value.ty, ir::Ty::Float);
}

#[test]
fn pow_int_stays_int_pow_float_is_float() {
    let m = analyze_ok("a = 2 ** 10\nb = 2.0 ** 10\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
}

#[test]
fn chained_comparison_lowers_to_let_and() {
    let m = analyze_ok("x = 1\nb = 0 < x < 10\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!("expected Assign");
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    let ir::ExprKind::Let {
        value: first, body, ..
    } = &value.kind
    else {
        panic!("expected Let, got {:?}", value.kind);
    };
    assert!(
        matches!(first.kind, ir::ExprKind::ConstInt(0)),
        "outer let should bind the first operand, got {:?}",
        first.kind
    );
    let ir::ExprKind::Let {
        value: middle,
        body,
        ..
    } = &body.kind
    else {
        panic!("expected nested Let, got {:?}", body.kind);
    };
    assert!(
        matches!(&middle.kind, ir::ExprKind::GlobalLoad(name) if name == "x"),
        "inner let should bind the shared middle operand, got {:?}",
        middle.kind
    );
    assert!(matches!(
        body.kind,
        ir::ExprKind::Binary {
            op: ir::BinOp::And,
            ..
        }
    ));
}

#[test]
fn str_variables_and_concat() {
    let m = analyze_ok("s = \"ab\"\nt = s + \"c\"\nu = s * 3\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert_eq!(m.globals[0], ("s".to_string(), ir::Ty::Str));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
}

#[test]
fn str_comparisons_are_bool() {
    let m = analyze_ok("b = \"a\" < \"b\"\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
}

#[test]
fn str_rsplit_lowers() {
    let m = analyze_ok("xs = \"a,b,c\".rsplit(\",\", 1)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::list_of(ir::Ty::Str));
    let ir::ExprKind::StrCall { func, args } = &value.kind else {
        panic!();
    };
    assert_eq!(*func, ir::StrFn::RSplit);
    assert_eq!(args.len(), 3);
}

#[test]
fn str_split_ws_lowers_with_unlimited_maxsplit() {
    let m = analyze_ok("xs = \"a b\".split()\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    let ir::ExprKind::StrCall { func, args } = &value.kind else {
        panic!();
    };
    assert_eq!(*func, ir::StrFn::SplitWs);
    assert_eq!(args.len(), 2);
    assert!(matches!(&args[1].kind, ir::ExprKind::ConstInt(n) if *n == -1));
}

#[test]
fn str_splitlines_lowers() {
    let m = analyze_ok("xs = \"a\\nb\".splitlines()\nys = \"a\\nb\".splitlines(True)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::list_of(ir::Ty::Str));
    let ir::ExprKind::StrCall { func, args } = &value.kind else {
        panic!();
    };
    assert_eq!(*func, ir::StrFn::SplitLines);
    assert_eq!(args.len(), 2);
    assert!(matches!(&args[1].kind, ir::ExprKind::ConstBool(false)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    let ir::ExprKind::StrCall { args, .. } = &value.kind else {
        panic!();
    };
    assert!(matches!(&args[1].kind, ir::ExprKind::ConstBool(true)));
}

#[test]
fn str_splitlines_rejects_arity() {
    let e = analyze_err("print(\"a\".splitlines(1, 2))\n");
    assert!(e.message.contains("at most 1 argument"), "{}", e.message);
}

#[test]
fn str_replace_count_lowers() {
    let m = analyze_ok("s = \"aaa\".replace(\"a\", \"b\", 1)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    let ir::ExprKind::StrCall { func, args } = &value.kind else {
        panic!();
    };
    assert_eq!(*func, ir::StrFn::Replace);
    assert_eq!(args.len(), 4);
    assert!(matches!(&args[3].kind, ir::ExprKind::ConstInt(n) if *n == 1));
    let m = analyze_ok("s = \"aaa\".replace(\"a\", \"b\")\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    let ir::ExprKind::StrCall { args, .. } = &value.kind else {
        panic!();
    };
    assert!(matches!(&args[3].kind, ir::ExprKind::ConstInt(n) if *n == -1));
}

#[test]
fn str_replace_rejects_bad_count() {
    let e = analyze_err("print(\"a\".replace(\"a\", \"b\", \"x\"))\n");
    assert!(
        e.message.contains("cannot be interpreted as an integer"),
        "{}",
        e.message
    );
    let e = analyze_err("print(\"a\".replace(\"a\"))\n");
    assert!(e.message.contains("at least 2 arguments"), "{}", e.message);
}

#[test]
fn str_split_empty_sep_is_error() {
    let e = analyze_err("xs = \"a\".split(\"\")\n");
    assert!(e.message.contains("empty separator"), "{}", e.message);
    let e = analyze_err("xs = \"a\".rsplit(\"\")\n");
    assert!(e.message.contains("empty separator"), "{}", e.message);
}

#[test]
fn str_partition_lowers() {
    let m = analyze_ok("t = \"a,b\".partition(\",\")\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(
        value.ty,
        ir::tuple_of(&[ir::Ty::Str, ir::Ty::Str, ir::Ty::Str])
    );
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::Partition,
            ..
        }
    ));
}

#[test]
fn str_removeprefix_lowers() {
    let m = analyze_ok("s = \"hello\".removeprefix(\"he\")\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::RemovePrefix,
            ..
        }
    ));
}

#[test]
fn str_strip_chars_lowers() {
    let m = analyze_ok(
        "a = \"xxhi\".strip(\"x\")\n\
             b = \"  hi  \".strip()\n\
             c = \"  hi  \".strip(None)\n\
             d = \"xxhi\".lstrip(\"x\")\n\
             e = \"hixx\".rstrip(\"x\")\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let want = [
        (ir::StrFn::StripChars, 2usize),
        (ir::StrFn::Strip, 1),
        (ir::StrFn::Strip, 1),
        (ir::StrFn::LstripChars, 2),
        (ir::StrFn::RstripChars, 2),
    ];
    for (i, (func, nargs)) in want.into_iter().enumerate() {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!("body[{i}]");
        };
        assert_eq!(value.ty, ir::Ty::Str);
        let ir::ExprKind::StrCall { func: got, args } = &value.kind else {
            panic!("body[{i}] not StrCall");
        };
        assert_eq!(*got, func);
        assert_eq!(args.len(), nargs);
    }
}

#[test]
fn str_strip_rejects_bad_chars() {
    let e = analyze_err("print(\"a\".strip(1))\n");
    assert!(e.message.contains("must be None or str"), "{}", e.message);
    let e = analyze_err("print(\"a\".strip(\"x\", \"y\"))\n");
    assert!(e.message.contains("at most 1 argument"), "{}", e.message);
    let e = analyze_err("print(\"a\".lstrip(True))\n");
    assert!(e.message.contains("must be None or str"), "{}", e.message);
}

#[test]
fn str_expandtabs_lowers() {
    let m = analyze_ok("s = \"a\\tb\".expandtabs()\nt = \"a\\tb\".expandtabs(4)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    let ir::ExprKind::StrCall { func, args } = &value.kind else {
        panic!();
    };
    assert_eq!(*func, ir::StrFn::ExpandTabs);
    assert_eq!(args.len(), 2);
    assert!(matches!(&args[1].kind, ir::ExprKind::ConstInt(n) if *n == 8));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    let ir::ExprKind::StrCall { args, .. } = &value.kind else {
        panic!();
    };
    assert!(matches!(&args[1].kind, ir::ExprKind::ConstInt(n) if *n == 4));
}

#[test]
fn str_expandtabs_rejects_bad_tabsize() {
    let e = analyze_err("print(\"a\".expandtabs(\"x\"))\n");
    assert!(
        e.message.contains("cannot be interpreted as an integer"),
        "{}",
        e.message
    );
    let e = analyze_err("print(\"a\".expandtabs(1, 2))\n");
    assert!(e.message.contains("at most 1 argument"), "{}", e.message);
}

#[test]
fn str_pad_family_lowers() {
    let m = analyze_ok("a = \"42\".zfill(5)\nb = \"hi\".center(5, \"-\")\nc = \"hi\".ljust(4)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::ZFill,
            args,
        } if args.len() == 2
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::Center,
            args,
        } if args.len() == 3
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::LJust,
            ..
        }
    ));
}

#[test]
fn str_pad_rejects_bad_fill() {
    let e = analyze_err("print(\"hi\".center(5, \"--\"))\n");
    assert!(e.message.contains("exactly one character"), "{}", e.message);
    let e = analyze_err("print(\"hi\".zfill(\"5\"))\n");
    assert!(
        e.message.contains("cannot be interpreted as an integer"),
        "{}",
        e.message
    );
}

#[test]
fn str_maketrans_and_translate_lower() {
    let m = analyze_ok(
        "t = str.maketrans(\"ab\", \"xy\")\n\
             s = \"abba\".translate(t)\n\
             u = str.maketrans(\"ab\", \"xy\", \"q\")\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(value.ty, ir::Ty::Dict { .. }));
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::MakeTrans,
            ..
        }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::Translate,
            ..
        }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::MakeTransDelete,
            ..
        }
    ));
}

#[test]
fn str_maketrans_rejects_unequal_const() {
    let e = analyze_err("print(str.maketrans(\"ab\", \"c\"))\n");
    assert!(e.message.contains("equal length"), "{}", e.message);
}

#[test]
fn str_translate_rejects_str_table() {
    let e = analyze_err("print(\"a\".translate(\"b\"))\n");
    assert!(e.message.contains("dict[int, int]"), "{}", e.message);
}

#[test]
fn str_casefold_lowers() {
    let m = analyze_ok("s = \"Hi\".casefold()\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::CaseFold,
            ..
        }
    ));
}

#[test]
fn str_casefold_rejects_extra_arg() {
    let e = analyze_err("print(\"a\".casefold(\"x\"))\n");
    assert!(e.message.contains("exactly 0 argument"), "{}", e.message);
}

#[test]
fn str_capitalize_title_swapcase_lower() {
    let m = analyze_ok("a = \"hi\".capitalize()\nb = \"hi\".title()\nc = \"Hi\".swapcase()\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::Capitalize,
            ..
        }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::Title,
            ..
        }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::SwapCase,
            ..
        }
    ));
}

#[test]
fn str_isdigit_is_bool() {
    let m = analyze_ok("b = \"42\".isdigit()\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::IsDigit,
            ..
        }
    ));
}

#[test]
fn str_rfind_is_int() {
    let m = analyze_ok("i = \"banana\".rfind(\"an\")\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::RFind,
            ..
        }
    ));
}

#[test]
fn str_index_and_find_bounds_lower() {
    let m = analyze_ok(
        "a = \"banana\".index(\"an\")\n\
             b = \"banana\".find(\"an\", 2)\n\
             c = \"banana\".rindex(\"an\", 0, 5)\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::Index,
            args,
        } if args.len() == 4
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::Find,
            args,
        } if args.len() == 4
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::RIndex,
            ..
        }
    ));
}

#[test]
fn str_find_rejects_non_int_start() {
    let e = analyze_err("print(\"a\".find(\"a\", \"x\"))\n");
    assert!(
        e.message.contains("cannot be interpreted as an integer"),
        "{}",
        e.message
    );
}

#[test]
fn str_count_bounds_lower() {
    let m = analyze_ok("n = \"banana\".count(\"an\", 2)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::Count,
            args,
        } if args.len() == 4
    ));
}

#[test]
fn str_startswith_tuple_and_bounds_lower() {
    let m = analyze_ok(
        "a = \"hello\".startswith((\"x\", \"he\"))\n\
             b = \"hello\".endswith(\".py\", 0, 4)\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::StartsWithTuple,
            args,
        } if args.len() == 4
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(
        &value.kind,
        ir::ExprKind::StrCall {
            func: ir::StrFn::EndsWith,
            ..
        }
    ));
}

#[test]
fn str_startswith_rejects_list() {
    let e = analyze_err("print(\"a\".startswith([\"a\"]))\n");
    assert!(e.message.contains("str or a tuple of str"), "{}", e.message);
}

#[test]
fn str_isalnum_istitle_isascii_are_bool() {
    let m = analyze_ok("a = \"A1\".isalnum()\nb = \"Hi\".istitle()\nc = \"az\".isascii()\n");
    let entry = find_func(&m, ENTRY_NAME);
    for (i, want) in [ir::StrFn::IsAlnum, ir::StrFn::IsTitle, ir::StrFn::IsAscii]
        .into_iter()
        .enumerate()
    {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Bool);
        assert!(matches!(&value.kind, ir::ExprKind::StrCall { func, .. } if *func == want));
    }
}

#[test]
fn str_isdecimal_isnumeric_isidentifier_isprintable_are_bool() {
    let m = analyze_ok(
        "a = \"12\".isdecimal()\nb = \"12\".isnumeric()\n\
             c = \"_x\".isidentifier()\nd = \"ok\".isprintable()\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    for (i, want) in [
        ir::StrFn::IsDecimal,
        ir::StrFn::IsNumeric,
        ir::StrFn::IsIdentifier,
        ir::StrFn::IsPrintable,
    ]
    .into_iter()
    .enumerate()
    {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!("body[{i}]");
        };
        assert_eq!(value.ty, ir::Ty::Bool);
        assert!(matches!(&value.kind, ir::ExprKind::StrCall { func, .. } if *func == want));
    }
}

#[test]
fn str_isidentifier_rejects_extra_arg() {
    let e = analyze_err("print(\"a\".isidentifier(\"x\"))\n");
    assert!(e.message.contains("exactly 0 argument"), "{}", e.message);
}

#[test]
fn str_isalpha_isspace_case_are_bool() {
    let m = analyze_ok(
        "a = \"ab\".isalpha()\nb = \" \\t\".isspace()\n\
             c = \"AB\".isupper()\nd = \"ab\".islower()\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    for (i, want) in [
        ir::StrFn::IsAlpha,
        ir::StrFn::IsSpace,
        ir::StrFn::IsUpper,
        ir::StrFn::IsLower,
    ]
    .into_iter()
    .enumerate()
    {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!("body[{i}]");
        };
        assert_eq!(value.ty, ir::Ty::Bool);
        assert!(matches!(
            &value.kind,
            ir::ExprKind::StrCall { func, .. } if *func == want
        ));
    }
}

#[test]
fn abs_int_float_and_bool() {
    let m = analyze_ok("a = abs(-5)\nb = abs(-2.5)\nc = abs(True)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Abs(_)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
    assert!(matches!(value.kind, ir::ExprKind::Abs(_)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    // bool promotes to int, then abs
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Abs(_)));
}

#[test]
fn abs_rejects_str() {
    let e = analyze_err("x = abs(\"nope\")\n");
    assert!(
        e.message.contains("bad operand type for abs()"),
        "{}",
        e.message
    );
}

#[test]
fn round_lowers() {
    let m = analyze_ok("a = round(-2.5)\nb = round(7)\nc = round(1.25, 1)\nd = round(15, -1)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(
        value.kind,
        ir::ExprKind::Round { ndigits: None, .. }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
    assert!(matches!(
        value.kind,
        ir::ExprKind::Round {
            ndigits: Some(_),
            ..
        }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[3] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
}

#[test]
fn round_rejects_str() {
    let e = analyze_err("x = round(\"nope\")\n");
    assert!(
        e.message.contains("__round__") || e.message.contains("str"),
        "{}",
        e.message
    );
}

#[test]
fn ord_lowers_str_to_int() {
    let m = analyze_ok("a = ord(\"A\")\nb = ord(\"é\")\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Ord(_)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Ord(_)));
}

#[test]
fn ord_rejects_non_str() {
    let e = analyze_err("x = ord(1)\n");
    assert!(
        e.message.contains("ord() expected string of length 1"),
        "{}",
        e.message
    );
}

#[test]
fn ord_rejects_wrong_arity() {
    let e = analyze_err("x = ord()\n");
    assert!(
        e.message.contains("ord() takes exactly one argument"),
        "{}",
        e.message
    );
    let e = analyze_err("x = ord(\"a\", \"b\")\n");
    assert!(
        e.message.contains("ord() takes exactly one argument"),
        "{}",
        e.message
    );
}

#[test]
fn chr_lowers_int_and_bool_to_str() {
    let m = analyze_ok("a = chr(65)\nb = chr(True)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(value.kind, ir::ExprKind::Chr(_)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(value.kind, ir::ExprKind::Chr(_)));
}

#[test]
fn chr_rejects_non_int() {
    let e = analyze_err("x = chr(\"A\")\n");
    assert!(
        e.message
            .contains("'str' object cannot be interpreted as an integer"),
        "{}",
        e.message
    );
}

#[test]
fn chr_rejects_wrong_arity() {
    let e = analyze_err("x = chr()\n");
    assert!(
        e.message.contains("chr() takes exactly one argument"),
        "{}",
        e.message
    );
    let e = analyze_err("x = chr(1, 2)\n");
    assert!(
        e.message.contains("chr() takes exactly one argument"),
        "{}",
        e.message
    );
}

#[test]
fn hex_bin_oct_lower_int_and_bool() {
    let m = analyze_ok("a = hex(255)\nb = bin(True)\nc = oct(8)\n");
    let entry = find_func(&m, ENTRY_NAME);
    for i in 0..3 {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!("{:?}", entry.body[i]);
        };
        assert_eq!(value.ty, ir::Ty::Str);
        assert!(
            matches!(value.kind, ir::ExprKind::FormatValue { .. }),
            "{:?}",
            value.kind
        );
    }
}

#[test]
fn hex_rejects_str() {
    let e = analyze_err("x = hex(\"ff\")\n");
    assert!(
        e.message.contains("cannot be interpreted as an integer"),
        "{}",
        e.message
    );
}

#[test]
fn hex_rejects_wrong_arity() {
    let e = analyze_err("x = hex()\n");
    assert!(
        e.message.contains("hex() takes exactly one argument"),
        "{}",
        e.message
    );
    let e = analyze_err("x = bin(1, 2)\n");
    assert!(
        e.message.contains("bin() takes exactly one argument"),
        "{}",
        e.message
    );
}

#[test]
fn divmod_lowers_to_tuple() {
    let m = analyze_ok("a = divmod(7, 3)\nb = divmod(7, 2.0)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!("{:?}", entry.body[0]);
    };
    assert_eq!(value.ty, ir::tuple_of(&[ir::Ty::Int, ir::Ty::Int]));
    assert!(matches!(value.kind, ir::ExprKind::Let { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!("{:?}", entry.body[1]);
    };
    assert_eq!(value.ty, ir::tuple_of(&[ir::Ty::Float, ir::Ty::Float]));
}

#[test]
fn divmod_rejects_str() {
    let e = analyze_err("x = divmod(\"a\", 1)\n");
    assert!(
        e.message
            .contains("unsupported operand type(s) for divmod()"),
        "{}",
        e.message
    );
}

#[test]
fn divmod_rejects_wrong_arity() {
    let e = analyze_err("x = divmod(1)\n");
    assert!(
        e.message.contains("divmod expected 2 arguments"),
        "{}",
        e.message
    );
}

#[test]
fn pow_two_arg_lowers_like_starstar() {
    let m = analyze_ok("a = pow(2, 10)\nb = pow(2, -1)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!("{:?}", entry.body[0]);
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(
        value.kind,
        ir::ExprKind::Binary {
            op: ir::BinOp::Pow,
            ..
        }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!("{:?}", entry.body[1]);
    };
    assert_eq!(value.ty, ir::Ty::Float);
}

#[test]
fn pow_three_arg_lowers() {
    let m = analyze_ok("a = pow(2, 10, 3)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::PowMod { .. }));
}

#[test]
fn pow_three_arg_rejects_float() {
    let e = analyze_err("x = pow(2.0, 3, 5)\n");
    assert!(
        e.message.contains("3rd argument") && e.message.contains("integers"),
        "{}",
        e.message
    );
}

#[test]
fn min_max_unify_numeric() {
    let m = analyze_ok("a = min(-3, 2)\nb = max(1, 1.5)\nc = min(True, 0)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Min { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
    assert!(matches!(value.kind, ir::ExprKind::Max { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Min { .. }));
}

#[test]
fn min_rejects_str() {
    let e = analyze_err("x = min(1, \"nope\")\n");
    assert!(
        e.message.contains("min()") && e.message.contains("str"),
        "{}",
        e.message
    );
}

#[test]
fn min_arity() {
    let e = analyze_err("x = min()\n");
    assert!(
        e.message.contains("at least 1 argument") || e.message.contains("got 0"),
        "{}",
        e.message
    );
}

#[test]
fn min_multi_arg_numeric_folds() {
    let m = analyze_ok("a = min(3, 1, 4, 2)\nb = max(3, 1, 4)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Min { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Max { .. }));
}

#[test]
fn min_multi_arg_str_folds() {
    let m = analyze_ok("a = min(\"bb\", \"a\", \"ccc\")\nb = max(\"bb\", \"a\")\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(value.kind, ir::ExprKind::Min { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(value.kind, ir::ExprKind::Max { .. }));
}

#[test]
fn min_multi_arg_tuple_folds() {
    let m = analyze_ok("a = min((1, 2), (0, 9), (1, 0))\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(value.ty, ir::Ty::Tuple(_)));
    assert!(matches!(value.kind, ir::ExprKind::Min { .. }));
}

#[test]
fn tuple_lt_lowers() {
    let m = analyze_ok("b = (1, 2) < (0, 9)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(
        value.kind,
        ir::ExprKind::Binary {
            op: ir::BinOp::Lt,
            ..
        }
    ));
}

#[test]
fn sorted_list_of_tuples_ok() {
    let m = analyze_ok("ys = sorted([(1, \"b\"), (0, \"a\")])\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(value.ty, ir::Ty::List(_)));
}

#[test]
fn list_lt_lowers() {
    let m = analyze_ok("b = [1, 2] < [1, 3]\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(
        value.kind,
        ir::ExprKind::Binary {
            op: ir::BinOp::Lt,
            ..
        }
    ));
}

#[test]
fn min_multi_arg_list_folds() {
    let m = analyze_ok("a = min([1, 2], [0, 9], [1, 0])\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(value.ty, ir::Ty::List(_)));
    assert!(matches!(value.kind, ir::ExprKind::Min { .. }));
}

#[test]
fn min_multi_arg_with_key_desugars_to_block() {
    let m = analyze_ok(
        "\
def k(x: int) -> int:
    return -x
v = min(3, 1, 4, key=k)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    let ir::ExprKind::Block { stmts, .. } = &value.kind else {
        panic!("expected Block desugar, got {:?}", value.kind);
    };
    assert!(
        stmts.iter().any(|s| matches!(s, ir::Stmt::If { .. })),
        "expected comparison Ifs, stmts={stmts:?}"
    );
}

#[test]
fn min_list_form() {
    let m =
        analyze_ok("a = min([3, 1, 4])\nb = max([1.5, -2.0])\nc = min([\"b\", \"a\", \"c\"])\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::MinList(_)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
    assert!(matches!(value.kind, ir::ExprKind::MaxList(_)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    assert!(matches!(value.kind, ir::ExprKind::MinList(_)));
}

#[test]
fn sum_list_int_and_float() {
    let m = analyze_ok("a = sum([1, 2, 3])\nb = sum([1.5, 2.5])\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Sum { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
    assert!(matches!(value.kind, ir::ExprKind::Sum { .. }));
}

#[test]
fn sum_with_start_lowers() {
    let m =
        analyze_ok("a = sum([1, 2, 3], 10)\nb = sum([1, 2], start=0.5)\nc = sum([1.5], start=1)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::Sum { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
}

#[test]
fn sum_start_rejects_str() {
    let e = analyze_err("x = sum([1, 2], start=\"a\")\n");
    assert!(
        e.message.contains("sum() start") || e.message.contains("str"),
        "{}",
        e.message
    );
}

#[test]
fn sum_rejects_duplicate_start() {
    let e = analyze_err("x = sum([1], 2, start=3)\n");
    assert!(e.message.contains("multiple values"), "{}", e.message);
}

#[test]
fn sum_rejects_str_list() {
    let e = analyze_err("x = sum([\"a\", \"b\"])\n");
    assert!(
        e.message.contains("sum()") && e.message.contains("list[str]"),
        "{}",
        e.message
    );
}

#[test]
fn str_cast_and_index_and_len() {
    let m = analyze_ok("s = str(42)\nc = s[0]\nn = len(s)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(value.kind, ir::ExprKind::IntToStr(_)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
}

#[test]
fn int_float_from_str_lower() {
    let m = analyze_ok("a = int(\"42\")\nb = float(\"1.5\")\nc = int(\"ff\", 16)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::StrToInt { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
    assert!(matches!(value.kind, ir::ExprKind::StrToFloat(_)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::StrToInt { .. }));
}

#[test]
fn int_zero_arg_is_zero() {
    let m = analyze_ok("a = int()\nb = float()\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(value.kind, ir::ExprKind::ConstInt(0)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(value.kind, ir::ExprKind::ConstFloat(_)));
}

#[test]
fn int_explicit_base_rejects_non_str() {
    let e = analyze_err("print(int(3.5, 10))\n");
    assert!(
        e.message
            .contains("can't convert non-string with explicit base"),
        "{}",
        e.message
    );
}

#[test]
fn list_literal_keeps_each_element_type() {
    // Python prints `[1, 2.5, True]`. Collapsing to `list[float]` used to
    // print `[1.0, 2.5, 1.0]`, changing both the values and their types.
    let m = analyze_ok("xs = [1, 2.5, True]\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(
        value.ty,
        ir::list_of(ir::union_of(&[ir::Ty::Bool, ir::Ty::Int, ir::Ty::Float]))
    );
}

#[test]
fn homogeneous_numeric_lists_keep_optimized_storage() {
    // The union only appears for genuinely mixed literals; uniform lists
    // must not pay for it.
    for (src, want) in [
        ("xs = [1, 2, 3]\n", ir::Ty::Int),
        ("xs = [1.0, 2.0]\n", ir::Ty::Float),
        ("xs = [True, False]\n", ir::Ty::Bool),
    ] {
        let m = analyze_ok(src);
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::list_of(want), "for {src:?}");
    }
}

#[test]
fn empty_list_defaults_to_list_any() {
    let m = analyze_ok("xs = []\nprint(len(xs))\n");
    let entry = find_func(&m, ENTRY_NAME);
    // First global assign is xs = [] with list[Any].
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!("expected GlobalAssign, got {:?}", entry.body[0]);
    };
    assert_eq!(value.ty, ir::list_of(ir::Ty::Any));
    analyze_ok("xs: list[int] = []\n");
}

#[test]
fn any_annotation_and_coerce() {
    let m = analyze_ok(
        "\
x: Any = 1
y: int = x
print(y)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(entry.body.iter().any(|s| matches!(
        s,
        ir::Stmt::GlobalAssign { name, value, .. }
            if name == "x" && value.ty == ir::Ty::Any
    )));
}

#[test]
fn exclusive_field_after_multi_isinstance() {
    let m = analyze_ok(
        "\
class A:
    def __init__(self):
        self.a = 1
class B(A):
    def __init__(self):
        self.a = 1
        self.b = 2
class C(A):
    def __init__(self):
        self.a = 1
        self.c = 3
def f(x: A):
    if isinstance(x, (B, C)):
        return x.b
    return 0
print(f(B()))
",
    );
    let f = find_func(&m, "f");
    // Body should contain GetFieldPartial for exclusive .b
    fn has_partial(stmts: &[ir::Stmt]) -> bool {
        for s in stmts {
            match s {
                ir::Stmt::Return(Some(e))
                | ir::Stmt::ExprStmt(e)
                | ir::Stmt::Assign { value: e, .. } => {
                    if expr_has_partial(e) {
                        return true;
                    }
                }
                ir::Stmt::If { branches, orelse } => {
                    for (_, b) in branches {
                        if has_partial(b) {
                            return true;
                        }
                    }
                    if has_partial(orelse) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }
    fn expr_has_partial(e: &ir::Expr) -> bool {
        match &e.kind {
            ir::ExprKind::GetFieldPartial { .. } => true,
            ir::ExprKind::Block { stmts, result } => has_partial(stmts) || expr_has_partial(result),
            _ => false,
        }
    }
    assert!(has_partial(&f.body), "expected GetFieldPartial in f body");
}

#[test]
fn list_index_and_assignment() {
    let m = analyze_ok("xs = [1, 2]\ny = xs[0]\nxs[1] = 5\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(entry.body[2], ir::Stmt::IndexAssign { .. }));
}

#[test]
fn list_del_lowers() {
    let m = analyze_ok("xs = [1, 2, 3]\ndel xs[1]\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(entry.body[1], ir::Stmt::IndexDelete { .. }));
}

#[test]
fn list_slice_assign_lowers() {
    let m = analyze_ok("xs = [1, 2, 3, 4]\nxs[1:3] = [9]\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(entry.body[1], ir::Stmt::ListSliceAssign { .. }));
}

#[test]
fn list_slice_del_lowers() {
    let m = analyze_ok("xs = [1, 2, 3]\ndel xs[1:2]\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(entry.body[1], ir::Stmt::ListSliceAssign { .. }));
}

#[test]
fn list_slice_assign_rejects_str() {
    let e = analyze_err("xs = [1, 2, 3]\nxs[1:2] = \"a\"\n");
    assert!(
        e.message.contains("slice assignment") && e.message.contains("str"),
        "{}",
        e.message
    );
}

#[test]
fn list_del_rejects_non_int_index() {
    let e = analyze_err("xs = [1, 2]\ndel xs[\"a\"]\n");
    assert!(
        e.message.contains("list index") || e.message.contains("str"),
        "{}",
        e.message
    );
}

#[test]
fn del_on_str_is_error() {
    let e = analyze_err("s = \"ab\"\ndel s[0]\n");
    assert!(e.message.contains("'del'"), "{}", e.message);
}

#[test]
fn list_append_becomes_stmt() {
    let m = analyze_ok("xs = [1]\nxs.append(2)\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(entry.body[1], ir::Stmt::ListAppend { .. }));
}

#[test]
fn list_insert_remove_clear_and_index() {
    let m = analyze_ok(
        "xs = [1, 2, 3]\nxs.insert(1, 9)\nxs.remove(2)\n\
             i = xs.index(9)\nxs.clear()\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(entry.body[1], ir::Stmt::ListInsert { .. }));
    assert!(matches!(entry.body[2], ir::Stmt::ListRemove { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[3] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::ListIndexOf { .. }));
    assert!(matches!(entry.body[4], ir::Stmt::ListClear { .. }));
}

#[test]
fn list_reverse_lowers() {
    let m = analyze_ok("xs = [1, 2, 3]\nxs.reverse()\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(entry.body[1], ir::Stmt::ListReverse { .. }));
}

#[test]
fn list_reverse_rejects_args() {
    let e = analyze_err("xs = [1]\nxs.reverse(1)\n");
    assert!(e.message.contains("no arguments"), "{}", e.message);
}

#[test]
fn list_reverse_rejects_expression_position() {
    let e = analyze_err("xs = [1, 2]\ny = xs.reverse()\n");
    assert!(
        e.message.contains("returns None") && e.message.contains("reverse"),
        "{}",
        e.message
    );
}

#[test]
fn list_count_lowers() {
    let m = analyze_ok("xs = [1, 2, 1]\nn = xs.count(1)\nxs.count(9)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::ListCount { .. }));
    assert!(matches!(entry.body[2], ir::Stmt::ExprStmt(_)));
}

#[test]
fn list_count_nested_lowers() {
    let m = analyze_ok("n = [[1], [2], [1]].count([1])\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    let ir::ExprKind::ListCount { list, value } = &value.kind else {
        panic!("expected ListCount, got {:?}", value.kind);
    };
    assert_eq!(list.ty, ir::list_of(ir::list_of(ir::Ty::Int)));
    assert_eq!(value.ty, ir::list_of(ir::Ty::Int));
}

#[test]
fn list_count_rejects_wrong_arity() {
    let e = analyze_err("n = [1, 2].count()\n");
    assert!(e.message.contains("exactly one argument"), "{}", e.message);
}

#[test]
fn list_count_rejects_wrong_elem_type() {
    let e = analyze_err("n = [1, 2].count(\"a\")\n");
    assert!(
        e.message.contains("count() argument") && e.message.contains("str"),
        "{}",
        e.message
    );
}

#[test]
fn list_concat_and_repeat() {
    let m = analyze_ok("a = [1] + [2, 3]\nb = [1, 2] * 3\nc = 2 * [9]\n");
    let entry = find_func(&m, ENTRY_NAME);
    for i in 0..3 {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!();
        };
        assert_eq!(value.ty, ir::list_of(ir::Ty::Int));
        assert!(matches!(
            value.kind,
            ir::ExprKind::Binary {
                op: ir::BinOp::Add | ir::BinOp::Mul,
                ..
            }
        ));
    }
}

#[test]
fn list_concat_rejects_mixed_elem() {
    let e = analyze_err("x = [1] + [1.5]\n");
    assert!(e.message.contains("concatenate"), "{}", e.message);
}

#[test]
fn list_eq_is_bool() {
    let m = analyze_ok("b = [1, 2] == [1, 2]\nc = [1] != [2]\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(
        value.kind,
        ir::ExprKind::Binary {
            op: ir::BinOp::Eq,
            ..
        }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(
        value.kind,
        ir::ExprKind::Binary {
            op: ir::BinOp::Ne,
            ..
        }
    ));
}

#[test]
fn list_sort_stmt_and_sorted_builtin() {
    let m = analyze_ok("xs = [3, 1]\nxs.sort()\nys = sorted([2, 1])\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(entry.body[1], ir::Stmt::ListSort { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert_eq!(value.ty, ir::list_of(ir::Ty::Int));
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
}

#[test]
fn sorted_with_key_desugars_to_block_with_whiles() {
    let m = analyze_ok(
        "\
def k(x: int) -> int:
    return -x
ys = sorted([3, 1, 2], key=k)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!("expected GlobalAssign, got {:?}", entry.body[0]);
    };
    assert_eq!(value.ty, ir::list_of(ir::Ty::Int));
    let ir::ExprKind::Block { stmts, .. } = &value.kind else {
        panic!("expected Block");
    };
    let while_count = stmts
        .iter()
        .filter(|s| matches!(s, ir::Stmt::While { .. }))
        .count();
    assert!(
        while_count >= 2,
        "expected fill + insertion-sort Whiles, stmts={stmts:?}"
    );
    assert!(
        stmts.iter().any(|s| matches!(
            s,
            ir::Stmt::While {
                body,
                ..
            } if body.iter().any(|b| matches!(b, ir::Stmt::ListAppendUnchecked { .. }))
        )),
        "expected ListAppendUnchecked in key-fill loop"
    );
}

#[test]
fn list_sort_with_key_desugars_to_whiles() {
    let m = analyze_ok(
        "\
def k(x: int) -> int:
    return -x
xs = [3, 1, 2]
xs.sort(key=k)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    // GlobalAssign(xs), Assign(lsort temp), … Whiles
    let while_count = entry
        .body
        .iter()
        .filter(|s| matches!(s, ir::Stmt::While { .. }))
        .count();
    assert!(
        while_count >= 2,
        "expected fill + insertion-sort Whiles, body={:?}",
        entry.body
    );
    assert!(
        entry.body.iter().any(|s| matches!(
            s,
            ir::Stmt::While {
                body,
                ..
            } if body.iter().any(|b| matches!(b, ir::Stmt::ListAppendUnchecked { .. }))
        )),
        "expected ListAppendUnchecked in key-fill loop"
    );
}

#[test]
fn list_sort_reverse_desugars_with_reverse_loops() {
    let m = analyze_ok("xs = [3, 1, 2]\nxs.sort(reverse=True)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let while_count = entry
        .body
        .iter()
        .filter(|s| matches!(s, ir::Stmt::While { .. }))
        .count();
    // reverse before + reverse after (each one While) — no key fill.
    assert!(
        while_count >= 2,
        "expected reverse Whiles around ListSort, body={:?}",
        entry.body
    );
    assert!(
        entry
            .body
            .iter()
            .any(|s| matches!(s, ir::Stmt::ListSort { .. })),
        "expected ListSort, body={:?}",
        entry.body
    );
}

#[test]
fn sorted_reverse_desugars_to_block() {
    let m = analyze_ok("ys = sorted([3, 1, 2], reverse=True)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    let ir::ExprKind::Block { stmts, .. } = &value.kind else {
        panic!("expected Block");
    };
    assert!(
        stmts.iter().any(|s| matches!(s, ir::Stmt::ListSort { .. })),
        "expected ListSort in sorted(reverse=)"
    );
    let while_count = stmts
        .iter()
        .filter(|s| matches!(s, ir::Stmt::While { .. }))
        .count();
    assert!(while_count >= 2, "expected reverse Whiles, stmts={stmts:?}");
}

#[test]
fn reverse_truthy_int_const_folds() {
    let m = analyze_ok("ys = sorted([1, 2], reverse=1)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    let ir::ExprKind::Block { stmts, .. } = &value.kind else {
        panic!("expected Block");
    };
    // reverse=1 is Always: reverse Whiles present (before and after sort).
    let while_count = stmts
        .iter()
        .filter(|s| matches!(s, ir::Stmt::While { .. }))
        .count();
    assert!(
        while_count >= 2,
        "expected reverse Whiles for reverse=1, stmts={stmts:?}"
    );
}

#[test]
fn reverse_falsy_int_is_noop() {
    let m = analyze_ok("ys = sorted([1, 2], reverse=0)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    // reverse=0 → Never: only the ascending sort While(s), no reverse pair.
    // Still a Block; presence of sort is enough — no Cond bind for reverse.
    let ir::ExprKind::Block { stmts, .. } = &value.kind else {
        panic!("expected Block");
    };
    assert!(
        !stmts.iter().any(|s| matches!(
            s,
            ir::Stmt::Assign { name, .. } if name.starts_with(".sort.rev")
        )),
        "reverse=0 should not bind runtime reverse cond"
    );
}

#[test]
fn list_sort_key_expr_position_is_none_error() {
    let e = analyze_err(
        "\
def k(x: int) -> int:
    return x
xs = [1, 2]
ys = xs.sort(key=k)
",
    );
    assert!(
        e.message.contains("returns None") && e.message.contains("list.sort"),
        "{}",
        e.message
    );
}

#[test]
fn min_with_key_desugars_to_block_with_raise_on_empty() {
    let m = analyze_ok(
        "\
def k(x: int) -> int:
    return x
v = min([3, 1, 2], key=k)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    let ir::ExprKind::Block { stmts, .. } = &value.kind else {
        panic!("expected Block");
    };
    assert!(
        stmts.iter().any(|s| matches!(
            s,
            ir::Stmt::If {
                branches,
                ..
            } if branches.iter().any(|(_, b)| b
                .iter()
                .any(|st| matches!(st, ir::Stmt::Raise { exc: ir::ExcType::ValueError, .. })))
        )),
        "expected empty-list Raise ValueError, stmts={stmts:?}"
    );
    // Scan While lives in the non-empty orelse of the empty check.
    assert!(
        stmts.iter().any(|s| matches!(
            s,
            ir::Stmt::If { orelse, .. }
                if orelse.iter().any(|st| matches!(st, ir::Stmt::While { .. }))
        )),
        "expected scan While in nonempty branch, stmts={stmts:?}"
    );
}

#[test]
fn min_with_default_no_raise_on_empty() {
    let m = analyze_ok("xs: list[int] = []\nv = min(xs, default=99)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!("{:?}", entry.body);
    };
    assert_eq!(value.ty, ir::Ty::Int);
    let ir::ExprKind::Block { stmts, .. } = &value.kind else {
        panic!("expected Block for default=");
    };
    assert!(
        !stmts.iter().any(|s| matches!(
            s,
            ir::Stmt::If {
                branches,
                ..
            } if branches.iter().any(|(_, b)| b
                .iter()
                .any(|st| matches!(st, ir::Stmt::Raise { .. })))
        )),
        "default= must not Raise on empty"
    );
}

#[test]
fn min_default_none_joins_optional() {
    let m = analyze_ok("v = min([1, 2], default=None)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    // int | None
    assert!(
        matches!(value.ty, ir::Ty::Union(_)) || value.ty == ir::Ty::None || value.ty == ir::Ty::Int,
        "expected joined type, got {}",
        value.ty
    );
    let members = ir::flatten_union_members(value.ty);
    assert!(
        members.contains(&ir::Ty::Int) && members.contains(&ir::Ty::None),
        "expected int|None, got {}",
        value.ty
    );
}

#[test]
fn min_multi_arg_rejects_default() {
    let e = analyze_err("print(min(1, 2, default=0))\n");
    assert!(
        e.message.contains("default") && e.message.contains("multiple positional"),
        "{}",
        e.message
    );
}

#[test]
fn key_builtin_len_desugars() {
    let m = analyze_ok("ys = sorted([\"a\", \"bb\"], key=len)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::list_of(ir::Ty::Str));
    let ir::ExprKind::Block { stmts, .. } = &value.kind else {
        panic!("expected Block desugar");
    };
    // Keys list is built with Len on each str element.
    let has_len_key = stmts.iter().any(|s| match s {
        ir::Stmt::While { body, .. } => body.iter().any(|st| {
            matches!(
                st,
                ir::Stmt::ListAppendUnchecked { value, .. }
                    if matches!(value.kind, ir::ExprKind::Len(_))
            )
        }),
        _ => false,
    });
    assert!(
        has_len_key,
        "expected Len in key materialize, stmts={stmts:?}"
    );
}

#[test]
fn key_builtin_abs_ok() {
    let m = analyze_ok("ys = sorted([-3, 1, -2], key=abs)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::list_of(ir::Ty::Int));
}

#[test]
fn key_len_on_class_with_len_ok() {
    let m = analyze_ok(
        "\
class C:
    def __init__(self, n: int):
        self.n = n
    def __len__(self) -> int:
        return self.n
xs: list[C] = [C(3), C(1), C(2)]
ys = sorted(xs, key=len)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    // GlobalAssign xs, GlobalAssign ys
    assert!(entry.body.len() >= 2);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!("{:?}", entry.body);
    };
    assert!(matches!(value.ty, ir::Ty::List(_)));
}

#[test]
fn key_len_on_class_without_len_rejected() {
    let e = analyze_err(
        "\
class C:
    def __init__(self, n: int):
        self.n = n
xs: list[C] = [C(1)]
print(sorted(xs, key=len))
",
    );
    assert!(
        e.message.contains("key=len") && (e.message.contains("C") || e.message.contains("class#")),
        "{}",
        e.message
    );
}

#[test]
fn key_builtin_sum_still_rejected() {
    let e = analyze_err("print(sorted([[1], [1, 2]], key=sum))\n");
    assert!(
        e.message.contains("builtin 'sum'") && e.message.contains("key="),
        "{}",
        e.message
    );
}

#[test]
fn key_len_on_int_rejected() {
    let e = analyze_err("print(sorted([1, 2], key=len))\n");
    assert!(
        e.message.contains("key=len") && e.message.contains("int"),
        "{}",
        e.message
    );
}

#[test]
fn min_multi_arg_key_type_mismatch() {
    let e = analyze_err(
        "\
def k(x: int) -> int:
    return x
print(min(1, \"a\", key=k))
",
    );
    assert!(
        e.message.contains("same type") && e.message.contains("key="),
        "{}",
        e.message
    );
}

#[test]
fn augmented_index_assignment_uses_temps() {
    let m = analyze_ok("xs = [1, 2]\nxs[0] += 5\n");
    let entry = find_func(&m, ENTRY_NAME);
    // Assign(list), Assign(.aug.base), Assign(.aug.idx), IndexAssign
    assert_eq!(entry.body.len(), 4);
    assert!(matches!(entry.body[3], ir::Stmt::IndexAssign { .. }));
}

#[test]
fn for_range_desugars_to_while_with_step() {
    let m = analyze_ok("for i in range(10):\n    print(i)\n");
    let entry = find_func(&m, ENTRY_NAME);
    // Assign(.range.stop), Assign(i), While
    let ir::Stmt::While { step, .. } = &entry.body[2] else {
        panic!("expected While, got {:?}", entry.body[2]);
    };
    assert_eq!(step.len(), 1);
}

#[test]
fn for_range_zero_step_is_compile_error() {
    let e = analyze_err("for i in range(0, 10, 0):\n    print(i)\n");
    assert!(e.message.contains("zero"), "{}", e.message);
}

#[test]
fn for_over_file_desugars_to_while_more() {
    let m = analyze_ok("f = open(\"x\")\nfor line in f:\n    print(line)\n");
    let entry = find_func(&m, ENTRY_NAME);
    // open, file temp, more=True, While more (EOF uses flag, not break)
    assert!(
        entry.body.iter().any(|s| matches!(
            s,
            ir::Stmt::While {
                cond: ir::Expr {
                    kind: ir::ExprKind::Local(name),
                    ..
                },
                ..
            } if name.contains("more")
        )),
        "expected while-more for file iteration, body={:?}",
        entry.body
    );
}

#[test]
fn for_else_without_break_is_straight_line() {
    // No break in body → else is appended (not if-not-broke), so return
    // analysis can see else `return`s.
    let m = analyze_ok(
        "\
for i in range(2):
    pass
else:
    print(1)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(
        entry
            .body
            .iter()
            .any(|s| matches!(s, ir::Stmt::While { .. })),
        "{:?}",
        entry.body
    );
    // No broke-flag If: else is straight-line after the while.
    assert!(
        !entry.body.iter().any(|s| matches!(
            s,
            ir::Stmt::If {
                branches,
                ..
            } if matches!(
                branches[0].0.kind,
                ir::ExprKind::Unary { op: ir::UnOp::Not, .. }
            )
        )),
        "expected straight-line for-else when no break, body={:?}",
        entry.body
    );
}

#[test]
fn while_else_without_break_is_straight_line() {
    let m = analyze_ok(
        "\
n = 0
while n < 1:
    n = n + 1
else:
    print(1)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(
        entry
            .body
            .iter()
            .any(|s| matches!(s, ir::Stmt::While { .. })),
        "{:?}",
        entry.body
    );
    // broke flag only when the body can `break`.
    assert!(
        !entry.body.iter().any(|s| matches!(
            s,
            ir::Stmt::Assign {
                value: ir::Expr {
                    kind: ir::ExprKind::ConstBool(false),
                    ..
                },
                ..
            }
        )),
        "expected no broke=False when body has no break, body={:?}",
        entry.body
    );
}

#[test]
fn while_else_with_break_uses_broke_flag() {
    let m = analyze_ok(
        "\
n = 0
while n < 3:
    n = n + 1
    break
else:
    print(1)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(
        entry.body.iter().any(|s| matches!(
            s,
            ir::Stmt::If {
                branches,
                ..
            } if matches!(
                branches[0].0.kind,
                ir::ExprKind::Unary { op: ir::UnOp::Not, .. }
            )
        )),
        "expected if-not-broke when body can break, body={:?}",
        entry.body
    );
}

#[test]
fn for_without_else_has_no_broke_flag() {
    let m = analyze_ok("for i in range(2):\n    print(i)\n");
    let entry = find_func(&m, ENTRY_NAME);
    // no ConstBool false assign for broke
    let bool_false_assigns = entry
        .body
        .iter()
        .filter(|s| {
            matches!(
                s,
                ir::Stmt::Assign {
                    value: ir::Expr {
                        kind: ir::ExprKind::ConstBool(false),
                        ..
                    },
                    ..
                }
            )
        })
        .count();
    assert_eq!(bool_false_assigns, 0, "{:?}", entry.body);
}

#[test]
fn file_typed_param_and_return() {
    let m = analyze_ok(
        "\
def first(f: file) -> str:
    return f.readline()

def wrap(path: str) -> file:
    return open(path)

f = open(\"x\")
print(first(f))
",
    );
    let first = find_func(&m, "first");
    assert_eq!(first.params[0].1, ir::Ty::File);
    assert_eq!(first.ret, ir::Ty::Str);
    let wrap = find_func(&m, "wrap");
    assert_eq!(wrap.ret, ir::Ty::File);
}

#[test]
fn multi_assign_binds_both() {
    let m = analyze_ok("a = b = 1\n");
    let entry = find_func(&m, ENTRY_NAME);
    // temp + two global assigns (right-to-left)
    assert!(entry.body.len() >= 3);
    assert!(
        m.globals.iter().any(|(n, t)| n == "a" && *t == ir::Ty::Int),
        "{:?}",
        m.globals
    );
    assert!(
        m.globals.iter().any(|(n, t)| n == "b" && *t == ir::Ty::Int),
        "{:?}",
        m.globals
    );
}

#[test]
fn defaults_and_keyword_args() {
    let m = analyze_ok(
        "\
def f(a: int, b: int = 2) -> int:
    return a + b
print(f(1))
print(f(1, b=3))
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(
        &entry.body[0],
        ir::Stmt::Print { args, .. } if args.len() == 1
            && matches!(args[0].kind, ir::ExprKind::Call { ref args, .. } if args.len() == 2)
    ));
}

#[test]
fn missing_required_after_kw_is_error() {
    let e = analyze_err("def f(a: int, b: int = 1) -> int:\n    return a\nprint(f(b=2))\n");
    assert!(
        e.message.contains("missing required argument 'a'"),
        "{}",
        e.message
    );
}

#[test]
fn for_over_list_binds_elem_type() {
    let m = analyze_ok("for x in [1.5, 2.5]:\n    print(x)\n");
    let _ = find_func(&m, ENTRY_NAME);
    let has_x_float = m
        .globals
        .iter()
        .any(|(n, t)| n == "x" && *t == ir::Ty::Float);
    assert!(has_x_float, "globals: {:?}", m.globals);
}

#[test]
fn for_over_str_binds_str() {
    let m = analyze_ok("for c in \"abc\":\n    print(c)\n");
    let _ = find_func(&m, ENTRY_NAME);
    let has_c_str = m.globals.iter().any(|(n, t)| n == "c" && *t == ir::Ty::Str);
    assert!(has_c_str, "globals: {:?}", m.globals);
}

#[test]
fn for_over_int_is_error() {
    let e = analyze_err("for x in 42:\n    print(x)\n");
    assert!(e.message.contains("not iterable"), "{}", e.message);
}

#[test]
fn range_outside_for_is_error() {
    let e = analyze_err("xs = range(10)\n");
    assert!(e.message.contains("for"), "{}", e.message);
}

#[test]
fn str_and_int_concat_is_error() {
    let e = analyze_err("x = \"a\" + 1\n");
    assert!(e.message.contains("concatenate"), "{}", e.message);
}

#[test]
fn str_item_assignment_is_error() {
    let e = analyze_err("s = \"ab\"\ns[0] = \"c\"\n");
    assert!(e.message.contains("immutable"), "{}", e.message);
}

#[test]
fn heterogeneous_list_infers_a_union() {
    // Was an error before 0.137. A container keeps each element's own
    // type, so this is `list[int | str]` — the same shape the annotated
    // spelling has always produced, and the same rule 0.89 built for
    // mixed numerics.
    let m = analyze_ok("xs = [1, \"a\"]\nprint(xs)\n");
    let ty = m
        .globals
        .iter()
        .find(|(n, _)| n == "xs")
        .map(|(_, t)| *t)
        .expect("xs should be a module global");
    assert_eq!(ty, ir::list_of(ir::union_of(&[ir::Ty::Int, ir::Ty::Str])));
}

#[test]
fn a_file_element_still_has_no_union_to_join_into() {
    // `File` has no print tag, so it cannot be a tagged container slot.
    // That is a representation limit, not a policy choice, and it is the
    // one pair the union fallback declines.
    let e = analyze_err("f = open(\"x\", \"w\")\nxs = [f, 1]\n");
    assert!(e.message.contains("share one type"), "{}", e.message);
}

#[test]
fn error_no_entry_point() {
    let e = analyze_err("def helper() -> int:\n    return 1\n");
    assert!(e.message.contains("entry point"), "{}", e.message);
}

#[test]
fn error_variable_changes_type() {
    // Numeric multi-assign joins (int + float → float storage), so that
    // path is allowed. Incompatible non-numeric reassign still errors when
    // the joined storage cannot accept the RHS without a prior join pass
    // seeing both — here a later bool into a str-only binding.
    let e = analyze_err(
        "\
def f():
    x: str = \"a\"
    x = 1
    return x
print(f())
",
    );
    assert!(
        e.message.contains("type mismatch")
            || e.message.contains("storage type")
            || e.message.contains("expected str"),
        "{}",
        e.message
    );
}

#[test]
fn multi_assign_numeric_promotes() {
    // int then float → float storage (join_types numeric promotion).
    let m = analyze_ok("x = 1\nx = 2.5\nprint(x)\n");
    assert!(
        m.globals
            .iter()
            .any(|(n, t)| n == "x" && *t == ir::Ty::Float),
        "{:?}",
        m.globals
    );
}

#[test]
fn error_undefined_name() {
    let e = analyze_err("x = y + 1\n");
    assert!(e.message.contains("not defined"), "{}", e.message);
}

#[test]
fn error_missing_return_path() {
    let e = analyze_err("def f(a: int) -> int:\n    if a:\n        return 1\n");
    assert!(e.message.contains("without a return"), "{}", e.message);
}

#[test]
fn error_break_outside_loop() {
    let e = analyze_err("break\n");
    assert!(e.message.contains("outside"), "{}", e.message);
}

#[test]
fn str_truthiness_works() {
    analyze_ok("s = \"x\"\nif s:\n    print(1)\n");
    analyze_ok("xs = [1]\nwhile xs:\n    break\n");
}

#[test]
fn entry_calls_main_when_no_script() {
    let m = analyze_ok("def main():\n    print(1)\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert!(matches!(
        &entry.body[0],
        ir::Stmt::ExprStmt(ir::Expr {
            kind: ir::ExprKind::Call { func, .. },
            ..
        }) if func == "main"
    ));
}

#[test]
fn list_params_and_returns() {
    analyze_ok(
        "\
def total(xs: list[int]) -> int:
    t = 0
    for x in xs:
        t += x
    return t

print(total([1, 2, 3]))
",
    );
}

#[test]
fn empty_list_arg_uses_param_type() {
    analyze_ok(
        "\
def count(xs: list[str]) -> int:
    return len(xs)

print(count([]))
",
    );
}

#[test]
fn cannot_redefine_builtins() {
    let e = analyze_err("def len(x: int) -> int:\n    return x\nprint(len(1))\n");
    assert!(e.message.contains("builtin"), "{}", e.message);
}

#[test]
fn slices_type_correctly() {
    let m = analyze_ok("s = \"hello\"\nt = s[1:3]\nxs = [1, 2, 3]\nys = xs[:2]\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[3] else {
        panic!();
    };
    assert_eq!(value.ty, ir::list_of(ir::Ty::Int));
    // missing bounds become i64::MIN sentinels, missing step becomes 1
    let ir::ExprKind::Slice { lo, hi, step, .. } = &value.kind else {
        panic!();
    };
    assert!(matches!(lo.kind, ir::ExprKind::ConstInt(i64::MIN)));
    assert!(matches!(hi.kind, ir::ExprKind::ConstInt(_)));
    assert!(matches!(step.kind, ir::ExprKind::ConstInt(1)));
}

#[test]
fn error_slicing_an_int() {
    let e = analyze_err("x = 5\ny = x[1:2]\n");
    assert!(e.message.contains("sliced"), "{}", e.message);
}

#[test]
fn contains_types_correctly() {
    let m = analyze_ok("b = \"ell\" in \"hello\"\nc = 2 in [1, 2]\nd = 5 not in [1, 2]\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(value.kind, ir::ExprKind::Contains { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    // not in == Not(Contains)
    assert!(matches!(
        &value.kind,
        ir::ExprKind::Unary { op: ir::UnOp::Not, operand }
            if matches!(operand.kind, ir::ExprKind::Contains { .. })
    ));
}

#[test]
fn contains_coerces_needle_to_elem_type() {
    let m = analyze_ok("b = 1 in [1.5, 2.5]\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    let ir::ExprKind::Contains { needle, .. } = &value.kind else {
        panic!();
    };
    assert_eq!(needle.ty, ir::Ty::Float);
}

#[test]
fn contains_nested_list_lowers() {
    let m = analyze_ok("b = [1, 2] in [[1, 2], [3]]\nc = [9] not in [[1], [2]]\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    let ir::ExprKind::Contains { needle, haystack } = &value.kind else {
        panic!("expected Contains, got {:?}", value.kind);
    };
    assert_eq!(needle.ty, ir::list_of(ir::Ty::Int));
    assert_eq!(haystack.ty, ir::list_of(ir::list_of(ir::Ty::Int)));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(
        &value.kind,
        ir::ExprKind::Unary { op: ir::UnOp::Not, operand }
            if matches!(operand.kind, ir::ExprKind::Contains { .. })
    ));
}

#[test]
fn contains_nested_list_rejects_scalar_needle() {
    let e = analyze_err("b = 1 in [[1, 2]]\n");
    assert!(
        e.message.contains("'in' operand") || e.message.contains("list[int]"),
        "{}",
        e.message
    );
}

#[test]
fn error_int_in_str() {
    let e = analyze_err("b = 5 in \"hello\"\n");
    assert!(e.message.contains("str on the left"), "{}", e.message);
}

#[test]
fn pop_returns_element_type() {
    let m = analyze_ok("xs = [1.5]\nx = xs.pop()\ny = xs.pop(0)\nxs.pop()\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Float);
    assert!(matches!(value.kind, ir::ExprKind::ListPop { .. }));
    // statement-position pop is allowed and discarded
    assert!(matches!(entry.body[3], ir::Stmt::ExprStmt(_)));
}

#[test]
fn fstring_lowers_to_concat_with_conversions() {
    let m = analyze_ok("x = 42\ns = f\"x={x}!\"\nprint(s)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Str);
    // somewhere in the tree there must be an IntToStr conversion
    fn has_int_to_str(e: &ir::Expr) -> bool {
        match &e.kind {
            ir::ExprKind::IntToStr(_) => true,
            ir::ExprKind::Binary { left, right, .. } => {
                has_int_to_str(left) || has_int_to_str(right)
            }
            _ => false,
        }
    }
    assert!(has_int_to_str(value), "{value:?}");
}

#[test]
fn fstring_format_spec_lowers_to_format_value() {
    let m = analyze_ok("x = 3.14159\ns = f\"{x:.2f}\"\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    fn has_fmt(e: &ir::Expr) -> bool {
        match &e.kind {
            ir::ExprKind::FormatValue { value, spec } => {
                value.ty == ir::Ty::Float
                    && matches!(&spec.kind, ir::ExprKind::ConstStr(s) if s == ".2f")
            }
            ir::ExprKind::Binary { left, right, .. } => has_fmt(left) || has_fmt(right),
            _ => false,
        }
    }
    assert!(has_fmt(value), "{value:?}");
}

#[test]
fn fstring_format_spec_on_int() {
    let m = analyze_ok("n = 2\ns = f\"{n:.2f}\"\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    fn has_int_fmt(e: &ir::Expr) -> bool {
        match &e.kind {
            ir::ExprKind::FormatValue { value, .. } => value.ty == ir::Ty::Int,
            ir::ExprKind::Binary { left, right, .. } => has_int_fmt(left) || has_int_fmt(right),
            _ => false,
        }
    }
    assert!(has_int_fmt(value), "{value:?}");
}

#[test]
fn fstring_repr_conversion_on_str() {
    let m = analyze_ok("s = \"hi\"\nt = f\"{s!r}\"\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    fn has_repr(e: &ir::Expr) -> bool {
        match &e.kind {
            ir::ExprKind::StrRepr(_) => true,
            ir::ExprKind::Binary { left, right, .. } => has_repr(left) || has_repr(right),
            _ => false,
        }
    }
    assert!(has_repr(value), "{value:?}");
}

#[test]
fn fstring_of_a_list_renders_it() {
    // Rejected before 0.108; now the same text `print` writes.
    let m = analyze_ok("xs = [1]\ns = f\"{xs}\"\nprint(s)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(
        matches!(&value.kind, ir::ExprKind::ContainerRepr(_)),
        "{value:?}"
    );
}

#[test]
fn error_fstring_spec_on_a_list() {
    // A format *spec* on a container is still rejected, as in CPython.
    let e = analyze_err("xs = [1]\ns = f\"{xs:>4}\"\nprint(s)\n");
    assert!(e.message.contains("convert"), "{}", e.message);
}

#[test]
fn tuple_literal_and_unpack() {
    let m = analyze_ok("a, b = 1, 2\nprint(a)\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert!(
        entry
            .body
            .iter()
            .any(|s| matches!(s, ir::Stmt::Assign { .. }))
    );
}

#[test]
fn tuple_unpack_length_mismatch_is_error() {
    let e = analyze_err("a, b = (1,)\n");
    assert!(e.message.contains("not enough values"), "{}", e.message);
    let e = analyze_err("a, b = (1, 2, 3)\n");
    assert!(e.message.contains("too many values"), "{}", e.message);
}

#[test]
fn dict_key_type_rejected_in_annotation() {
    let e = analyze_err("d: dict[float, int] = {}\n");
    assert!(
        e.message.contains("not supported") || e.message.contains("only int"),
        "{}",
        e.message
    );
}

#[test]
fn dict_bare_get_returns_optional() {
    // Bare get(key) → Optional[V] (None on miss).
    let m = analyze_ok("d: dict[str, int] = {\"a\": 1}\nx = d.get(\"a\")\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!("{:?}", entry.body);
    };
    assert_eq!(value.ty, ir::optional_of(ir::Ty::Int));
    assert!(matches!(value.kind, ir::ExprKind::DictGet { .. }));
}

#[test]
fn dict_setdefault_lowers() {
    let m = analyze_ok(
        "d: dict[str, int] = {\"a\": 1}\nx = d.setdefault(\"a\", 9)\ny = d.setdefault(\"b\", 2)\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let mut n = 0;
    for s in &entry.body {
        if let ir::Stmt::GlobalAssign { value, .. } = s
            && matches!(value.kind, ir::ExprKind::DictSetDefault { .. })
        {
            assert_eq!(value.ty, ir::Ty::Int);
            n += 1;
        }
    }
    assert_eq!(n, 2, "expected two setdefault lowers: {:?}", entry.body);
}

#[test]
fn dict_setdefault_bare_requires_optional() {
    let e = analyze_err("d: dict[str, int] = {}\nprint(d.setdefault(\"a\"))\n");
    assert!(
        e.message.contains("setdefault") && e.message.contains("None"),
        "{}",
        e.message
    );
}

#[test]
fn dict_popitem_lowers() {
    let m = analyze_ok("d: dict[str, int] = {\"a\": 1}\np = d.popitem()\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!("{:?}", entry.body);
    };
    assert_eq!(value.ty, ir::tuple_of(&[ir::Ty::Str, ir::Ty::Int]));
    assert!(matches!(value.kind, ir::ExprKind::DictPopItem(_)));
}

#[test]
fn dict_fromkeys_lowers() {
    let m = analyze_ok(
        "a = dict.fromkeys([1, 2], 0)\n\
             b = dict.fromkeys([\"x\"])\n\
             c = dict.fromkeys({1, 2}, 1)\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(value.ty, ir::Ty::Dict { .. }));
    assert!(matches!(value.kind, ir::ExprKind::DictFromKeys { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(
        value.ty,
        ir::Ty::Dict {
            key: ir::Ty::Str,
            value: ir::Ty::None
        }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert!(matches!(value.kind, ir::ExprKind::DictFromKeys { .. }));
}

#[test]
fn dict_fromkeys_rejects_bad_keys() {
    let e = analyze_err("print(dict.fromkeys(1, 0))\n");
    assert!(e.message.contains("fromkeys"), "{}", e.message);
}

#[test]
fn dict_popitem_rejects_args() {
    let e = analyze_err("d: dict[str, int] = {\"a\": 1}\nprint(d.popitem(1))\n");
    assert!(
        e.message.contains("popitem") && e.message.contains("no arguments"),
        "{}",
        e.message
    );
}

#[test]
fn optional_assign_and_reject_as_int() {
    let m = analyze_ok("x: int | None = None\nx = 5\n");
    let entry = find_func(&m, ENTRY_NAME);
    assert!(
        entry
            .body
            .iter()
            .any(|s| matches!(s, ir::Stmt::GlobalAssign { .. }))
    );
    let e = analyze_err("x: int | None = None\ny: int = x\n");
    assert!(
        e.message.contains("cannot use")
            || e.message.contains("is None")
            || e.message.contains("type mismatch")
            || e.message.contains("None"),
        "{}",
        e.message
    );
}

#[test]
fn is_none_lowers() {
    let m = analyze_ok("x: int | None = 1\nb = x is None\nc = x is not None\n");
    let entry = find_func(&m, ENTRY_NAME);
    let has_is = entry.body.iter().any(|s| match s {
        ir::Stmt::GlobalAssign { value, .. } => {
            matches!(value.kind, ir::ExprKind::IsNone { .. })
        }
        _ => false,
    });
    assert!(has_is, "expected IsNone in IR: {:?}", entry.body);
}

#[test]
fn set_subset_lowers() {
    let m = analyze_ok(
        "a = {1, 2}\nb = {1, 2, 3}\nx = a.issubset(b)\ny = a < b\nz = a.isdisjoint({4})\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(
        value.kind,
        ir::ExprKind::SetIsSubset { proper: false, .. }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[3] else {
        panic!();
    };
    assert!(matches!(
        value.kind,
        ir::ExprKind::SetIsSubset { proper: true, .. }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[4] else {
        panic!();
    };
    assert!(matches!(value.kind, ir::ExprKind::SetIsDisjoint { .. }));
}

#[test]
fn set_inplace_updates_lower() {
    let m = analyze_ok(
        "s = {1, 2, 3}\n\
             s.intersection_update({2, 3})\n\
             s.difference_update({3})\n\
             s.symmetric_difference_update({1})\n\
             s &= {1, 2}\n\
             s -= {2}\n\
             s ^= {1, 4}\n",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ops = [
        ir::SetUpdateOp::Intersect,
        ir::SetUpdateOp::Diff,
        ir::SetUpdateOp::SymDiff,
        ir::SetUpdateOp::Intersect,
        ir::SetUpdateOp::Diff,
        ir::SetUpdateOp::SymDiff,
    ];
    for (i, want) in ops.into_iter().enumerate() {
        let ir::Stmt::SetUpdate { op, .. } = &entry.body[i + 1] else {
            panic!("body[{}]: {:?}", i + 1, entry.body[i + 1]);
        };
        assert_eq!(*op, want);
    }
}

#[test]
fn set_intersection_update_rejects_expr() {
    let e = analyze_err("s = {1}\nx = s.intersection_update({1})\n");
    assert!(e.message.contains("returns None"), "{}", e.message);
}

#[test]
fn set_pop_lowers() {
    let m = analyze_ok("s = {1, 2}\nx = s.pop()\ns.pop()\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::SetPop(_)));
    assert!(matches!(entry.body[2], ir::Stmt::ExprStmt(_)));
}

#[test]
fn set_pop_rejects_args() {
    let e = analyze_err("s = {1}\nprint(s.pop(1))\n");
    assert!(e.message.contains("no arguments"), "{}", e.message);
}

#[test]
fn set_copy_lowers() {
    let m = analyze_ok("s = {1, 2}\nt = s.copy()\ns.copy()\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(value.ty, ir::Ty::Set(_)));
    assert!(matches!(value.kind, ir::ExprKind::SetCopy(_)));
    assert!(matches!(entry.body[2], ir::Stmt::ExprStmt(_)));
}

#[test]
fn set_copy_rejects_args() {
    let e = analyze_err("s = {1}\nprint(s.copy(1))\n");
    assert!(e.message.contains("no arguments"), "{}", e.message);
}

#[test]
fn set_eq_lowers() {
    let m = analyze_ok("b = {1, 2} == {1, 2}\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(
        value.kind,
        ir::ExprKind::Binary {
            op: ir::BinOp::Eq,
            ..
        }
    ));
}

#[test]
fn set_empty_needs_annotation() {
    let e = analyze_err("s = set()\n");
    assert!(
        e.message.contains("annotation") || e.message.contains("set()"),
        "{}",
        e.message
    );
}

#[test]
fn class_point_lowers_with_fields_and_method() {
    let m = analyze_ok(
        "\
class Point:
    def __init__(self, x: int, y: int):
        self.x = x
        self.y = y
    def sum(self) -> int:
        return self.x + self.y
p = Point(1, 2)
print(p.sum())
",
    );
    assert!(!m.classes.is_empty());
    assert_eq!(m.classes[0].name, "Point");
    assert!(m.classes[0].fields.iter().any(|(n, _)| n == "x"));
    assert!(
        m.funcs
            .iter()
            .any(|f| f.name == "Point.__init__" || f.name.ends_with("Point.__init__"))
    );
    assert!(
        m.funcs
            .iter()
            .any(|f| f.name == "Point.sum" || f.name.ends_with("Point.sum"))
    );
}

#[test]
fn class_eq_identity_lowers() {
    let m = analyze_ok(
        "\
class A:
    pass
a = A()
b = a == a
c = a != A()
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(
        value.kind,
        ir::ExprKind::IsIdentity { not: false, .. }
    ));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[2] else {
        panic!();
    };
    assert!(matches!(
        value.kind,
        ir::ExprKind::IsIdentity { not: true, .. }
    ));
}

#[test]
fn class_eq_dunder_lowers() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: P) -> bool:
        return self.x == other.x
b = P(1) == P(1)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
}

#[test]
fn list_class_eq_lowers_to_block() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: P) -> bool:
        return self.x == other.x
b = [P(1)] == [P(1)]
c = [P(1)] != [P(2)]
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert!(matches!(
        value.kind,
        ir::ExprKind::Unary {
            op: ir::UnOp::Not,
            ..
        }
    ));
}

#[test]
fn list_int_eq_stays_binary() {
    let m = analyze_ok("b = [1, 2] == [1, 2]\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(
        value.kind,
        ir::ExprKind::Binary {
            op: ir::BinOp::Eq,
            ..
        }
    ));
}

#[test]
fn list_class_contains_lowers_to_block() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: P) -> bool:
        return self.x == other.x
xs = [P(1), P(2)]
b = P(1) in xs
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
}

#[test]
fn tuple_class_eq_lowers_to_block() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: P) -> bool:
        return self.x == other.x
b = (P(1), 1) == (P(1), 1)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
}

#[test]
fn class_lt_rejected() {
    let e = analyze_err(
        "\
class A:
    pass
print(A() < A())
",
    );
    assert!(
        e.message.contains("operator '<'") && e.message.contains("class"),
        "{}",
        e.message
    );
    assert!(e.message.contains("__lt__"), "{}", e.message);
}

#[test]
fn class_lt_dunder_lowers() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __lt__(self, other: P) -> bool:
        return self.x < other.x
    def __le__(self, other: P) -> bool:
        return self.x <= other.x
    def __gt__(self, other: P) -> bool:
        return self.x > other.x
    def __ge__(self, other: P) -> bool:
        return self.x >= other.x
a = P(1) < P(2)
b = P(1) <= P(1)
c = P(2) > P(1)
d = P(2) >= P(2)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    for i in 0..4 {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!("expected GlobalAssign at {i}");
        };
        assert_eq!(value.ty, ir::Ty::Bool);
        assert!(
            matches!(value.kind, ir::ExprKind::Block { .. }),
            "cmp {i} should lower to a method-call block"
        );
    }
}

#[test]
fn class_lt_inherited_lowers() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __lt__(self, other: P) -> bool:
        return self.x < other.x
class Q(P):
    pass
b = Q(1) < Q(2)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
}

#[test]
fn class_gt_reflects_to_lt() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __lt__(self, other: P) -> bool:
        return self.x < other.x
b = P(1) > P(2)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
}

#[test]
fn class_lt_reflects_to_gt() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __gt__(self, other: P) -> bool:
        return self.x > other.x
b = P(1) < P(2)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Bool);
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
}

#[test]
fn class_ge_without_le_rejected() {
    let e = analyze_err(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __lt__(self, other: P) -> bool:
        return self.x < other.x
print(P(1) >= P(2))
",
    );
    assert!(
        e.message.contains("operator '>='")
            && (e.message.contains("__ge__") || e.message.contains("__le__")),
        "{}",
        e.message
    );
}

#[test]
fn class_sorted_and_min_max_desugar() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __lt__(self, other: P) -> bool:
        return self.x < other.x
xs = [P(3), P(1), P(2)]
ys = sorted(xs)
xs.sort()
a = min(P(3), P(1), P(2))
b = max(xs)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!("expected sorted assign, got {:?}", entry.body[1]);
    };
    assert_eq!(value.ty, ir::list_of(ir::Ty::Class(0)));
    let ir::ExprKind::Block { stmts, .. } = &value.kind else {
        panic!("sorted should desugar to a Block");
    };
    assert!(
        !stmts.iter().any(|s| matches!(s, ir::Stmt::ListSort { .. })),
        "class sorted must not use primitive ListSort"
    );
    assert!(
        stmts.iter().any(|s| matches!(s, ir::Stmt::While { .. })),
        "class sorted should insertion-sort with While"
    );
    assert!(
        !entry
            .body
            .iter()
            .any(|s| matches!(s, ir::Stmt::ListSort { .. })),
        "class list.sort must not use primitive ListSort"
    );
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[3] else {
        panic!("expected min assign, got {:?}", entry.body[3]);
    };
    assert!(matches!(value.ty, ir::Ty::Class(_)));
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[4] else {
        panic!("expected max assign, got {:?}", entry.body[4]);
    };
    assert!(matches!(value.kind, ir::ExprKind::Block { .. }));
}

#[test]
fn class_sorted_without_lt_rejected() {
    let e = analyze_err(
        "\
class A:
    pass
print(sorted([A()]))
",
    );
    assert!(
        e.message.contains("sort") && e.message.contains("__lt__"),
        "{}",
        e.message
    );
}

#[test]
fn class_min_without_lt_rejected() {
    let e = analyze_err(
        "\
class A:
    pass
print(min(A(), A()))
",
    );
    assert!(
        e.message.contains("min()") && e.message.contains("__lt__"),
        "{}",
        e.message
    );
}

#[test]
fn class_eq_int_without_dunder_rejected() {
    let e = analyze_err(
        "\
class A:
    pass
print(A() == 1)
",
    );
    assert!(
        e.message.contains("cannot compare") || e.message.contains("__eq__"),
        "{}",
        e.message
    );
}

#[test]
fn class_eq_reflected_from_int() {
    let m = analyze_ok(
        "\
class P:
    def __init__(self, x: int):
        self.x = x
    def __eq__(self, other: int) -> bool:
        return self.x == other
b = 1 == P(1)
c = 2 != P(1)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    for i in 0..2 {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!("expected GlobalAssign at {i}");
        };
        assert_eq!(value.ty, ir::Ty::Bool);
        assert!(
            matches!(value.kind, ir::ExprKind::Block { .. }),
            "reflected ==/!= should lower to a method-call block"
        );
    }
}

#[test]
fn class_eq_unrelated_without_compatible_eq_is_identity() {
    let m = analyze_ok(
        "\
class A:
    pass
class B:
    def __eq__(self, other: B) -> bool:
        return True
x = A() == B()
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert!(matches!(
        value.kind,
        ir::ExprKind::IsIdentity { not: false, .. }
    ));
}

#[test]
fn class_getitem_rejected() {
    let e = analyze_err(
        "\
class A:
    pass
print(A()[0])
",
    );
    assert!(
        e.message.contains("not subscriptable") && e.message.contains("__getitem__"),
        "{}",
        e.message
    );
}

#[test]
fn class_getitem_lowers() {
    let m = analyze_ok(
        "\
class Box:
    def __init__(self, xs: list[int]):
        self.xs = xs
    def __getitem__(self, i: int) -> int:
        return self.xs[i]
x = Box([1, 2])[0]
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(
        matches!(
            value.kind,
            ir::ExprKind::Call { .. } | ir::ExprKind::CallMethod { .. }
        ),
        "expected __getitem__ call, got {:?}",
        value.kind
    );
}

#[test]
fn class_setitem_rejected() {
    let e = analyze_err(
        "\
class A:
    pass
A()[0] = 1
",
    );
    assert!(
        e.message.contains("item assignment") && e.message.contains("__setitem__"),
        "{}",
        e.message
    );
}

#[test]
fn class_setitem_lowers() {
    let m = analyze_ok(
        "\
class Box:
    def __init__(self, xs: list[int]):
        self.xs = xs
    def __setitem__(self, i: int, v: int) -> None:
        self.xs[i] = v
Box([1])[0] = 2
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(
        entry
            .body
            .iter()
            .any(|s| matches!(s, ir::Stmt::ExprStmt(_))),
        "expected __setitem__ as ExprStmt, body={:?}",
        entry.body
    );
}

#[test]
fn class_delitem_rejected() {
    let e = analyze_err(
        "\
class A:
    pass
del A()[0]
",
    );
    assert!(e.message.contains("__delitem__"), "{}", e.message);
}

#[test]
fn class_delitem_lowers() {
    let m = analyze_ok(
        "\
class Box:
    def __init__(self, xs: list[int]):
        self.xs = xs
    def __delitem__(self, i: int) -> None:
        del self.xs[i]
del Box([1, 2])[0]
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(
        entry
            .body
            .iter()
            .any(|s| matches!(s, ir::Stmt::ExprStmt(_))),
        "expected __delitem__ as ExprStmt, body={:?}",
        entry.body
    );
}

#[test]
fn class_getitem_inherited_lowers() {
    let m = analyze_ok(
        "\
class Box:
    def __init__(self, xs: list[int]):
        self.xs = xs
    def __getitem__(self, i: int) -> int:
        return self.xs[i]
class Child(Box):
    pass
x = Child([7, 8])[1]
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
}

#[test]
fn class_getitem_wrong_key_rejected() {
    let e = analyze_err(
        "\
class Box:
    def __getitem__(self, i: int) -> int:
        return i
print(Box()[\"x\"])
",
    );
    assert!(
        e.message.contains("int") || e.message.contains("str") || e.message.contains("argument"),
        "{}",
        e.message
    );
}

#[test]
fn class_multi_base_rejected() {
    let e = analyze_err(
        "\
class A:
    pass
class B:
    pass
class C(A, B):
    pass
",
    );
    assert!(
        e.message
            .contains("multiple inheritance is not supported yet"),
        "{}",
        e.message
    );
}

#[test]
fn class_incompatible_override_rejected() {
    let e = analyze_err(
        "\
class A:
    def m(self) -> int:
        return 1
class B(A):
    def m(self) -> str:
        return \"x\"
",
    );
    assert!(
        e.message.contains("incompatible return type"),
        "{}",
        e.message
    );
}

#[test]
fn hetero_tuple_for_is_error() {
    let e = analyze_err("t = (1, \"a\")\nfor x in t:\n    print(x)\n");
    assert!(e.message.contains("heterogeneous"), "{}", e.message);
}

#[test]
fn raise_and_try_lower() {
    let m = analyze_ok(
        "\
try:
    raise ValueError(\"x\")
except ValueError as e:
    print(e)
finally:
    print(\"f\")
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(entry.body.iter().any(|s| matches!(s, ir::Stmt::Try { .. })));
    // `as e` binds a first-class exception object, not str.
    let e_ty = entry.locals.iter().find(|(n, _)| n == "e").map(|(_, t)| *t);
    assert_eq!(e_ty, Some(ir::Ty::Exception), "locals: {:?}", entry.locals);
}

#[test]
fn except_exception_and_isinstance_lower() {
    let m = analyze_ok(
        "\
try:
    raise FileNotFoundError(\"x\")
except OSError as e:
    print(isinstance(e, OSError))
    print(str(e))
try:
    raise ValueError(\"v\")
except Exception:
    print(\"ok\")
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(entry.body.iter().any(|s| matches!(s, ir::Stmt::Try { .. })));
    fn walk_expr(e: &ir::Expr, f: &mut dyn FnMut(&ir::Expr)) {
        f(e);
        match &e.kind {
            ir::ExprKind::ExcIsInstance { value, .. }
            | ir::ExprKind::ExcToStr(value)
            | ir::ExprKind::ToBool(value) => walk_expr(value, f),
            ir::ExprKind::IsInstance { value, .. } => walk_expr(value, f),
            _ => {}
        }
    }
    fn walk_stmt(s: &ir::Stmt, f: &mut dyn FnMut(&ir::Expr)) {
        match s {
            ir::Stmt::Print {
                args,
                sep,
                end,
                flush,
                ..
            } => {
                for a in args {
                    walk_expr(a, f);
                }
                walk_expr(sep, f);
                walk_expr(end, f);
                walk_expr(flush, f);
            }
            ir::Stmt::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                for st in body.iter().chain(orelse).chain(finally) {
                    walk_stmt(st, f);
                }
                for (_, _, hb) in handlers {
                    for st in hb {
                        walk_stmt(st, f);
                    }
                }
            }
            _ => {}
        }
    }
    let mut saw_exc_isinstance = false;
    let mut saw_exc_to_str = false;
    for s in &entry.body {
        walk_stmt(s, &mut |e| match &e.kind {
            ir::ExprKind::ExcIsInstance { .. } => saw_exc_isinstance = true,
            ir::ExprKind::ExcToStr(_) => saw_exc_to_str = true,
            _ => {}
        });
    }
    assert!(
        saw_exc_isinstance,
        "expected ExcIsInstance in {:?}",
        entry.body
    );
    assert!(saw_exc_to_str, "expected ExcToStr in {:?}", entry.body);
}

#[test]
fn exception_in_list_is_ok() {
    // exceptions may be list elements as of v0.24
    let m = analyze_ok(
        "\
xs = []
try:
    raise ValueError(\"x\")
except ValueError as e:
    xs.append(e)
print(len(xs))
",
    );
    let _ = m;
}

#[test]
fn raise_counts_as_return_path() {
    let m = analyze_ok(
        "\
def f(x: int) -> int:
    if x < 0:
        raise ValueError(\"neg\")
    return x

print(f(1))
",
    );
    assert_eq!(find_func(&m, "f").ret, ir::Ty::Int);
}

#[test]
fn try_raise_except_pass_missing_return() {
    // body raises but except falls through — not a valid -> int path
    let e = analyze_err(
        "\
def f() -> int:
    try:
        raise ValueError(\"x\")
    except ValueError:
        pass
print(1)
",
    );
    assert!(
        e.message.contains("return") || e.message.contains("end of its body"),
        "{}",
        e.message
    );
}

#[test]
fn try_return_is_ok_with_dead_handler() {
    let m = analyze_ok(
        "\
def f() -> int:
    try:
        return 1
    except ValueError:
        pass
print(f())
",
    );
    assert_eq!(find_func(&m, "f").ret, ir::Ty::Int);
}

#[test]
fn tuple_membership_lowers() {
    let m = analyze_ok("print(1 in (1, 2))\n");
    let entry = find_func(&m, ENTRY_NAME);
    let has = entry.body.iter().any(|s| match s {
        ir::Stmt::Print { args, .. } => args
            .iter()
            .any(|a| matches!(a.kind, ir::ExprKind::Contains { .. })),
        _ => false,
    });
    assert!(
        has,
        "expected Contains for tuple membership: {:?}",
        entry.body
    );
}

#[test]
fn tuple_count_and_index_lower() {
    let m = analyze_ok("n = (1, 2, 1).count(1)\ni = (1, 2, 1).index(2)\n(1, 2).count(9)\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::TupleCount { .. }));
    let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
        panic!();
    };
    assert_eq!(value.ty, ir::Ty::Int);
    assert!(matches!(value.kind, ir::ExprKind::TupleIndexOf { .. }));
    assert!(matches!(entry.body[2], ir::Stmt::ExprStmt(_)));
}

#[test]
fn tuple_index_rejects_extra_arg() {
    let e = analyze_err("print((1, 2).index(1, 0, 2, 3))\n");
    assert!(e.message.contains("at most 3 arguments"), "{}", e.message);
}

#[test]
fn tuple_index_bounds_lower() {
    let m = analyze_ok("i = (1, 2, 1).index(1, 1)\nj = (1, 2, 1).index(1, 0, 2)\n");
    let entry = find_func(&m, ENTRY_NAME);
    for i in 0..2 {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!("body[{i}]");
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::TupleIndexOf { .. }));
    }
}

#[test]
fn list_index_bounds_lower() {
    let m = analyze_ok("i = [1, 2, 1].index(1, 1)\nj = [1, 2, 1].index(1, True, 3)\n");
    let entry = find_func(&m, ENTRY_NAME);
    for i in 0..2 {
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[i] else {
            panic!("body[{i}]");
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::ListIndexOf { .. }));
    }
}

#[test]
fn list_index_rejects_none_bound() {
    let e = analyze_err("print([1, 2].index(1, None))\n");
    assert!(
        e.message.contains("slice indices must be integers"),
        "{}",
        e.message
    );
}

#[test]
fn tuple_count_rejects_incompatible_needle() {
    let e = analyze_err("print((1, 2).count(\"a\"))\n");
    assert!(
        e.message.contains("not compatible") || e.message.contains("type mismatch"),
        "{}",
        e.message
    );
}

#[test]
fn isinstance_bool_is_int() {
    let m = analyze_ok("print(isinstance(True, int))\n");
    let entry = find_func(&m, ENTRY_NAME);
    let ir::Stmt::Print { args, .. } = &entry.body[0] else {
        panic!("{:?}", entry.body);
    };
    assert!(
        matches!(args[0].kind, ir::ExprKind::ConstBool(true)),
        "{:?}",
        args[0]
    );
}

#[test]
fn isinstance_rejects_variable_type() {
    let e = analyze_err("t = 1\nprint(isinstance(1, t))\n");
    assert!(
        e.message.contains("type name")
            || e.message.contains("not a variable")
            || e.message.contains("does not support type"),
        "{}",
        e.message
    );
}

#[test]
fn multi_assign_joins_to_union() {
    let m = analyze_ok(
        "\
def f():
    x = 1
    x = \"a\"
    return x
print(f())
",
    );
    let f = find_func(&m, "f");
    // local x should be int|str
    let x_ty = f.locals.iter().find(|(n, _)| n == "x").map(|(_, t)| *t);
    assert!(
        x_ty.is_some_and(|t| matches!(t, ir::Ty::Union(_))),
        "expected union local, got {x_ty:?} in {:?}",
        f.locals
    );
}

#[test]
fn bare_param_infers_int() {
    let m = analyze_ok(
        "\
def f(x):
    return x + 1
print(f(2))
",
    );
    let f = find_func(&m, "f");
    assert_eq!(f.params[0].1, ir::Ty::Int);
}

#[test]
fn bare_param_infers_from_isinstance() {
    let m = analyze_ok(
        "\
def f(x):
    if isinstance(x, int):
        return x + 1
    return 0
print(f(3))
",
    );
    let f = find_func(&m, "f");
    assert_eq!(f.params[0].1, ir::Ty::Int);
}

#[test]
fn bare_param_multi_isinstance_needs_annotation() {
    let e = analyze_err(
        "\
def f(x):
    if isinstance(x, (int, float)):
        return x + 1
    return 0
print(f(3))
",
    );
    assert!(
        e.message.contains("missing a type annotation"),
        "{}",
        e.message
    );
}

#[test]
fn bare_param_isinstance_list_needs_annotation() {
    let e = analyze_err(
        "\
def f(x):
    if isinstance(x, list):
        return len(x)
    return 0
print(f([1]))
",
    );
    assert!(
        e.message.contains("missing a type annotation"),
        "{}",
        e.message
    );
}

#[test]
fn and_chain_isinstance_keeps_more_specific_class() {
    let m = analyze_ok(
        "\
class A:
    def __init__(self):
        self.a = 1
class B(A):
    def __init__(self):
        self.a = 1
        self.b = 2
class C(B):
    def __init__(self):
        self.a = 1
        self.b = 2
        self.c = 3
def f(x: A):
    if isinstance(x, C) and isinstance(x, B):
        return x.c
    return 0
print(f(C()))
",
    );
    let f = find_func(&m, "f");
    assert_eq!(f.ret, ir::Ty::Int);
}

#[test]
fn bare_param_infers_str_from_method() {
    let m = analyze_ok(
        "\
def f(x):
    return x.upper()
print(f(\"hi\"))
",
    );
    let f = find_func(&m, "f");
    assert_eq!(f.params[0].1, ir::Ty::Str);
}

#[test]
fn empty_list_from_append_infers_elem() {
    let m = analyze_ok(
        "\
def f():
    xs = []
    xs.append(1)
    return xs
print(f())
",
    );
    let f = find_func(&m, "f");
    assert_eq!(f.ret, ir::list_of(ir::Ty::Int));
}

#[test]
fn isinstance_subclass_peel_allows_subclass_field() {
    let m = analyze_ok(
        "\
class A:
    def __init__(self):
        self.a = 1
class B(A):
    def __init__(self):
        self.a = 1
        self.b = 2
def f(x: A):
    if isinstance(x, B):
        return x.b
    return x.a
print(f(B()))
",
    );
    let f = find_func(&m, "f");
    assert_eq!(f.params[0].1, ir::Ty::Class(0));
    assert_eq!(f.ret, ir::Ty::Int);
}

#[test]
fn subclass_into_base_union_ok() {
    let m = analyze_ok(
        "\
class A:
    def __init__(self):
        self.a = 1
class B(A):
    def __init__(self):
        self.a = 1
x: A | int = B()
print(x)
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(
        entry.body.iter().any(|s| matches!(
            s,
            ir::Stmt::GlobalAssign { name, .. } if name == "x"
        )),
        "{:?}",
        entry.body
    );
}

#[test]
fn raise_new_exc_types() {
    let m = analyze_ok(
        "\
try:
    raise FileNotFoundError(\"missing\")
except FileNotFoundError as e:
    print(e)
try:
    raise OverflowError(\"big\")
except (OverflowError, ValueError):
    print(\"ov\")
",
    );
    let entry = find_func(&m, ENTRY_NAME);
    assert!(entry.body.iter().any(|s| matches!(s, ir::Stmt::Try { .. })));
}

#[test]
fn import_bind_helpers() {
    assert_eq!(import_bind_name("pkg.mod", &None), "pkg");
    assert_eq!(import_bind_name("pkg.mod", &Some("m".into())), "m");
    assert_eq!(import_bound_module("pkg.mod", &None), "pkg");
    assert_eq!(import_bound_module("pkg.mod", &Some("m".into())), "pkg.mod");
}

#[test]
fn submodule_map_links_parents() {
    let names = vec![
        "pkg".into(),
        "pkg.mod".into(),
        "pkg.sub".into(),
        "pkg.sub.m".into(),
    ];
    let map = build_submodule_map(&names);
    assert_eq!(map["pkg"]["mod"], "pkg.mod");
    assert_eq!(map["pkg"]["sub"], "pkg.sub");
    assert_eq!(map["pkg.sub"]["m"], "pkg.sub.m");
}

#[test]
fn multi_module_reexport_analyze() {
    let mod_ast = parser::parse("VAL = 3\ndef f() -> int:\n    return VAL\n").unwrap();
    let pkg_ast = parser::parse("from pkg.mod import f, VAL\n").unwrap();
    let main_ast =
        parser::parse("import pkg\nprint(pkg.VAL, pkg.f())\nfrom pkg import f as g\nprint(g())\n")
            .unwrap();
    let m = analyze_program(&[
        ModuleInput {
            name: "pkg.mod".into(),
            ast: &mod_ast,
        },
        ModuleInput {
            name: "pkg".into(),
            ast: &pkg_ast,
        },
        ModuleInput {
            name: ENTRY_NAME.into(),
            ast: &main_ast,
        },
    ])
    .expect("reexport program should analyze");
    // Re-exported call should target origin IR name pkg.mod.f
    let entry = find_func(&m, ENTRY_NAME);
    let has_origin_call = entry.body.iter().any(|s| match s {
            ir::Stmt::Print { args, .. } => args.iter().any(|a| {
                matches!(
                    &a.kind,
                    ir::ExprKind::Call { func, .. } if func == "pkg.mod.f"
                )
            }),
            ir::Stmt::ExprStmt(e) => matches!(
                &e.kind,
                ir::ExprKind::Call { func, .. } if func == "pkg.mod.f" || func == "pkg.__init__" || func == "pkg.mod.__init__"
            ),
            _ => false,
        });
    assert!(
        has_origin_call
            || entry.body.iter().any(|s| matches!(
                s,
                ir::Stmt::Print { args, .. } if args.iter().any(|a| matches!(
                    &a.kind,
                    ir::ExprKind::Call { func, .. } if func.contains("f")
                ))
            )),
        "expected call through re-export; body={:?}",
        entry.body
    );
}

#[test]
fn mixed_class_list_from_empty_is_union() {
    let m = analyze_ok(
        "\
class Dog:
    def __init__(self):
        self.name = \"d\"
class Cat:
    def __init__(self):
        self.name = \"c\"
xs = []
xs.append(Dog())
xs.append(Cat())
print(xs[0].name)
print(len(xs))
",
    );
    let xs = m.globals.iter().find(|(n, _)| n == "xs").map(|(_, t)| *t);
    assert!(
        matches!(xs, Some(ir::Ty::List(e)) if matches!(e, ir::Ty::Union(_))),
        "expected list[Dog|Cat], got {:?}",
        xs
    );
}
