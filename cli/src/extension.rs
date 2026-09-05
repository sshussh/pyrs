//! Experimental native library target and its CPython adapter.
use std::{collections::HashSet, fs, path::PathBuf, process::Command};

use ir::Ty;
use parser::ast::{self, ExprKind as E, StmtKind as S};

use crate::{cli::ExtensionCommand, read_source, render_diag, temp_workdir, temp_workdir_in};

const BRIDGE: &str = include_str!("../../codegen/runtime/python_bridge.c");

pub fn build(cmd: ExtensionCommand) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("build-extension currently supports Linux only".into());
    }
    if !identifier(&cmd.module) {
        return Err("extension module name must be an ASCII Python identifier".into());
    }
    let source = read_source(&cmd.input)?;
    let ast = parser::parse(&source).map_err(|d| render_diag(&d, &cmd.input, &source))?;
    let names: HashSet<String> = ast
        .body
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            S::FuncDef(f) => Some(f.name.clone()),
            _ => None,
        })
        .collect();
    for stmt in &ast.body {
        match &stmt.kind {
            S::FuncDef(f) => {
                if !identifier(&f.name) || f.params.iter().any(|p| !identifier(&p.name)) {
                    return Err("extension function and parameter names must be ASCII identifiers".into());
                }
                if !f.decorators.is_empty() || f.vararg.is_some() || f.kwarg.is_some()
                    || f.params.iter().any(|p| p.default.is_some()) {
                    return Err(format!("extension function '{}' cannot use decorators, defaults, *args or **kwargs yet", f.name));
                }
                validate_body(&f.body, &names)?;
            }
            S::ExprStmt(ast::Expr { kind: E::Str(_), .. }) | S::Pass => {}
            _ => return Err("extension source must contain numerical function definitions only; imports and module initialization are not supported yet".into()),
        }
    }
    let module =
        semantic::analyze_library(&ast).map_err(|d| render_diag(&d, &cmd.input, &source))?;
    if !module.globals.is_empty() || !module.classes.is_empty() {
        return Err("extension kernels cannot retain module globals or class state".into());
    }
    let mut exports = Vec::new();
    for f in &module.funcs {
        if f.name == module.entry {
            continue;
        }
        if f.is_generator || !scalar(f.ret) || f.params.iter().any(|(_, ty)| !argument(*ty)) {
            return Err(format!(
                "extension function '{}' requires int/float/bool or list[float] arguments and a scalar result",
                f.name
            ));
        }
        // A name-target augmented assignment can still mutate an aliased list.
        // Inspect resolved storage types rather than guessing from source names.
        if let Some(original) = ast.body.iter().find_map(|stmt| match &stmt.kind {
            S::FuncDef(original) if original.name == f.name => Some(original),
            _ => None,
        }) {
            validate_augmented(&original.body, f)?;
        }
        if !f.name.starts_with('_') {
            exports.push(f);
        }
    }
    if exports.is_empty() {
        return Err("extension source has no public numerical functions".into());
    }

    let config = Command::new(&cmd.python).args(["-I", "-c", r#"
import sys, sysconfig
if sys.implementation.name != 'cpython' or sys.version_info < (3, 12):
    raise SystemExit('build-extension requires CPython 3.12 or later')
if sysconfig.get_config_var('Py_GIL_DISABLED'):
    raise SystemExit('build-extension currently requires GIL-enabled CPython')
print('\0'.join([sysconfig.get_path('include'), sysconfig.get_path('platinclude'), sysconfig.get_config_var('EXT_SUFFIX')]), end='')
"#]).output().map_err(|e| format!("cannot inspect CPython {}: {e}", cmd.python.display()))?;
    if !config.status.success() {
        return Err(format!(
            "cannot inspect CPython: {}",
            String::from_utf8_lossy(&config.stderr)
        ));
    }
    let config =
        String::from_utf8(config.stdout).map_err(|_| "CPython configuration is not UTF-8")?;
    let fields: Vec<_> = config.split('\0').collect();
    if fields.len() != 3 || !fields[2].ends_with(".so") {
        return Err("CPython returned an unsupported extension configuration".into());
    }
    let output = cmd
        .output
        .unwrap_or_else(|| PathBuf::from(format!("{}{}", cmd.module, fields[2])));
    if output == cmd.input
        || (output.exists() && fs::canonicalize(&output).ok() == fs::canonicalize(&cmd.input).ok())
    {
        return Err("extension output must not overwrite its Python source".into());
    }
    if !PathBuf::from(fields[0]).join("Python.h").is_file() {
        return Err(format!(
            "CPython development headers are missing from {}",
            fields[0]
        ));
    }
    let dir = temp_workdir()?;
    let object = dir.join("kernels.o");
    codegen::compile_ir_to_object(&codegen::emit_library_ir(&module), &object, cmd.opt_level)
        .map_err(|e| format!("error[codegen]: {e}"))?;
    fs::write(dir.join("runtime.c"), codegen::RUNTIME_C).map_err(|e| e.to_string())?;
    fs::write(dir.join("gc.c"), codegen::GC_C).map_err(|e| e.to_string())?;
    fs::write(dir.join("gc.h"), codegen::GC_H).map_err(|e| e.to_string())?;
    fs::write(dir.join("bridge.c"), wrapper(&cmd.module, &exports)).map_err(|e| e.to_string())?;
    fs::write(
        dir.join("exports.map"),
        format!("{{ global: PyInit_{}; local: *; }};\n", cmd.module),
    )
    .map_err(|e| e.to_string())?;
    let library = dir.join("extension.so");
    let cc = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let result = Command::new(cc)
        .arg("-shared")
        .arg("-fPIC")
        .arg("-O2")
        .arg("-fno-omit-frame-pointer")
        // Buffer exporters own double storage; the runtime's generic slots
        // access the same bytes through integer pointers.
        .arg("-fno-strict-aliasing")
        .arg("-Wno-format-truncation")
        .arg("-I")
        .arg(fields[0])
        .arg("-I")
        .arg(fields[1])
        .arg(&object)
        .arg(dir.join("bridge.c"))
        .arg(dir.join("gc.c"))
        .arg(format!(
            "-Wl,--version-script={}",
            dir.join("exports.map").display()
        ))
        .arg("-lm")
        .arg("-o")
        .arg(&library)
        .output()
        .map_err(|e| format!("failed to invoke C compiler: {e}"))?;
    if !result.status.success() {
        return Err(format!(
            "extension linking failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    // Publish a new inode atomically. Truncating an existing shared library can
    // corrupt a different Python process that still has the old file mapped.
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let publish = temp_workdir_in(parent)?;
    let staged = publish.join("extension.so");
    fs::copy(library, &staged).map_err(|e| format!("cannot write {}: {e}", output.display()))?;
    fs::rename(staged, &output).map_err(|e| format!("cannot publish {}: {e}", output.display()))?;
    println!("{}", output.display());
    Ok(())
}

fn identifier(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn scalar(ty: Ty) -> bool {
    matches!(ty, Ty::Int | Ty::Float | Ty::Bool | Ty::None)
}
fn argument(ty: Ty) -> bool {
    matches!(ty, Ty::Int | Ty::Float | Ty::Bool) || matches!(ty, Ty::List(t) if *t == Ty::Float)
}
fn code(ty: Ty) -> usize {
    match ty {
        Ty::Int => 0,
        Ty::Float => 1,
        Ty::Bool => 2,
        Ty::None => 3,
        Ty::List(_) => 4,
        _ => unreachable!(),
    }
}
fn ctype(ty: Ty, result: bool) -> &'static str {
    match ty {
        Ty::Int => "long long",
        Ty::Float => "double",
        Ty::Bool => "_Bool",
        Ty::None if result => "void",
        Ty::None => "unsigned char",
        Ty::List(_) => "PyrsList *",
        _ => unreachable!(),
    }
}

fn wrapper(name: &str, exports: &[&ir::Function]) -> String {
    let mut out = BRIDGE.to_string();
    for (index, f) in exports.iter().enumerate() {
        let params = f
            .params
            .iter()
            .map(|(_, t)| ctype(*t, false))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "\nextern {} pyrs_{}({});\n",
            ctype(f.ret, true),
            f.name,
            if params.is_empty() { "void" } else { &params }
        ));
        out.push_str(&format!(
            "static void bridge_invoke_{index}(BridgeArg *a) {{\n"
        ));
        let args = f
            .params
            .iter()
            .enumerate()
            .map(|(i, (_, ty))| match ty {
                Ty::Int => format!("a[{i}].integer"),
                Ty::Float => format!("a[{i}].floating"),
                Ty::Bool => format!("a[{i}].boolean"),
                Ty::None => "0".into(),
                Ty::List(_) => format!("&a[{i}].list"),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let result = match f.ret {
            Ty::Int => format!("a[{}].integer = ", f.params.len()),
            Ty::Float => format!("a[{}].floating = ", f.params.len()),
            Ty::Bool => format!("a[{}].boolean = ", f.params.len()),
            _ => String::new(),
        };
        out.push_str(&format!("    {result}pyrs_{}({args});\n}}\n", f.name));
        let keys = f
            .params
            .iter()
            .map(|(name, _)| format!("\"{name}\", "))
            .collect::<String>();
        let kinds = f
            .params
            .iter()
            .map(|(_, ty)| code(*ty).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("static PyObject *bridge_wrap_{index}(PyObject *self, PyObject *args, PyObject *kwargs) {{\n    static const char *names[] = {{{keys}NULL}};\n    static const int kinds[] = {{{kinds}{comma}0}};\n    return bridge_call(self, args, kwargs, \"{}\", {}, names, kinds, {}, bridge_invoke_{index});\n}}\n", f.name, f.params.len(), code(f.ret), comma=if kinds.is_empty() { "" } else { ", " }));
    }
    out.push_str("static PyMethodDef bridge_methods[] = {\n");
    for (index, f) in exports.iter().enumerate() {
        out.push_str(&format!("    {{\"{}\", (PyCFunction)(void (*)(void))bridge_wrap_{index}, METH_VARARGS | METH_KEYWORDS, \"Experimental native numerical kernel.\"}},\n", f.name));
    }
    out.push_str(&format!("    {{NULL, NULL, 0, NULL}}\n}};\nstatic PyModuleDef_Slot bridge_slots[] = {{\n    {{Py_mod_exec, bridge_module_exec}},\n    {{Py_mod_multiple_interpreters, Py_MOD_MULTIPLE_INTERPRETERS_NOT_SUPPORTED}},\n    {{0, NULL}}\n}};\nstatic struct PyModuleDef bridge_module = {{\n    PyModuleDef_HEAD_INIT, \"{name}\", \"Experimental PyRs native kernels.\", 0, bridge_methods, bridge_slots, NULL, NULL, NULL\n}};\nPyMODINIT_FUNC PyInit_{name}(void) {{ return PyModuleDef_Init(&bridge_module); }}\n"));
    out
}

fn target(t: &ast::AssignTarget) -> Result<(), String> {
    if matches!(t, ast::AssignTarget::Name { .. }) {
        Ok(())
    } else {
        Err("extension kernels cannot mutate or unpack sequence/attribute targets yet".into())
    }
}

fn validate_body(body: &[ast::Stmt], names: &HashSet<String>) -> Result<(), String> {
    for stmt in body {
        match &stmt.kind {
            S::Assign { targets, value, .. } => {
                for t in targets { target(t)?; }
                expression(value, names)?;
            }
            S::AugAssign { target: t, value, .. } => { target(t)?; expression(value, names)?; }
            S::Return(Some(e)) | S::ExprStmt(e) => expression(e, names)?,
            S::If { branches, orelse } => {
                for (cond, body) in branches { expression(cond, names)?; validate_body(body, names)?; }
                validate_body(orelse, names)?;
            }
            S::While { cond, body, orelse } => { expression(cond, names)?; validate_body(body, names)?; validate_body(orelse, names)?; }
            S::For { target: t, iter, body, orelse } => { target(t)?; expression(iter, names)?; validate_body(body, names)?; validate_body(orelse, names)?; }
            S::Raise { message, .. } => expression(message, names)?,
            S::Assert { test, msg } => { expression(test, names)?; if let Some(msg) = msg { expression(msg, names)?; } }
            S::Try { body, handlers, orelse, finally } => {
                validate_body(body, names)?;
                for h in handlers { validate_body(&h.body, names)?; }
                validate_body(orelse, names)?; validate_body(finally, names)?;
            }
            S::Return(None) | S::Pass | S::Break | S::Continue => {}
            _ => return Err("extension kernels do not support imports, globals, nested definitions, callbacks or external effects yet".into()),
        }
    }
    Ok(())
}

fn expression(e: &ast::Expr, names: &HashSet<String>) -> Result<(), String> {
    match &e.kind {
        E::Int(_) | E::IntDigits(_) | E::Float(_) | E::Bool(_) | E::NoneLit | E::Str(_) | E::Name(_) => {}
        E::Unary { operand, .. } | E::Cast { arg: operand, .. } => expression(operand, names)?,
        E::Binary { op, left, right } => {
            if matches!(op, ast::BinOp::Is | ast::BinOp::IsNot) { return Err("extension kernels cannot depend on object identity".into()); }
            expression(left, names)?; expression(right, names)?;
        }
        E::Compare { first, rest } => {
            expression(first, names)?;
            for (op, e) in rest {
                if matches!(op, ast::BinOp::Is | ast::BinOp::IsNot) { return Err("extension kernels cannot depend on object identity".into()); }
                expression(e, names)?;
            }
        }
        E::Index { base, index } => { expression(base, names)?; expression(index, names)?; }
        E::Call { func, args, keywords, kwargs, .. } => {
            if !names.contains(func) && !matches!(func.as_str(), "len" | "range" | "abs" | "sum" | "min" | "max" | "round" | "pow" | "divmod") {
                return Err(format!("extension kernel call '{func}' is not supported; use numerical builtins or functions in this module"));
            }
            if kwargs.is_some() { return Err("extension kernels do not support ** call unpacking yet".into()); }
            for arg in args {
                match arg { ast::PosArg::Pos(e) => expression(e, names)?, _ => return Err("extension kernels do not support * call unpacking yet".into()) }
            }
            for kw in keywords { expression(&kw.value, names)?; }
        }
        _ => return Err("extension kernels support scalar expressions and read-only sequence indexing; methods, views, comprehensions and dynamic objects are not supported yet".into()),
    }
    Ok(())
}

fn validate_augmented(body: &[ast::Stmt], f: &ir::Function) -> Result<(), String> {
    for stmt in body {
        match &stmt.kind {
            S::AugAssign {
                target: ast::AssignTarget::Name { name, .. },
                ..
            } => {
                if f.params
                    .iter()
                    .chain(&f.locals)
                    .any(|(n, ty)| n == name && !scalar(*ty))
                {
                    return Err(format!(
                        "extension kernel '{}' cannot mutate sequence '{name}'",
                        f.name
                    ));
                }
            }
            S::If { branches, orelse } => {
                for (_, body) in branches {
                    validate_augmented(body, f)?;
                }
                validate_augmented(orelse, f)?;
            }
            S::While { body, orelse, .. } | S::For { body, orelse, .. } => {
                validate_augmented(body, f)?;
                validate_augmented(orelse, f)?;
            }
            S::Try {
                body,
                handlers,
                orelse,
                finally,
            } => {
                validate_augmented(body, f)?;
                for h in handlers {
                    validate_augmented(&h.body, f)?;
                }
                validate_augmented(orelse, f)?;
                validate_augmented(finally, f)?;
            }
            _ => {}
        }
    }
    Ok(())
}
