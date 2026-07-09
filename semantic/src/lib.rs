//! Semantic analysis: name resolution, type checking, and lowering the AST
//! into the typed IR.
//!
//! Typing rules (a statically-typed subset of Python, mypy-flavored):
//! - `int`, `float`, `bool`, `str`, `list[T]` values; `bool` is assignable
//!   to `int`, and `int`/`bool` are assignable to `float` (implicit
//!   promotion casts are inserted).
//! - a variable's type is fixed by its first assignment and cannot change.
//! - `/` is true division and always produces `float`; `//` and `%` follow
//!   Python's floored semantics; `**` on ints yields int (a negative
//!   exponent traps at runtime), on floats yields float.
//! - str supports `+` (concat), `*` int (repeat), comparisons, indexing,
//!   `len()`, and `str(...)` conversions.
//! - lists are homogeneous; they support indexing (read/write), `len()`,
//!   `.append(...)`, and iteration; assignment aliases (like Python).
//! - conditions accept any value with truthiness (numerics `!= 0`,
//!   str/list `len != 0`); `and`/`or`/`not` produce `bool`.
//! - `for` iterates `range(...)`, lists, and strings; it desugars to a
//!   `while` whose `continue` target runs the increment.
//!
//! The program entry is the top-level script statements; if there are none
//! but a zero-parameter `main` is defined, `main()` is called instead.

use std::collections::HashMap;

use common::{Diagnostic, Phase, Span};
use parser::ast;

pub fn ping() -> String {
    String::from("pong")
}

type SResult<T> = Result<T, Diagnostic>;

/// Name of the synthesized entry function holding top-level statements.
pub const ENTRY_NAME: &str = "__main__";

fn err(message: impl Into<String>, span: Span) -> Diagnostic {
    Diagnostic::new(Phase::Semantic, message, span)
}

fn resolve_type(ty: ast::TypeName) -> ir::Ty {
    match ty {
        ast::TypeName::Int => ir::Ty::Int,
        ast::TypeName::Float => ir::Ty::Float,
        ast::TypeName::Bool => ir::Ty::Bool,
        ast::TypeName::Str => ir::Ty::Str,
        ast::TypeName::File => ir::Ty::File,
        ast::TypeName::List(e) => ir::list_of(resolve_type(*e)),
        ast::TypeName::None => ir::Ty::None,
    }
}

fn elem_of(ty: ir::Ty, span: Span) -> SResult<ir::Ty> {
    match ty {
        ir::Ty::None => Err(err("list elements cannot be None", span)),
        ir::Ty::File => Err(err("files cannot be stored in lists yet", span)),
        other => Ok(other),
    }
}

#[derive(Debug, Clone)]
struct ParamSig {
    name: String,
    ty: ir::Ty,
    /// Cloned AST default; lowered at each call site when the arg is omitted.
    default: Option<ast::Expr>,
}

#[derive(Debug, Clone)]
struct FuncSig {
    params: Vec<ParamSig>,
    ret: ir::Ty,
    span: Span,
}

/// One parsed module handed to [`analyze_program`]. The driver supplies
/// these in topological order (dependencies first) with the root last;
/// the index doubles as the diagnostic file id.
pub struct ModuleInput<'a> {
    /// Import name: `"utils"` for `utils.py`, [`ENTRY_NAME`] for the root.
    pub name: String,
    pub ast: &'a ast::Module,
}

/// How a locally-visible imported name resolves.
#[derive(Debug, Clone)]
enum ImportBinding {
    /// `import sys [as s]`
    Sys,
    /// `import M [as m]` — the local name refers to module `M`.
    Module(String),
    /// `from M import x [as y]` — the local name refers to `M.x`.
    Symbol { module: String, name: String },
}

/// A fully analyzed module's exported surface, for cross-module lookup.
struct ModuleData {
    funcs: HashMap<String, FuncSig>,
    globals: HashMap<String, ir::Ty>,
}

/// Read-only per-module context threaded into every function lowering.
struct ModuleCtx<'a> {
    /// The module's own name (`""`-prefixed for the root, `"M."` otherwise
    /// via [`ModuleCtx::prefix`]).
    module: &'a str,
    is_root: bool,
    funcs: &'a HashMap<String, FuncSig>,
    imports: &'a HashMap<String, ImportBinding>,
    /// Fully analyzed dependency modules, keyed by name.
    mods: &'a HashMap<String, ModuleData>,
}

impl ModuleCtx<'_> {
    /// The IR name prefix for this module's own symbols. The root keeps
    /// bare names (`x`, `foo`); other modules are namespaced (`utils.x`).
    fn prefix(&self) -> String {
        if self.is_root {
            String::new()
        } else {
            format!("{}.", self.module)
        }
    }
}

/// The IR/emit name of `name` defined in module `module` (always
/// namespaced — only the root is bare, handled by the caller).
fn qual(module: &str, name: &str) -> String {
    format!("{module}.{name}")
}

/// The builtins that cannot be shadowed by a user `def`.
const BUILTINS: [&str; 10] = [
    "print", "len", "range", "input", "open", "abs", "min", "max", "sum", "sorted",
];

/// A call to a module's run-once init function, `<mod>.__init__()`.
fn init_call(module: &str) -> ir::Stmt {
    ir::Stmt::ExprStmt(ir::Expr {
        ty: ir::Ty::None,
        kind: ir::ExprKind::Call {
            func: qual(module, "__init__"),
            args: vec![],
        },
    })
}

/// Analyze a single module (no file imports beyond `sys`). Used by tests
/// and by the driver for single-file programs.
pub fn analyze(module: &ast::Module) -> SResult<ir::Module> {
    analyze_program(&[ModuleInput {
        name: ENTRY_NAME.to_string(),
        ast: module,
    }])
}

/// Collect a module's function signatures (with builtin-shadowing and
/// duplicate checks).
fn collect_sigs(module: &ast::Module) -> SResult<(HashMap<String, FuncSig>, Vec<&ast::FuncDef>)> {
    let mut funcs: HashMap<String, FuncSig> = HashMap::new();
    let mut order: Vec<&ast::FuncDef> = Vec::new();
    for stmt in &module.body {
        if let ast::StmtKind::FuncDef(f) = &stmt.kind {
            if BUILTINS.contains(&f.name.as_str()) {
                return Err(err(
                    format!("cannot redefine the builtin '{}'", f.name),
                    f.span,
                ));
            }
            if funcs.contains_key(&f.name) {
                return Err(err(
                    format!("function '{}' is defined more than once", f.name),
                    f.span,
                ));
            }
            let mut params = Vec::new();
            let mut seen_names = std::collections::HashSet::new();
            for p in &f.params {
                let ty = resolve_type(p.ty);
                if ty == ir::Ty::None {
                    return Err(err(
                        format!("parameter '{}' cannot have type None", p.name),
                        p.span,
                    ));
                }
                if !seen_names.insert(p.name.clone()) {
                    return Err(err(
                        format!("duplicate parameter name '{}'", p.name),
                        p.span,
                    ));
                }
                params.push(ParamSig {
                    name: p.name.clone(),
                    ty,
                    default: p.default.clone(),
                });
            }
            let ret = f.ret.map(resolve_type).unwrap_or(ir::Ty::None);
            funcs.insert(
                f.name.clone(),
                FuncSig {
                    params,
                    ret,
                    span: f.span,
                },
            );
            order.push(f);
        }
    }
    Ok((funcs, order))
}

/// The names a module binds at the top level (assignment targets), so
/// `from M import x` can be validated before M is lowered.
fn collect_global_names(module: &ast::Module) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for stmt in &module.body {
        match &stmt.kind {
            ast::StmtKind::Assign { targets, .. } => {
                for t in targets {
                    if let ast::AssignTarget::Name { name, .. } = t {
                        names.insert(name.clone());
                    }
                }
            }
            ast::StmtKind::AugAssign {
                target: ast::AssignTarget::Name { name, .. },
                ..
            } => {
                names.insert(name.clone());
            }
            _ => {}
        }
    }
    names
}

/// Build a module's import bindings (local name → target), validating that
/// imported modules and symbols exist.
fn collect_imports(
    module: &ast::Module,
    self_name: &str,
    all_funcs: &HashMap<String, HashMap<String, FuncSig>>,
    all_globals: &HashMap<String, std::collections::HashSet<String>>,
) -> SResult<HashMap<String, ImportBinding>> {
    let mut imports: HashMap<String, ImportBinding> = HashMap::new();
    for stmt in &module.body {
        match &stmt.kind {
            ast::StmtKind::Import {
                module: m,
                alias,
                span,
            } => {
                let local = alias.clone().unwrap_or_else(|| m.clone());
                let binding = if m == "sys" {
                    ImportBinding::Sys
                } else {
                    if m == self_name {
                        return Err(err(format!("module '{m}' cannot import itself"), *span));
                    }
                    ImportBinding::Module(m.clone())
                };
                imports.insert(local, binding);
            }
            ast::StmtKind::FromImport {
                module: m,
                names,
                span,
            } => {
                if m == "sys" {
                    return Err(err(
                        "'from sys import ...' is not supported; use 'import sys' \
                         and 'sys.argv'",
                        *span,
                    ));
                }
                if m == self_name {
                    return Err(err(
                        format!("module '{m}' cannot import from itself"),
                        *span,
                    ));
                }
                let mfuncs = all_funcs.get(m);
                let mglobals = all_globals.get(m);
                for (name, alias, nspan) in names {
                    let is_func = mfuncs.is_some_and(|f| f.contains_key(name));
                    let is_global = mglobals.is_some_and(|g| g.contains(name));
                    if !is_func && !is_global {
                        return Err(err(
                            format!("cannot import name '{name}' from '{m}'"),
                            *nspan,
                        ));
                    }
                    let local = alias.clone().unwrap_or_else(|| name.clone());
                    imports.insert(
                        local,
                        ImportBinding::Symbol {
                            module: m.clone(),
                            name: name.clone(),
                        },
                    );
                }
            }
            _ => {}
        }
    }
    Ok(imports)
}

/// Analyze a whole program: several modules with cross-file imports.
/// `modules` is in topological order (dependencies first, root last);
/// diagnostics are tagged with the module index as their file id.
pub fn analyze_program(modules: &[ModuleInput]) -> SResult<ir::Module> {
    assert!(!modules.is_empty(), "a program needs at least one module");
    let root_idx = modules.len() - 1;

    // pass 1: every module's signatures and global-name surface
    let mut all_funcs: HashMap<String, HashMap<String, FuncSig>> = HashMap::new();
    let mut all_orders: Vec<Vec<&ast::FuncDef>> = Vec::new();
    let mut all_global_names: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    for (i, m) in modules.iter().enumerate() {
        let (funcs, order) = collect_sigs(m.ast).map_err(|d| d.with_file(i))?;
        all_funcs.insert(m.name.clone(), funcs);
        all_orders.push(order);
        all_global_names.insert(m.name.clone(), collect_global_names(m.ast));
    }

    // pass 2: import bindings (validated against the collected surface)
    let mut all_imports: Vec<HashMap<String, ImportBinding>> = Vec::new();
    for (i, m) in modules.iter().enumerate() {
        let imports = collect_imports(m.ast, &m.name, &all_funcs, &all_global_names)
            .map_err(|d| d.with_file(i))?;
        all_imports.push(imports);
    }

    // pass 3: lower each module in dependency order, accumulating results
    let mut mods: HashMap<String, ModuleData> = HashMap::new();
    let mut out_funcs: Vec<ir::Function> = Vec::new();
    let mut out_globals: Vec<(String, ir::Ty)> = Vec::new();

    for (i, m) in modules.iter().enumerate() {
        let is_root = i == root_idx;
        let funcs = &all_funcs[&m.name];
        let mctx = ModuleCtx {
            module: &m.name,
            is_root,
            funcs,
            imports: &all_imports[i],
            mods: &mods,
        };

        let mut globals: HashMap<String, ir::Ty> = HashMap::new();
        let mut globals_order: Vec<(String, ir::Ty)> = Vec::new();
        let script: Vec<ast::Stmt> = m
            .ast
            .body
            .iter()
            .filter(|s| !matches!(s.kind, ast::StmtKind::FuncDef(_)))
            .cloned()
            .collect();

        // the module's top-level statements become its init function; for
        // the root that IS the entry, otherwise `<mod>.__init__` guarded to
        // run once
        let init_name = if is_root {
            ENTRY_NAME.to_string()
        } else {
            qual(&m.name, "__init__")
        };

        let init = if is_root && script.is_empty() {
            // PyRs convenience: a root that is only definitions calls main()
            if let Some(sig) = funcs.get("main") {
                if !sig.params.is_empty() {
                    return Err(err(
                        "main() is used as the entry point and cannot take parameters",
                        sig.span,
                    )
                    .with_file(i));
                }
                Some(ir::Function {
                    name: ENTRY_NAME.to_string(),
                    params: vec![],
                    ret: ir::Ty::None,
                    locals: vec![],
                    body: vec![ir::Stmt::ExprStmt(ir::Expr {
                        ty: sig.ret,
                        kind: ir::ExprKind::Call {
                            func: "main".to_string(),
                            args: vec![],
                        },
                    })],
                })
            } else {
                None
            }
        } else {
            let init_def = ast::FuncDef {
                name: init_name.clone(),
                params: vec![],
                ret: None,
                body: script,
                span: Span::default(),
            };
            let mut f = lower_function(&init_def, &mctx, &mut globals, &mut globals_order, true)
                .map_err(|d| d.with_file(i))?;
            if !is_root {
                add_init_guard(&mut f, &m.name, &mut globals_order);
            }
            Some(f)
        };

        // functions, with the module's globals now typed
        for fd in &all_orders[i] {
            let f = lower_function(fd, &mctx, &mut globals, &mut globals_order, false)
                .map_err(|d| d.with_file(i))?;
            out_funcs.push(f);
        }

        match init {
            Some(f) => out_funcs.push(f),
            None if is_root => {
                return Err(err(
                    "program has no entry point: add top-level statements or define main()",
                    Span::default(),
                )
                .with_file(i));
            }
            // a non-root module with only definitions has no init to call
            None => {}
        }

        out_globals.extend(globals_order.iter().cloned());
        mods.insert(
            m.name.clone(),
            ModuleData {
                funcs: funcs.clone(),
                globals,
            },
        );
    }

    Ok(ir::Module {
        funcs: out_funcs,
        globals: out_globals,
        entry: ENTRY_NAME.to_string(),
    })
}

/// Prepend a run-once guard to a module init: `if <mod>.__done__: return;
/// <mod>.__done__ = True; ...`.
fn add_init_guard(f: &mut ir::Function, module: &str, globals_order: &mut Vec<(String, ir::Ty)>) {
    let done = qual(module, "__done__");
    globals_order.push((done.clone(), ir::Ty::Bool));
    let guard = ir::Stmt::If {
        branches: vec![(
            ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::GlobalLoad(done.clone()),
            },
            vec![ir::Stmt::Return(None)],
        )],
        orelse: vec![],
    };
    let set = ir::Stmt::GlobalAssign {
        name: done,
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(true),
        },
    };
    let mut body = vec![guard, set];
    body.append(&mut f.body);
    f.body = body;
}

struct FnCtx<'a> {
    mctx: &'a ModuleCtx<'a>,
    globals: &'a mut HashMap<String, ir::Ty>,
    globals_order: &'a mut Vec<(String, ir::Ty)>,
    /// This function is a module init: its top-level bindings are globals.
    is_entry: bool,
    /// Names this function declared with `global`.
    declared_globals: std::collections::HashSet<String>,
    fn_name: String,
    ret: ir::Ty,
    locals: HashMap<String, ir::Ty>,
    locals_order: Vec<(String, ir::Ty)>,
    loop_depth: usize,
    temp_counter: usize,
    /// Active comprehension variables: user name → (storage local, type).
    /// Innermost last. Comprehension variables shadow but never leak
    /// (Python 3 scoping).
    comp_renames: Vec<(String, String, ir::Ty)>,
}

impl FnCtx<'_> {
    /// A compiler-synthesized local. The leading '.' keeps it out of the
    /// user namespace (Python identifiers cannot start with '.').
    fn fresh_temp(&mut self, hint: &str, ty: ir::Ty) -> String {
        self.temp_counter += 1;
        let name = format!(".{hint}{}", self.temp_counter);
        self.locals_order.push((name.clone(), ty));
        name
    }

    /// Does an assignment to `name` target a module global here?
    fn binds_global(&self, name: &str) -> bool {
        self.is_entry || self.declared_globals.contains(name)
    }

    /// This module's functions.
    fn funcs(&self) -> &HashMap<String, FuncSig> {
        self.mctx.funcs
    }

    /// The IR/emit name for one of *this* module's own globals.
    fn own_global(&self, name: &str) -> String {
        format!("{}{}", self.mctx.prefix(), name)
    }

    /// The IR/emit name for one of *this* module's own functions.
    fn own_func(&self, name: &str) -> String {
        format!("{}{}", self.mctx.prefix(), name)
    }

    /// Is `sys` imported (under any alias)?
    fn sys_alias(&self, name: &str) -> bool {
        matches!(self.mctx.imports.get(name), Some(ImportBinding::Sys))
    }

    /// If `name` is an imported module alias, its real module name.
    fn module_alias(&self, name: &str) -> Option<String> {
        match self.mctx.imports.get(name) {
            Some(ImportBinding::Module(real)) => Some(real.clone()),
            _ => None,
        }
    }
}

fn lower_function(
    f: &ast::FuncDef,
    mctx: &ModuleCtx,
    globals: &mut HashMap<String, ir::Ty>,
    globals_order: &mut Vec<(String, ir::Ty)>,
    is_entry: bool,
) -> SResult<ir::Function> {
    let mut params = Vec::new();
    let mut ctx = FnCtx {
        mctx,
        globals,
        globals_order,
        is_entry,
        declared_globals: std::collections::HashSet::new(),
        fn_name: f.name.clone(),
        ret: f.ret.map(resolve_type).unwrap_or(ir::Ty::None),
        locals: HashMap::new(),
        locals_order: Vec::new(),
        loop_depth: 0,
        temp_counter: 0,
        comp_renames: Vec::new(),
    };

    for p in &f.params {
        let ty = resolve_type(p.ty);
        if ctx.locals.insert(p.name.clone(), ty).is_some() {
            return Err(err(format!("duplicate parameter '{}'", p.name), p.span));
        }
        params.push((p.name.clone(), ty));
    }

    let body = lower_block(&f.body, &mut ctx)?;

    // every path through a value-returning function must return
    if ctx.ret != ir::Ty::None && !block_returns(&body) {
        return Err(err(
            format!(
                "function '{}' is declared to return {} but can reach the end \
                 of its body without a return statement",
                f.name, ctx.ret
            ),
            f.span,
        ));
    }

    // functions keep their given name for the init (already qualified); a
    // regular function is namespaced by its module
    let ir_name = if f.name == ENTRY_NAME || f.name.contains('.') {
        f.name.clone()
    } else {
        ctx.own_func(&f.name)
    };

    Ok(ir::Function {
        name: ir_name,
        params,
        ret: ctx.ret,
        locals: ctx.locals_order,
        body,
    })
}

fn lower_block(stmts: &[ast::Stmt], ctx: &mut FnCtx) -> SResult<Vec<ir::Stmt>> {
    let mut out = Vec::new();
    for stmt in stmts {
        lower_stmt(stmt, ctx, &mut out)?;
    }
    Ok(out)
}

fn lower_stmt(stmt: &ast::Stmt, ctx: &mut FnCtx, out: &mut Vec<ir::Stmt>) -> SResult<()> {
    match &stmt.kind {
        ast::StmtKind::FuncDef(f) => Err(err(
            format!(
                "nested function definitions are not supported yet ('{}')",
                f.name
            ),
            f.span,
        )),
        ast::StmtKind::Pass => Ok(()),
        ast::StmtKind::Import { module, span, .. } => {
            if !ctx.is_entry {
                return Err(err(
                    "imports are only supported at the top level of the program",
                    *span,
                ));
            }
            // `import sys` needs no init; any other module runs its body
            // (once, guarded) at this point
            if module != "sys" {
                out.push(init_call(module));
            }
            Ok(())
        }
        ast::StmtKind::FromImport { module, span, .. } => {
            if !ctx.is_entry {
                return Err(err(
                    "imports are only supported at the top level of the program",
                    *span,
                ));
            }
            out.push(init_call(module));
            Ok(())
        }
        ast::StmtKind::With { item, target, body } => {
            lower_with(item, target.as_ref(), body, ctx, out)
        }
        ast::StmtKind::Global(names) => {
            // a no-op at module level, like Python
            if ctx.is_entry {
                return Ok(());
            }
            for (name, span) in names {
                if ctx.locals.contains_key(name) {
                    return Err(err(
                        format!(
                            "'{name}' is already a parameter or local here; \
                             the 'global' declaration must come before any use"
                        ),
                        *span,
                    ));
                }
                if !ctx.globals.contains_key(name) {
                    return Err(err(
                        format!(
                            "no global '{name}' is assigned at the top level \
                             of the program"
                        ),
                        *span,
                    ));
                }
                ctx.declared_globals.insert(name.clone());
            }
            Ok(())
        }
        ast::StmtKind::Break => {
            if ctx.loop_depth == 0 {
                return Err(err("'break' outside of a loop", stmt.span));
            }
            out.push(ir::Stmt::Break);
            Ok(())
        }
        ast::StmtKind::Continue => {
            if ctx.loop_depth == 0 {
                return Err(err("'continue' outside of a loop", stmt.span));
            }
            out.push(ir::Stmt::Continue);
            Ok(())
        }
        ast::StmtKind::Return(value) => {
            match (value, ctx.ret) {
                (None, ir::Ty::None) => out.push(ir::Stmt::Return(None)),
                (None, expected) => {
                    return Err(err(
                        format!(
                            "function '{}' must return a value of type {}",
                            ctx.fn_name, expected
                        ),
                        stmt.span,
                    ));
                }
                (Some(e), ir::Ty::None) => {
                    // `return None` is fine in a None function
                    if matches!(e.kind, ast::ExprKind::NoneLit) {
                        out.push(ir::Stmt::Return(None));
                        return Ok(());
                    }
                    return Err(err(
                        format!(
                            "function '{}' does not declare a return type; \
                             annotate it (e.g. 'def {}(...) -> int:') to return a value",
                            ctx.fn_name, ctx.fn_name
                        ),
                        e.span,
                    ));
                }
                (Some(e), expected) => {
                    // `return []` needs the declared type for inference
                    let value = if let (ast::ExprKind::ListLit(items), ir::Ty::List(elem)) =
                        (&e.kind, expected)
                    {
                        lower_list_lit(items, Some(*elem), e.span, ctx)?
                    } else {
                        let v = lower_expr(e, ctx)?;
                        coerce(v, expected, e.span, "return value")?
                    };
                    out.push(ir::Stmt::Return(Some(value)));
                }
            }
            Ok(())
        }
        ast::StmtKind::Assign {
            targets,
            annotation,
            value,
        } => {
            if targets.is_empty() {
                return Err(err("assignment has no targets", stmt.span));
            }
            if targets.len() == 1 {
                return lower_assign(&targets[0], *annotation, value, ctx, out);
            }
            if annotation.is_some() {
                return Err(err(
                    "type annotations are not allowed in multi-target assignment",
                    stmt.span,
                ));
            }
            // evaluate RHS once, then assign right-to-left (Python order)
            let value_ir = lower_expr(value, ctx)?;
            if value_ir.ty == ir::Ty::None {
                return Err(err(
                    "cannot assign: the expression has no value (returns None)",
                    value.span,
                ));
            }
            let tmp = ctx.fresh_temp("multi", value_ir.ty);
            out.push(ir::Stmt::Assign {
                name: tmp.clone(),
                value: value_ir.clone(),
            });
            let load = ir::Expr {
                ty: value_ir.ty,
                kind: ir::ExprKind::Local(tmp),
            };
            for target in targets.iter().rev() {
                lower_assign_ir(target, None, load.clone(), value.span, ctx, out)?;
            }
            Ok(())
        }
        ast::StmtKind::AugAssign { target, op, value } => {
            lower_aug_assign(target, *op, value, stmt.span, ctx, out)
        }
        ast::StmtKind::ExprStmt(e) => {
            // print is a statement-level builtin
            if let ast::ExprKind::Call { func, args, .. } = &e.kind
                && func == "print"
                && !ctx.funcs().contains_key("print")
            {
                let mut lowered_args = Vec::new();
                for (i, arg) in args.iter().enumerate() {
                    let a = lower_expr(arg, ctx)?;
                    if a.ty == ir::Ty::None {
                        return Err(err(
                            format!("print argument {} has no value (returns None)", i + 1),
                            arg.span,
                        ));
                    }
                    if a.ty == ir::Ty::File {
                        return Err(err("file objects cannot be printed yet", arg.span));
                    }
                    lowered_args.push(a);
                }
                out.push(ir::Stmt::Print(lowered_args));
                return Ok(());
            }
            // xs.append(v) is a statement in the IR
            if let ast::ExprKind::MethodCall {
                base,
                method,
                method_span,
                args,
                keywords,
            } = &e.kind
            {
                // `module.func(args)` as a statement discards the result
                if let ast::ExprKind::Name(alias) = &base.kind
                    && let Some(real) = ctx.module_alias(alias)
                {
                    let call = lower_module_call(&real, method, *method_span, args, keywords, ctx)?;
                    out.push(ir::Stmt::ExprStmt(call));
                    return Ok(());
                }
                if !keywords.is_empty() {
                    return Err(err(
                        "keyword arguments are not supported for this method call",
                        keywords[0].name_span,
                    ));
                }
                let stmt = lower_method_stmt(base, method, *method_span, args, ctx)?;
                out.push(stmt);
                return Ok(());
            }
            let lowered = lower_expr(e, ctx)?;
            out.push(ir::Stmt::ExprStmt(lowered));
            Ok(())
        }
        ast::StmtKind::If { branches, orelse } => {
            let mut lowered_branches = Vec::new();
            for (cond, body) in branches {
                let c = lower_condition(cond, ctx)?;
                let b = lower_block(body, ctx)?;
                lowered_branches.push((c, b));
            }
            let lowered_orelse = lower_block(orelse, ctx)?;
            out.push(ir::Stmt::If {
                branches: lowered_branches,
                orelse: lowered_orelse,
            });
            Ok(())
        }
        ast::StmtKind::While { cond, body, orelse } => {
            let c = lower_condition(cond, ctx)?;
            ctx.loop_depth += 1;
            let b = lower_block(body, ctx)?;
            ctx.loop_depth -= 1;
            push_loop_with_else(c, b, vec![], orelse, ctx, out)
        }
        ast::StmtKind::For {
            var,
            var_span,
            iter,
            body,
            orelse,
        } => lower_for(var, *var_span, iter, body, orelse, ctx, out),
    }
}

/// `with open(...) as f:` — files only. Desugars to bind + body + close,
/// with the close also inserted before every early exit (return, or a
/// break/continue that leaves the with-block), matching Python's
/// try/finally semantics. Runtime traps exit the process, so they need
/// no handling.
fn lower_with(
    item: &ast::Expr,
    target: Option<&(String, Span)>,
    body: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let item_ir = lower_expr(item, ctx)?;
    if item_ir.ty != ir::Ty::File {
        return Err(err(
            format!(
                "'{}' object does not support the context manager protocol",
                item_ir.ty
            ),
            item.span,
        ));
    }

    // bind the handle: `as name` uses the user's variable, otherwise a temp
    let (load_handle, bind_stmt) = match target {
        Some((name, name_span)) => {
            let bind = bind_name(name, *name_span, None, item_ir, item.span, ctx)?;
            let load = if ctx.binds_global(name) {
                ir::ExprKind::GlobalLoad(ctx.own_global(name))
            } else {
                ir::ExprKind::Local(name.clone())
            };
            (
                ir::Expr {
                    ty: ir::Ty::File,
                    kind: load,
                },
                bind,
            )
        }
        Option::None => {
            let t = ctx.fresh_temp("with", ir::Ty::File);
            let load = ir::Expr {
                ty: ir::Ty::File,
                kind: ir::ExprKind::Local(t.clone()),
            };
            (
                load,
                ir::Stmt::Assign {
                    name: t,
                    value: item_ir,
                },
            )
        }
    };
    out.push(bind_stmt);

    let close_stmt = || {
        ir::Stmt::ExprStmt(ir::Expr {
            ty: ir::Ty::None,
            kind: ir::ExprKind::FileCall {
                func: ir::FileFn::Close,
                args: vec![load_handle.clone()],
            },
        })
    };

    let mut lowered = lower_block(body, ctx)?;
    insert_closes(&mut lowered, &close_stmt(), false, ctx);
    out.extend(lowered);
    out.push(close_stmt());
    Ok(())
}

/// Insert `close` before every statement that exits the with-block.
/// `return` always exits (even from nested loops); `break`/`continue`
/// exit only when they belong to a loop *enclosing* the with, i.e. when
/// we are not inside a loop nested within the with-body.
fn insert_closes(
    stmts: &mut Vec<ir::Stmt>,
    close: &ir::Stmt,
    in_nested_loop: bool,
    ctx: &mut FnCtx,
) {
    let mut i = 0;
    while i < stmts.len() {
        match &mut stmts[i] {
            ir::Stmt::Return(value) => {
                // evaluate the return value BEFORE closing: Python runs
                // the finally after the return expression
                if let Some(v) = value.take() {
                    let ty = v.ty;
                    let t = ctx.fresh_temp("with.ret", ty);
                    let assign = ir::Stmt::Assign {
                        name: t.clone(),
                        value: v,
                    };
                    *value = Some(ir::Expr {
                        ty,
                        kind: ir::ExprKind::Local(t),
                    });
                    stmts.insert(i, close.clone());
                    stmts.insert(i, assign);
                    i += 2;
                } else {
                    stmts.insert(i, close.clone());
                    i += 1;
                }
            }
            ir::Stmt::Break | ir::Stmt::Continue if !in_nested_loop => {
                stmts.insert(i, close.clone());
                i += 1;
            }
            ir::Stmt::If { branches, orelse } => {
                for (_, b) in branches.iter_mut() {
                    insert_closes(b, close, in_nested_loop, ctx);
                }
                insert_closes(orelse, close, in_nested_loop, ctx);
            }
            ir::Stmt::While { body, step, .. } => {
                // break/continue inside this loop stay inside the with
                insert_closes(body, close, true, ctx);
                insert_closes(step, close, true, ctx);
            }
            _ => {}
        }
        i += 1;
    }
}

/// Method calls in statement position (`xs.append(v)`, `xs.pop()`,
/// `s.upper()` with the result discarded).
fn lower_method_stmt(
    base: &ast::Expr,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    let base_ir = lower_expr(base, ctx)?;
    match base_ir.ty {
        ir::Ty::List(elem) => match method {
            "append" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("append() takes exactly one argument ({} given)", args.len()),
                        method_span,
                    ));
                }
                let value = lower_expr(&args[0], ctx)?;
                let value = coerce(value, *elem, args[0].span, "append() argument")?;
                Ok(ir::Stmt::ListAppend {
                    list: base_ir,
                    value,
                })
            }
            "insert" => {
                if args.len() != 2 {
                    return Err(err(
                        format!("insert() takes exactly 2 arguments ({} given)", args.len()),
                        method_span,
                    ));
                }
                let index = lower_expr(&args[0], ctx)?;
                let index = coerce(index, ir::Ty::Int, args[0].span, "insert() index")?;
                let value = lower_expr(&args[1], ctx)?;
                let value = coerce(value, *elem, args[1].span, "insert() argument")?;
                Ok(ir::Stmt::ListInsert {
                    list: base_ir,
                    index,
                    value,
                })
            }
            "remove" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("remove() takes exactly one argument ({} given)", args.len()),
                        method_span,
                    ));
                }
                let value = lower_expr(&args[0], ctx)?;
                let value = coerce(value, *elem, args[0].span, "remove() argument")?;
                Ok(ir::Stmt::ListRemove {
                    list: base_ir,
                    value,
                })
            }
            "clear" => {
                if !args.is_empty() {
                    return Err(err(
                        format!("clear() takes no arguments ({} given)", args.len()),
                        method_span,
                    ));
                }
                Ok(ir::Stmt::ListClear { list: base_ir })
            }
            "sort" => {
                if !args.is_empty() {
                    return Err(err(
                        format!(
                            "sort() takes no arguments ({} given); key=/reverse= \
                             are not supported yet",
                            args.len()
                        ),
                        method_span,
                    ));
                }
                ensure_sortable_list_elem(*elem, method_span)?;
                Ok(ir::Stmt::ListSort { list: base_ir })
            }
            // pop / index as statements discard the result
            "pop" => {
                let pop = lower_list_pop(base_ir, *elem, args, method_span, ctx)?;
                Ok(ir::Stmt::ExprStmt(pop))
            }
            "index" => {
                let idx = lower_list_index_of(base_ir, *elem, args, method_span, ctx)?;
                Ok(ir::Stmt::ExprStmt(idx))
            }
            _ => Err(err(
                format!(
                    "list method '{method}' is not supported yet (supported: \
                     append, pop, insert, remove, index, clear, sort)"
                ),
                method_span,
            )),
        },
        ir::Ty::Str => {
            let call = lower_str_method(base_ir, method, method_span, args, ctx)?;
            Ok(ir::Stmt::ExprStmt(call))
        }
        ir::Ty::File => {
            let call = lower_file_method(base_ir, method, method_span, args, ctx)?;
            Ok(ir::Stmt::ExprStmt(call))
        }
        other => Err(err(
            format!("'{other}' has no method '{method}'"),
            method_span,
        )),
    }
}

/// The supported file methods.
fn lower_file_method(
    base_ir: ir::Expr,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    use ir::FileFn::*;

    let (func, ret, takes_str_arg) = match method {
        "read" => (Read, ir::Ty::Str, false),
        "readline" => (ReadLine, ir::Ty::Str, false),
        "readlines" => (ReadLines, ir::list_of(ir::Ty::Str), false),
        "write" => (Write, ir::Ty::Int, true),
        "close" => (Close, ir::Ty::None, false),
        _ => {
            return Err(err(
                format!(
                    "file method '{method}' is not supported yet (supported: \
                     read, readline, readlines, write, close)"
                ),
                method_span,
            ));
        }
    };

    let expected_args = usize::from(takes_str_arg);
    if args.len() != expected_args {
        return Err(err(
            format!(
                "{method}() takes exactly {expected_args} argument(s) ({} given)",
                args.len()
            ),
            method_span,
        ));
    }

    let mut call_args = vec![base_ir];
    if takes_str_arg {
        let a = lower_expr(&args[0], ctx)?;
        if a.ty != ir::Ty::Str {
            return Err(err(
                format!("{method}() expects a str argument, found {}", a.ty),
                args[0].span,
            ));
        }
        call_args.push(a);
    }
    Ok(ir::Expr {
        ty: ret,
        kind: ir::ExprKind::FileCall {
            func,
            args: call_args,
        },
    })
}

/// The supported `str` methods (ASCII case/whitespace rules).
fn lower_str_method(
    base_ir: ir::Expr,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    use ir::StrFn::*;

    // (runtime function, result type, extra str args expected)
    let (func, ret, str_args): (ir::StrFn, ir::Ty, usize) = match method {
        "upper" => (Upper, ir::Ty::Str, 0),
        "lower" => (Lower, ir::Ty::Str, 0),
        "strip" => (Strip, ir::Ty::Str, 0),
        "lstrip" => (Lstrip, ir::Ty::Str, 0),
        "rstrip" => (Rstrip, ir::Ty::Str, 0),
        "startswith" => (StartsWith, ir::Ty::Bool, 1),
        "endswith" => (EndsWith, ir::Ty::Bool, 1),
        "find" => (Find, ir::Ty::Int, 1),
        "rfind" => (RFind, ir::Ty::Int, 1),
        "rindex" => (RIndex, ir::Ty::Int, 1),
        "count" => (Count, ir::Ty::Int, 1),
        "replace" => (Replace, ir::Ty::Str, 2),
        "split" => {
            return lower_str_split(base_ir, args, method_span, ctx);
        }
        "join" => {
            if args.len() != 1 {
                return Err(err(
                    format!("join() takes exactly one argument ({} given)", args.len()),
                    method_span,
                ));
            }
            let parts = lower_expr(&args[0], ctx)?;
            if parts.ty != ir::list_of(ir::Ty::Str) {
                return Err(err(
                    format!("join() expects a list[str], found {}", parts.ty),
                    args[0].span,
                ));
            }
            return Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::StrCall {
                    func: Join,
                    args: vec![base_ir, parts],
                },
            });
        }
        "isdigit" => (IsDigit, ir::Ty::Bool, 0),
        "isalpha" => (IsAlpha, ir::Ty::Bool, 0),
        "isspace" => (IsSpace, ir::Ty::Bool, 0),
        "isupper" => (IsUpper, ir::Ty::Bool, 0),
        "islower" => (IsLower, ir::Ty::Bool, 0),
        _ => {
            return Err(err(
                format!(
                    "str method '{method}' is not supported yet (supported: \
                     upper, lower, strip, lstrip, rstrip, startswith, \
                     endswith, find, rfind, rindex, count, replace, split, \
                     join, isdigit, isalpha, isspace, isupper, islower)"
                ),
                method_span,
            ));
        }
    };

    if args.len() != str_args {
        return Err(err(
            format!(
                "{method}() takes exactly {str_args} argument(s) ({} given)",
                args.len()
            ),
            method_span,
        ));
    }
    let mut call_args = vec![base_ir];
    for arg in args {
        let a = lower_expr(arg, ctx)?;
        if a.ty != ir::Ty::Str {
            return Err(err(
                format!("{method}() expects str arguments, found {}", a.ty),
                arg.span,
            ));
        }
        call_args.push(a);
    }
    Ok(ir::Expr {
        ty: ret,
        kind: ir::ExprKind::StrCall {
            func,
            args: call_args,
        },
    })
}

fn lower_str_split(
    base_ir: ir::Expr,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let (func, call_args) = match args {
        [] => (ir::StrFn::SplitWs, vec![base_ir]),
        [sep] => {
            let s = lower_expr(sep, ctx)?;
            if s.ty != ir::Ty::Str {
                return Err(err(
                    format!("split() separator must be a str, found {}", s.ty),
                    sep.span,
                ));
            }
            if matches!(&s.kind, ir::ExprKind::ConstStr(c) if c.is_empty()) {
                return Err(err("empty separator", sep.span));
            }
            (ir::StrFn::Split, vec![base_ir, s])
        }
        _ => {
            return Err(err(
                format!("split() takes at most one argument ({} given)", args.len()),
                method_span,
            ));
        }
    };
    Ok(ir::Expr {
        ty: ir::list_of(ir::Ty::Str),
        kind: ir::ExprKind::StrCall {
            func,
            args: call_args,
        },
    })
}

fn lower_list_pop(
    list: ir::Expr,
    elem: ir::Ty,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let index = match args {
        [] => int_const(-1),
        [arg] => {
            let i = lower_expr(arg, ctx)?;
            coerce(i, ir::Ty::Int, arg.span, "pop() index")?
        }
        _ => {
            return Err(err(
                format!("pop() takes at most one argument ({} given)", args.len()),
                method_span,
            ));
        }
    };
    Ok(ir::Expr {
        ty: elem,
        kind: ir::ExprKind::ListPop {
            list: Box::new(list),
            index: Box::new(index),
        },
    })
}

fn ensure_sortable_list_elem(elem: ir::Ty, span: Span) -> SResult<()> {
    match elem {
        ir::Ty::Int | ir::Ty::Float | ir::Ty::Bool | ir::Ty::Str => Ok(()),
        other => Err(err(
            format!(
                "sort is only supported for list[int], list[float], list[bool], \
                 and list[str], found list[{other}]"
            ),
            span,
        )),
    }
}

fn lower_list_index_of(
    list: ir::Expr,
    elem: ir::Ty,
    args: &[ast::Expr],
    method_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if args.len() != 1 {
        return Err(err(
            format!("index() takes exactly one argument ({} given)", args.len()),
            method_span,
        ));
    }
    let value = lower_expr(&args[0], ctx)?;
    let value = coerce(value, elem, args[0].span, "index() argument")?;
    Ok(ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::ListIndexOf {
            list: Box::new(list),
            value: Box::new(value),
        },
    })
}

fn lower_assign(
    target: &ast::AssignTarget,
    annotation: Option<ast::TypeName>,
    value: &ast::Expr,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let ann_ty = annotation.map(resolve_type);
    // `xs: list[int] = [...]` / `= []`: propagate the element type
    let lowered =
        if let (ast::ExprKind::ListLit(items), Some(ir::Ty::List(elem))) = (&value.kind, ann_ty) {
            lower_list_lit(items, Some(*elem), value.span, ctx)?
        } else {
            lower_expr(value, ctx)?
        };
    lower_assign_ir(target, ann_ty, lowered, value.span, ctx, out)
}

/// Assign an already-lowered IR value to a target (used by multi-assign).
fn lower_assign_ir(
    target: &ast::AssignTarget,
    ann_ty: Option<ir::Ty>,
    value_ir: ir::Expr,
    value_span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    match target {
        ast::AssignTarget::Name { name, span } => {
            let stmt = bind_name(name, *span, ann_ty, value_ir, value_span, ctx)?;
            out.push(stmt);
            Ok(())
        }
        ast::AssignTarget::Index { base, index } => {
            if ann_ty.is_some() {
                return Err(err(
                    "type annotations are only allowed on plain variable names",
                    value_span,
                ));
            }
            let (list_ir, elem, index_ir) = lower_index_target(base, index, ctx)?;
            let value_ir = coerce(value_ir, elem, value_span, "list element assignment")?;
            out.push(ir::Stmt::IndexAssign {
                base: list_ir,
                index: index_ir,
                value: value_ir,
            });
            Ok(())
        }
    }
}

/// Check and lower the target of `base[index] = ...`.
fn lower_index_target(
    base: &ast::Expr,
    index: &ast::Expr,
    ctx: &mut FnCtx,
) -> SResult<(ir::Expr, ir::Ty, ir::Expr)> {
    let base_ir = lower_expr(base, ctx)?;
    let elem = match base_ir.ty {
        ir::Ty::List(e) => e,
        ir::Ty::Str => {
            return Err(err(
                "'str' object does not support item assignment (strings are \
                 immutable)",
                base.span,
            ));
        }
        other => {
            return Err(err(
                format!("'{other}' object does not support item assignment"),
                base.span,
            ));
        }
    };
    let index_ir = lower_expr(index, ctx)?;
    let index_ir = coerce(index_ir, ir::Ty::Int, index.span, "list index")?;
    Ok((base_ir, *elem, index_ir))
}

/// Bind `name = value_ir`, inferring or checking the variable's type.
/// At the top level (or after a `global` declaration) the binding targets
/// a module global; otherwise it creates/updates a function local.
fn bind_name(
    name: &str,
    name_span: Span,
    annotation: Option<ir::Ty>,
    value_ir: ir::Expr,
    value_span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Stmt> {
    if ctx.funcs().contains_key(name) {
        return Err(err(
            format!("'{name}' is a function and cannot be assigned to"),
            name_span,
        ));
    }

    let is_global = ctx.binds_global(name);
    let existing = if is_global {
        ctx.globals.get(name).copied()
    } else {
        ctx.locals.get(name).copied()
    };

    let target_ty = match (annotation, existing) {
        (Some(ann_ty), existing) => {
            if ann_ty == ir::Ty::None {
                return Err(err("cannot declare a variable of type None", name_span));
            }
            if let Some(existing) = existing
                && existing != ann_ty
            {
                return Err(err(
                    format!(
                        "variable '{name}' already has type {existing}; \
                         it cannot be re-declared as {ann_ty}"
                    ),
                    name_span,
                ));
            }
            ann_ty
        }
        (None, Some(existing)) => existing,
        (None, None) => match value_ir.ty {
            ir::Ty::None => {
                return Err(err(
                    format!(
                        "cannot assign to '{name}': the expression has no value \
                         (returns None)"
                    ),
                    value_span,
                ));
            }
            ty => ty,
        },
    };

    let value_expr = coerce_assign(value_ir, target_ty, name, value_span)?;

    if is_global {
        if !ctx.globals.contains_key(name) {
            ctx.globals.insert(name.to_string(), target_ty);
            ctx.globals_order.push((ctx.own_global(name), target_ty));
        }
        Ok(ir::Stmt::GlobalAssign {
            name: ctx.own_global(name),
            value: value_expr,
        })
    } else {
        if !ctx.locals.contains_key(name) {
            ctx.locals.insert(name.to_string(), target_ty);
            ctx.locals_order.push((name.to_string(), target_ty));
        }
        Ok(ir::Stmt::Assign {
            name: name.to_string(),
            value: value_expr,
        })
    }
}

fn lower_aug_assign(
    target: &ast::AssignTarget,
    op: ast::BinOp,
    value: &ast::Expr,
    span: Span,
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    match target {
        // desugar `x op= v` into `x = x op v`
        ast::AssignTarget::Name {
            name,
            span: name_span,
        } => {
            let (current_ty, is_global) = if let Some(&t) = ctx.locals.get(name) {
                (t, false)
            } else if ctx.binds_global(name) {
                match ctx.globals.get(name) {
                    Some(&t) => (t, true),
                    Option::None => {
                        return Err(err(format!("name '{name}' is not defined"), *name_span));
                    }
                }
            } else if ctx.globals.contains_key(name) {
                // Python raises UnboundLocalError at runtime; catch it here
                return Err(err(
                    format!(
                        "cannot modify global '{name}' here; add 'global {name}' \
                         at the top of the function"
                    ),
                    *name_span,
                ));
            } else {
                return Err(err(format!("name '{name}' is not defined"), *name_span));
            };
            let left = ir::Expr {
                ty: current_ty,
                kind: if is_global {
                    ir::ExprKind::GlobalLoad(ctx.own_global(name))
                } else {
                    ir::ExprKind::Local(name.clone())
                },
            };
            let right = lower_expr(value, ctx)?;
            let combined = lower_binary(op, left, right, span)?;
            let combined = coerce_assign(combined, current_ty, name, span)?;
            out.push(if is_global {
                ir::Stmt::GlobalAssign {
                    name: ctx.own_global(name),
                    value: combined,
                }
            } else {
                ir::Stmt::Assign {
                    name: name.clone(),
                    value: combined,
                }
            });
            Ok(())
        }
        // `xs[i] op= v`: evaluate base and index once via temps
        ast::AssignTarget::Index { base, index } => {
            let (list_ir, elem, index_ir) = lower_index_target(base, index, ctx)?;
            let list_ty = list_ir.ty;
            let base_t = ctx.fresh_temp("aug.base", list_ty);
            let idx_t = ctx.fresh_temp("aug.idx", ir::Ty::Int);
            out.push(ir::Stmt::Assign {
                name: base_t.clone(),
                value: list_ir,
            });
            out.push(ir::Stmt::Assign {
                name: idx_t.clone(),
                value: index_ir,
            });
            let base_local = ir::Expr {
                ty: list_ty,
                kind: ir::ExprKind::Local(base_t),
            };
            let idx_local = ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(idx_t),
            };
            let current = ir::Expr {
                ty: elem,
                kind: ir::ExprKind::Index {
                    base: Box::new(base_local.clone()),
                    index: Box::new(idx_local.clone()),
                },
            };
            let right = lower_expr(value, ctx)?;
            let combined = lower_binary(op, current, right, span)?;
            let combined =
                coerce(combined, elem, span, "list element assignment").map_err(|e| {
                    Diagnostic::new(
                        Phase::Semantic,
                        format!("{}; a list element's type cannot change", e.message),
                        e.span,
                    )
                })?;
            out.push(ir::Stmt::IndexAssign {
                base: base_local,
                index: idx_local,
                value: combined,
            });
            Ok(())
        }
    }
}

// ---- for loops ----

fn lower_for(
    var: &str,
    var_span: Span,
    iter: &ast::Expr,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    // `for i in range(...)` — lazy, no list is materialized
    if let ast::ExprKind::Call { func, args, .. } = &iter.kind
        && func == "range"
        && !ctx.funcs().contains_key("range")
    {
        return lower_for_range(var, var_span, args, iter.span, body, orelse, ctx, out);
    }

    // general case: list/string by index, or file via readline until ""
    let seq = lower_expr(iter, ctx)?;
    match seq.ty {
        ir::Ty::File => lower_for_file(var, var_span, seq, body, orelse, ctx, out),
        ir::Ty::List(_) | ir::Ty::Str => {
            lower_for_indexed(var, var_span, seq, body, orelse, ctx, out)
        }
        other => Err(err(format!("'{other}' object is not iterable"), iter.span)),
    }
}

/// Emit while + optional else (else runs only if no break).
fn push_loop_with_else(
    cond: ir::Expr,
    body: Vec<ir::Stmt>,
    step: Vec<ir::Stmt>,
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    if orelse.is_empty() {
        out.push(ir::Stmt::While { cond, body, step });
        return Ok(());
    }
    let broke = ctx.fresh_temp("broke", ir::Ty::Bool);
    out.push(ir::Stmt::Assign {
        name: broke.clone(),
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(false),
        },
    });
    let body = rewrite_breaks_set_flag(body, &broke);
    out.push(ir::Stmt::While { cond, body, step });
    let not_broke = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Unary {
            op: ir::UnOp::Not,
            operand: Box::new(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Local(broke),
            }),
        },
    };
    let else_body = lower_block(orelse, ctx)?;
    out.push(ir::Stmt::If {
        branches: vec![(not_broke, else_body)],
        orelse: vec![],
    });
    Ok(())
}

fn rewrite_breaks_set_flag(stmts: Vec<ir::Stmt>, broke: &str) -> Vec<ir::Stmt> {
    let mut out = Vec::with_capacity(stmts.len());
    for s in stmts {
        match s {
            ir::Stmt::Break => {
                out.push(ir::Stmt::Assign {
                    name: broke.to_string(),
                    value: ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::ConstBool(true),
                    },
                });
                out.push(ir::Stmt::Break);
            }
            ir::Stmt::If { branches, orelse } => {
                out.push(ir::Stmt::If {
                    branches: branches
                        .into_iter()
                        .map(|(c, b)| (c, rewrite_breaks_set_flag(b, broke)))
                        .collect(),
                    orelse: rewrite_breaks_set_flag(orelse, broke),
                });
            }
            other => out.push(other),
        }
    }
    out
}

/// `for line in f:` — while more: line = readline; if not line: more=False else: body
fn lower_for_file(
    var: &str,
    var_span: Span,
    file: ir::Expr,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let file_t = ctx.fresh_temp("for.file", ir::Ty::File);
    out.push(ir::Stmt::Assign {
        name: file_t.clone(),
        value: file,
    });
    let file_local = ir::Expr {
        ty: ir::Ty::File,
        kind: ir::ExprKind::Local(file_t),
    };

    // avoid Break for EOF so for-else still runs on clean exhaustion
    let more_t = ctx.fresh_temp("for.more", ir::Ty::Bool);
    out.push(ir::Stmt::Assign {
        name: more_t.clone(),
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(true),
        },
    });
    let more_local = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Local(more_t.clone()),
    };

    let line = ir::Expr {
        ty: ir::Ty::Str,
        kind: ir::ExprKind::FileCall {
            func: ir::FileFn::ReadLine,
            args: vec![file_local],
        },
    };
    let bind = bind_name(var, var_span, None, line, var_span, ctx)?;

    let line_local = if let ir::Stmt::Assign { name, .. } = &bind {
        ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::Local(name.clone()),
        }
    } else if let ir::Stmt::GlobalAssign { name, .. } = &bind {
        ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::GlobalLoad(name.clone()),
        }
    } else {
        return Err(err(
            "internal error: for-file loop variable binding",
            var_span,
        ));
    };
    let truthy = to_bool(line_local, var_span)?;
    let not_line = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Unary {
            op: ir::UnOp::Not,
            operand: Box::new(truthy.clone()),
        },
    };

    ctx.loop_depth += 1;
    let user_body = lower_block(body, ctx)?;
    ctx.loop_depth -= 1;

    let stop = ir::Stmt::Assign {
        name: more_t,
        value: ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(false),
        },
    };
    let loop_body = vec![
        bind,
        ir::Stmt::If {
            branches: vec![(not_line, vec![stop]), (truthy, user_body)],
            orelse: vec![],
        },
    ];

    push_loop_with_else(more_local, loop_body, vec![], orelse, ctx, out)
}

/// `for x in xs` / `for c in s` — index from 0 to len (re-read each iteration).
fn lower_for_indexed(
    var: &str,
    var_span: Span,
    seq: ir::Expr,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    let elem_ty = match seq.ty {
        ir::Ty::List(e) => *e,
        ir::Ty::Str => ir::Ty::Str,
        other => {
            return Err(err(
                format!("internal error: lower_for_indexed on {other}"),
                var_span,
            ));
        }
    };

    let seq_ty = seq.ty;
    let seq_t = ctx.fresh_temp("for.seq", seq_ty);
    let idx_t = ctx.fresh_temp("for.idx", ir::Ty::Int);
    out.push(ir::Stmt::Assign {
        name: seq_t.clone(),
        value: seq,
    });
    out.push(ir::Stmt::Assign {
        name: idx_t.clone(),
        value: int_const(0),
    });

    let seq_local = ir::Expr {
        ty: seq_ty,
        kind: ir::ExprKind::Local(seq_t),
    };
    let idx_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(idx_t.clone()),
    };

    // cond: idx < len(seq) — length is re-read every iteration, so
    // appending inside the loop extends it (like Python)
    let cond = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::Lt,
            left: Box::new(idx_local.clone()),
            right: Box::new(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(seq_local.clone())),
            }),
        },
    };

    // var = seq[idx] as the first statement of the body
    let element = ir::Expr {
        ty: elem_ty,
        kind: ir::ExprKind::Index {
            base: Box::new(seq_local),
            index: Box::new(idx_local.clone()),
        },
    };
    let bind = bind_name(var, var_span, None, element, var_span, ctx)?;

    ctx.loop_depth += 1;
    let user_body = lower_block(body, ctx);
    ctx.loop_depth -= 1;
    let mut loop_body = vec![bind];
    loop_body.extend(user_body?);

    let step = vec![ir::Stmt::Assign {
        name: idx_t,
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(idx_local),
                right: Box::new(int_const(1)),
            },
        },
    }];

    push_loop_with_else(cond, loop_body, step, orelse, ctx, out)
}

fn lower_for_range(
    var: &str,
    var_span: Span,
    args: &[ast::Expr],
    range_span: Span,
    body: &[ast::Stmt],
    orelse: &[ast::Stmt],
    ctx: &mut FnCtx,
    out: &mut Vec<ir::Stmt>,
) -> SResult<()> {
    if args.is_empty() || args.len() > 3 {
        return Err(err(
            format!("range() takes 1 to 3 arguments ({} given)", args.len()),
            range_span,
        ));
    }

    let mut lowered: Vec<ir::Expr> = Vec::new();
    for a in args {
        let v = lower_expr(a, ctx)?;
        lowered.push(coerce(v, ir::Ty::Int, a.span, "range() argument")?);
    }
    let (start, stop, step) = match lowered.len() {
        1 => (int_const(0), lowered.remove(0), int_const(1)),
        2 => {
            let stop = lowered.remove(1);
            (lowered.remove(0), stop, int_const(1))
        }
        _ => {
            let step = lowered.remove(2);
            let stop = lowered.remove(1);
            (lowered.remove(0), stop, step)
        }
    };

    // the loop variable must be an int (wherever it is bound)
    let existing_var_ty = if ctx.binds_global(var) {
        ctx.globals.get(var).copied()
    } else {
        ctx.locals.get(var).copied()
    };
    if let Some(existing) = existing_var_ty
        && existing != ir::Ty::Int
    {
        return Err(err(
            format!(
                "loop variable '{var}' already has type {existing}, but \
                 range() yields int"
            ),
            var_span,
        ));
    }

    // Python semantics: iterate a hidden counter and assign the user
    // variable at the top of each iteration. After exhaustion the variable
    // holds the last *yielded* value (not one past), an empty range never
    // assigns it, and mutating it inside the body cannot derail the loop.
    let stop_t = ctx.fresh_temp("range.stop", ir::Ty::Int);
    out.push(ir::Stmt::Assign {
        name: stop_t.clone(),
        value: stop,
    });
    let stop_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(stop_t),
    };
    let it_t = ctx.fresh_temp("range.it", ir::Ty::Int);
    out.push(ir::Stmt::Assign {
        name: it_t.clone(),
        value: start,
    });
    let it_local = ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::Local(it_t.clone()),
    };

    // constant steps get a simple condition; dynamic steps need a zero
    // check and a direction-aware condition
    let (cond, step_value) = match step.kind {
        ir::ExprKind::ConstInt(0) => {
            return Err(err("range() arg 3 must not be zero", range_span));
        }
        ir::ExprKind::ConstInt(k) => {
            let op = if k > 0 { ir::BinOp::Lt } else { ir::BinOp::Gt };
            let cond = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op,
                    left: Box::new(it_local.clone()),
                    right: Box::new(stop_local),
                },
            };
            (cond, int_const(k))
        }
        _ => {
            let step_t = ctx.fresh_temp("range.step", ir::Ty::Int);
            out.push(ir::Stmt::Assign {
                name: step_t.clone(),
                value: step,
            });
            let step_local = ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Local(step_t),
            };
            out.push(ir::Stmt::If {
                branches: vec![(
                    int_cmp(ir::BinOp::Eq, step_local.clone(), int_const(0)),
                    vec![ir::Stmt::Die(
                        "ValueError: range() arg 3 must not be zero".to_string(),
                    )],
                )],
                orelse: vec![],
            });
            // (step > 0 and it < stop) or (step < 0 and it > stop)
            let up = bool_and(
                int_cmp(ir::BinOp::Gt, step_local.clone(), int_const(0)),
                int_cmp(ir::BinOp::Lt, it_local.clone(), stop_local.clone()),
            );
            let down = bool_and(
                int_cmp(ir::BinOp::Lt, step_local.clone(), int_const(0)),
                int_cmp(ir::BinOp::Gt, it_local.clone(), stop_local),
            );
            let cond = ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Or,
                    left: Box::new(up),
                    right: Box::new(down),
                },
            };
            (cond, step_local)
        }
    };

    // var = .it as the first statement of the body
    let bind = bind_name(var, var_span, None, it_local.clone(), var_span, ctx)?;

    ctx.loop_depth += 1;
    let user_body = lower_block(body, ctx);
    ctx.loop_depth -= 1;
    let mut loop_body = vec![bind];
    loop_body.extend(user_body?);

    let step_stmt = ir::Stmt::Assign {
        name: it_t,
        value: ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(it_local),
                right: Box::new(step_value),
            },
        },
    };

    push_loop_with_else(cond, loop_body, vec![step_stmt], orelse, ctx, out)
}

fn int_const(v: i64) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Int,
        kind: ir::ExprKind::ConstInt(v),
    }
}

fn int_cmp(op: ir::BinOp, l: ir::Expr, r: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op,
            left: Box::new(l),
            right: Box::new(r),
        },
    }
}

fn bool_and(l: ir::Expr, r: ir::Expr) -> ir::Expr {
    ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Binary {
            op: ir::BinOp::And,
            left: Box::new(l),
            right: Box::new(r),
        },
    }
}

/// Coerce `value` for assignment into a variable of type `target`.
fn coerce_assign(value: ir::Expr, target: ir::Ty, name: &str, span: Span) -> SResult<ir::Expr> {
    coerce(value, target, span, &format!("assignment to '{name}'")).map_err(|e| {
        Diagnostic::new(
            Phase::Semantic,
            format!(
                "{}; a variable's type is fixed by its first assignment",
                e.message
            ),
            e.span,
        )
    })
}

/// Insert implicit promotion casts (`bool → int → float`) or fail.
fn coerce(value: ir::Expr, target: ir::Ty, span: Span, what: &str) -> SResult<ir::Expr> {
    if value.ty == target {
        return Ok(value);
    }
    match (value.ty, target) {
        (ir::Ty::Bool, ir::Ty::Int) => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(value)),
        }),
        (ir::Ty::Int, ir::Ty::Float) => Ok(ir::Expr {
            ty: ir::Ty::Float,
            kind: ir::ExprKind::IntToFloat(Box::new(value)),
        }),
        (ir::Ty::Bool, ir::Ty::Float) => {
            let as_int = ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(value)),
            };
            Ok(ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(as_int)),
            })
        }
        (found, expected) => Err(err(
            format!("type mismatch in {what}: expected {expected}, found {found}"),
            span,
        )),
    }
}

/// Lower an expression used as a condition; applies truthiness.
fn lower_condition(cond: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    let lowered = lower_expr(cond, ctx)?;
    to_bool(lowered, cond.span)
}

fn to_bool(value: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Bool => Ok(value),
        ir::Ty::Int | ir::Ty::Float | ir::Ty::Str | ir::Ty::List(_) => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ToBool(Box::new(value)),
        }),
        other => Err(err(
            format!("a value of type {other} cannot be used as a condition"),
            span,
        )),
    }
}

fn lower_expr(expr: &ast::Expr, ctx: &mut FnCtx) -> SResult<ir::Expr> {
    match &expr.kind {
        ast::ExprKind::Int(v) => Ok(int_const(*v)),
        ast::ExprKind::Float(v) => Ok(ir::Expr {
            ty: ir::Ty::Float,
            kind: ir::ExprKind::ConstFloat(*v),
        }),
        ast::ExprKind::Bool(v) => Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::ConstBool(*v),
        }),
        ast::ExprKind::Str(s) => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::ConstStr(s.clone()),
        }),
        ast::ExprKind::NoneLit => Err(err(
            "'None' cannot be used in an expression here",
            expr.span,
        )),
        ast::ExprKind::Name(name) => {
            if let Some((_, storage, ty)) = ctx
                .comp_renames
                .iter()
                .rev()
                .find(|(user, _, _)| user == name)
            {
                return Ok(ir::Expr {
                    ty: *ty,
                    kind: ir::ExprKind::Local(storage.clone()),
                });
            }
            if let Some(ty) = ctx.locals.get(name) {
                Ok(ir::Expr {
                    ty: *ty,
                    kind: ir::ExprKind::Local(name.clone()),
                })
            } else if let Some(ty) = ctx.globals.get(name) {
                // module globals are readable from any function
                Ok(ir::Expr {
                    ty: *ty,
                    kind: ir::ExprKind::GlobalLoad(ctx.own_global(name)),
                })
            } else if let Some(binding) = ctx.mctx.imports.get(name) {
                // a name brought in by `from other import ...`
                match binding {
                    ImportBinding::Symbol { module, name: real } => {
                        if let Some(ty) = ctx.mctx.mods[module].globals.get(real) {
                            Ok(ir::Expr {
                                ty: *ty,
                                kind: ir::ExprKind::GlobalLoad(qual(module, real)),
                            })
                        } else {
                            Err(err(
                                format!(
                                    "'{name}' is a function imported from '{module}'; \
                                     call it with parentheses: '{name}(...)'"
                                ),
                                expr.span,
                            ))
                        }
                    }
                    ImportBinding::Module(_) | ImportBinding::Sys => Err(err(
                        format!("module '{name}' is not a value; use '{name}.<name>'"),
                        expr.span,
                    )),
                }
            } else if ctx.funcs().contains_key(name) {
                Err(err(
                    format!("functions can only be called; add parentheses: '{name}(...)'"),
                    expr.span,
                ))
            } else {
                Err(err(format!("name '{name}' is not defined"), expr.span))
            }
        }
        ast::ExprKind::ListLit(items) => lower_list_lit(items, None, expr.span, ctx),
        ast::ExprKind::ListComp {
            elem,
            var,
            var_span,
            iter,
            cond,
        } => lower_list_comp(elem, var, *var_span, iter, cond.as_deref(), expr.span, ctx),
        ast::ExprKind::Index { base, index } => {
            let base_ir = lower_expr(base, ctx)?;
            let result_ty = match base_ir.ty {
                ir::Ty::List(e) => *e,
                ir::Ty::Str => ir::Ty::Str,
                other => {
                    return Err(err(
                        format!("'{other}' object is not subscriptable"),
                        base.span,
                    ));
                }
            };
            let index_ir = lower_expr(index, ctx)?;
            let index_ir = coerce(index_ir, ir::Ty::Int, index.span, "index")?;
            Ok(ir::Expr {
                ty: result_ty,
                kind: ir::ExprKind::Index {
                    base: Box::new(base_ir),
                    index: Box::new(index_ir),
                },
            })
        }
        ast::ExprKind::Attribute {
            base,
            attr,
            attr_span,
        } => {
            if let ast::ExprKind::Name(alias) = &base.kind {
                if ctx.sys_alias(alias) {
                    if attr == "argv" {
                        return Ok(ir::Expr {
                            ty: ir::list_of(ir::Ty::Str),
                            kind: ir::ExprKind::Argv,
                        });
                    }
                    return Err(err(
                        format!("'sys.{attr}' is not supported yet (only sys.argv)"),
                        *attr_span,
                    ));
                }
                // `module.global` from an imported module
                if let Some(real) = ctx.module_alias(alias) {
                    let data = &ctx.mctx.mods[&real];
                    if let Some(ty) = data.globals.get(attr) {
                        return Ok(ir::Expr {
                            ty: *ty,
                            kind: ir::ExprKind::GlobalLoad(qual(&real, attr)),
                        });
                    }
                    if data.funcs.contains_key(attr) {
                        return Err(err(
                            format!(
                                "'{real}.{attr}' is a function; call it: '{alias}.{attr}(...)'"
                            ),
                            *attr_span,
                        ));
                    }
                    return Err(err(
                        format!("module '{real}' has no attribute '{attr}'"),
                        *attr_span,
                    ));
                }
                if alias == "sys" {
                    return Err(err(
                        "name 'sys' is not defined; add 'import sys' at the top \
                         of the program",
                        base.span,
                    ));
                }
            }
            Err(err(
                "attribute access is only supported for 'sys.argv', imported \
                 module globals, and method calls",
                *attr_span,
            ))
        }
        ast::ExprKind::MethodCall {
            base,
            method,
            method_span,
            args,
            keywords,
        } => {
            // `module.func(args)` — a cross-module call, resolved before we
            // try to treat `base` as a value
            if let ast::ExprKind::Name(alias) = &base.kind
                && let Some(real) = ctx.module_alias(alias)
            {
                return lower_module_call(&real, method, *method_span, args, keywords, ctx);
            }
            if !keywords.is_empty() {
                return Err(err(
                    "keyword arguments are not supported for this method call",
                    keywords[0].name_span,
                ));
            }
            let base_ir = lower_expr(base, ctx)?;
            match base_ir.ty {
                ir::Ty::List(elem) => match method.as_str() {
                    // pop returns the removed element
                    "pop" => lower_list_pop(base_ir, *elem, args, *method_span, ctx),
                    "index" => lower_list_index_of(base_ir, *elem, args, *method_span, ctx),
                    "append" | "insert" | "remove" | "clear" | "sort" => Err(err(
                        format!(
                            "list.{method}(...) returns None and cannot be used \
                             in an expression"
                        ),
                        *method_span,
                    )),
                    _ => Err(err(
                        format!("'{}' has no method '{method}'", base_ir.ty),
                        *method_span,
                    )),
                },
                ir::Ty::Str => lower_str_method(base_ir, method, *method_span, args, ctx),
                ir::Ty::File => {
                    if method == "close" {
                        return Err(err(
                            "file.close() returns None and cannot be used in \
                             an expression",
                            *method_span,
                        ));
                    }
                    lower_file_method(base_ir, method, *method_span, args, ctx)
                }
                other => Err(err(
                    format!("'{other}' has no method '{method}'"),
                    *method_span,
                )),
            }
        }
        ast::ExprKind::Slice { base, lo, hi, step } => {
            let base_ir = lower_expr(base, ctx)?;
            let ty = match base_ir.ty {
                ir::Ty::Str => ir::Ty::Str,
                ir::Ty::List(e) => ir::Ty::List(e),
                other => {
                    return Err(err(format!("'{other}' object cannot be sliced"), base.span));
                }
            };
            // missing bounds are i64::MIN sentinels: their meaning depends
            // on the step's sign, resolved by the runtime like CPython
            let lo_ir = match lo {
                Some(e) => {
                    let v = lower_expr(e, ctx)?;
                    coerce(v, ir::Ty::Int, e.span, "slice bound")?
                }
                Option::None => int_const(i64::MIN),
            };
            let hi_ir = match hi {
                Some(e) => {
                    let v = lower_expr(e, ctx)?;
                    coerce(v, ir::Ty::Int, e.span, "slice bound")?
                }
                Option::None => int_const(i64::MIN),
            };
            let step_ir = match step {
                Some(e) => {
                    let v = lower_expr(e, ctx)?;
                    let v = coerce(v, ir::Ty::Int, e.span, "slice step")?;
                    if matches!(v.kind, ir::ExprKind::ConstInt(0)) {
                        return Err(err("slice step cannot be zero", e.span));
                    }
                    v
                }
                Option::None => int_const(1),
            };
            Ok(ir::Expr {
                ty,
                kind: ir::ExprKind::Slice {
                    base: Box::new(base_ir),
                    lo: Box::new(lo_ir),
                    hi: Box::new(hi_ir),
                    step: Box::new(step_ir),
                },
            })
        }
        ast::ExprKind::JoinedStr(parts) => {
            let mut result: Option<ir::Expr> = Option::None;
            for part in parts {
                let piece = match part {
                    ast::FStringPart::Literal(s) => ir::Expr {
                        ty: ir::Ty::Str,
                        kind: ir::ExprKind::ConstStr(s.clone()),
                    },
                    ast::FStringPart::Expr(e) => {
                        let v = lower_expr(e, ctx)?;
                        // reuse str() conversion rules
                        lower_cast(ast::TypeName::Str, v, e.span)?
                    }
                };
                result = Some(match result {
                    Option::None => piece,
                    Some(acc) => ir::Expr {
                        ty: ir::Ty::Str,
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Add,
                            left: Box::new(acc),
                            right: Box::new(piece),
                        },
                    },
                });
            }
            Ok(result.unwrap_or(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::ConstStr(String::new()),
            }))
        }
        ast::ExprKind::Call {
            func,
            func_span,
            args,
            keywords,
        } => lower_call(func, *func_span, args, keywords, expr.span, ctx),
        ast::ExprKind::Cast { ty, arg } => {
            let value = lower_expr(arg, ctx)?;
            lower_cast(*ty, value, arg.span)
        }
        ast::ExprKind::Unary { op, operand } => {
            let value = lower_expr(operand, ctx)?;
            match op {
                ast::UnaryOp::Not => {
                    let value = to_bool(value, operand.span)?;
                    Ok(ir::Expr {
                        ty: ir::Ty::Bool,
                        kind: ir::ExprKind::Unary {
                            op: ir::UnOp::Not,
                            operand: Box::new(value),
                        },
                    })
                }
                ast::UnaryOp::Neg => {
                    let value = promote_numeric(value, operand.span, "unary '-'")?;
                    // fold negated literals into constants so negative range
                    // steps and exponents are statically visible
                    match value.kind {
                        ir::ExprKind::ConstInt(v) => return Ok(int_const(v.wrapping_neg())),
                        ir::ExprKind::ConstFloat(v) => {
                            return Ok(ir::Expr {
                                ty: ir::Ty::Float,
                                kind: ir::ExprKind::ConstFloat(-v),
                            });
                        }
                        _ => {}
                    }
                    let ty = value.ty;
                    Ok(ir::Expr {
                        ty,
                        kind: ir::ExprKind::Unary {
                            op: ir::UnOp::Neg,
                            operand: Box::new(value),
                        },
                    })
                }
            }
        }
        ast::ExprKind::Compare { first, rest } => {
            let first_ir = lower_expr(first, ctx)?;
            lower_compare_chain(first_ir, rest, expr.span, ctx)
        }
        ast::ExprKind::Binary { op, left, right } => {
            // and/or take truthiness operands and short-circuit
            if matches!(op, ast::BinOp::And | ast::BinOp::Or) {
                let l = lower_condition(left, ctx)?;
                let r = lower_condition(right, ctx)?;
                let ir_op = if *op == ast::BinOp::And {
                    ir::BinOp::And
                } else {
                    ir::BinOp::Or
                };
                return Ok(ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::Binary {
                        op: ir_op,
                        left: Box::new(l),
                        right: Box::new(r),
                    },
                });
            }
            let l = lower_expr(left, ctx)?;
            let r = lower_expr(right, ctx)?;
            lower_binary(*op, l, r, expr.span)
        }
    }
}

/// `a < b <= c`: each middle operand is bound to a temp (evaluated once)
/// and the chain becomes short-circuit `and`s, exactly like Python.
fn lower_compare_chain(
    prev: ir::Expr,
    rest: &[(ast::BinOp, ast::Expr)],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let (op, operand) = &rest[0];
    let cur = lower_expr(operand, ctx)?;

    if rest.len() == 1 {
        return lower_binary(*op, prev, cur, span);
    }

    let cur_ty = cur.ty;
    let temp = ctx.fresh_temp("cmp", cur_ty);
    let temp_local = ir::Expr {
        ty: cur_ty,
        kind: ir::ExprKind::Local(temp.clone()),
    };

    let head = lower_binary(*op, prev, temp_local.clone(), span)?;
    let tail = lower_compare_chain(temp_local, &rest[1..], span, ctx)?;

    Ok(ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Let {
            name: temp,
            value: Box::new(cur),
            body: Box::new(bool_and(head, tail)),
        },
    })
}

/// `[elem for var in iter if cond]` desugars to a loop building a list
/// inside an expression-level Block. The variable lives in a hidden
/// storage slot (Python 3: it shadows but does not leak). Fast path:
/// when the produced count is knowable (list/str sources always; range
/// with a constant step of 1), the result is allocated at full capacity
/// and appended without per-element capacity checks.
fn lower_list_comp(
    elem: &ast::Expr,
    var: &str,
    var_span: Span,
    iter: &ast::Expr,
    cond: Option<&ast::Expr>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let mut stmts: Vec<ir::Stmt> = Vec::new();

    // ---- source setup: yields (per-iteration element expr, loop parts) ----
    struct Loop {
        cond: ir::Expr,
        step: Vec<ir::Stmt>,
        element: ir::Expr,
        /// exact or upper-bound capacity when knowable
        cap: Option<ir::Expr>,
    }

    let looping: Loop = if let ast::ExprKind::Call { func, args, .. } = &iter.kind
        && func == "range"
        && !ctx.funcs().contains_key("range")
    {
        if args.is_empty() || args.len() > 3 {
            return Err(err(
                format!("range() takes 1 to 3 arguments ({} given)", args.len()),
                iter.span,
            ));
        }
        let mut lowered: Vec<ir::Expr> = Vec::new();
        for a in args {
            let v = lower_expr(a, ctx)?;
            lowered.push(coerce(v, ir::Ty::Int, a.span, "range() argument")?);
        }
        let (start, stop, step) = match lowered.len() {
            1 => (int_const(0), lowered.remove(0), int_const(1)),
            2 => {
                let stop = lowered.remove(1);
                (lowered.remove(0), stop, int_const(1))
            }
            _ => {
                let step = lowered.remove(2);
                let stop = lowered.remove(1);
                (lowered.remove(0), stop, step)
            }
        };
        let stop_t = ctx.fresh_temp("comp.stop", ir::Ty::Int);
        stmts.push(ir::Stmt::Assign {
            name: stop_t.clone(),
            value: stop,
        });
        let stop_local = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Local(stop_t),
        };
        let it_t = ctx.fresh_temp("comp.it", ir::Ty::Int);
        stmts.push(ir::Stmt::Assign {
            name: it_t.clone(),
            value: start,
        });
        let it_local = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Local(it_t.clone()),
        };

        let (loop_cond, step_value, cap) = match step.kind {
            ir::ExprKind::ConstInt(0) => {
                return Err(err("range() arg 3 must not be zero", iter.span));
            }
            ir::ExprKind::ConstInt(1) => {
                // presize: cap = max(0, stop - it)
                let cap_t = ctx.fresh_temp("comp.cap", ir::Ty::Int);
                stmts.push(ir::Stmt::Assign {
                    name: cap_t.clone(),
                    value: ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::Binary {
                            op: ir::BinOp::Sub,
                            left: Box::new(stop_local.clone()),
                            right: Box::new(it_local.clone()),
                        },
                    },
                });
                let cap_local = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Local(cap_t.clone()),
                };
                stmts.push(ir::Stmt::If {
                    branches: vec![(
                        int_cmp(ir::BinOp::Lt, cap_local.clone(), int_const(0)),
                        vec![ir::Stmt::Assign {
                            name: cap_t,
                            value: int_const(0),
                        }],
                    )],
                    orelse: vec![],
                });
                (
                    int_cmp(ir::BinOp::Lt, it_local.clone(), stop_local),
                    int_const(1),
                    Some(cap_local),
                )
            }
            ir::ExprKind::ConstInt(k) => {
                let op = if k > 0 { ir::BinOp::Lt } else { ir::BinOp::Gt };
                (
                    int_cmp(op, it_local.clone(), stop_local),
                    int_const(k),
                    None,
                )
            }
            _ => {
                let step_t = ctx.fresh_temp("comp.step", ir::Ty::Int);
                stmts.push(ir::Stmt::Assign {
                    name: step_t.clone(),
                    value: step,
                });
                let step_local = ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Local(step_t),
                };
                stmts.push(ir::Stmt::If {
                    branches: vec![(
                        int_cmp(ir::BinOp::Eq, step_local.clone(), int_const(0)),
                        vec![ir::Stmt::Die(
                            "ValueError: range() arg 3 must not be zero".to_string(),
                        )],
                    )],
                    orelse: vec![],
                });
                let up = bool_and(
                    int_cmp(ir::BinOp::Gt, step_local.clone(), int_const(0)),
                    int_cmp(ir::BinOp::Lt, it_local.clone(), stop_local.clone()),
                );
                let down = bool_and(
                    int_cmp(ir::BinOp::Lt, step_local.clone(), int_const(0)),
                    int_cmp(ir::BinOp::Gt, it_local.clone(), stop_local),
                );
                let cond = ir::Expr {
                    ty: ir::Ty::Bool,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Or,
                        left: Box::new(up),
                        right: Box::new(down),
                    },
                };
                (cond, step_local, None)
            }
        };
        let step_stmt = ir::Stmt::Assign {
            name: it_t,
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(it_local.clone()),
                    right: Box::new(step_value),
                },
            },
        };
        Loop {
            cond: loop_cond,
            step: vec![step_stmt],
            element: it_local,
            cap,
        }
    } else {
        // list or str: index loop, exact/upper-bound presize via len
        let seq = lower_expr(iter, ctx)?;
        let src_elem_ty = match seq.ty {
            ir::Ty::List(e) => *e,
            ir::Ty::Str => ir::Ty::Str,
            other => {
                return Err(err(format!("'{other}' object is not iterable"), iter.span));
            }
        };
        let seq_ty = seq.ty;
        let seq_t = ctx.fresh_temp("comp.seq", seq_ty);
        stmts.push(ir::Stmt::Assign {
            name: seq_t.clone(),
            value: seq,
        });
        let idx_t = ctx.fresh_temp("comp.idx", ir::Ty::Int);
        stmts.push(ir::Stmt::Assign {
            name: idx_t.clone(),
            value: int_const(0),
        });
        let seq_local = ir::Expr {
            ty: seq_ty,
            kind: ir::ExprKind::Local(seq_t),
        };
        let idx_local = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Local(idx_t.clone()),
        };
        let cond = int_cmp(
            ir::BinOp::Lt,
            idx_local.clone(),
            ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Len(Box::new(seq_local.clone())),
            },
        );
        let element = ir::Expr {
            ty: src_elem_ty,
            kind: ir::ExprKind::Index {
                base: Box::new(seq_local.clone()),
                index: Box::new(idx_local.clone()),
            },
        };
        let step_stmt = ir::Stmt::Assign {
            name: idx_t,
            value: ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(idx_local),
                    right: Box::new(int_const(1)),
                },
            },
        };
        // exact capacity without a filter, upper bound with one
        let cap = ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::Len(Box::new(seq_local)),
        };
        Loop {
            cond,
            step: vec![step_stmt],
            element,
            cap: Some(cap),
        }
    };

    // ---- hidden storage for the loop variable ----
    let src_elem_ty = looping.element.ty;
    ctx.temp_counter += 1;
    let storage = format!(".comp{}.{var}", ctx.temp_counter);
    ctx.locals_order.push((storage.clone(), src_elem_ty));

    ctx.comp_renames
        .push((var.to_string(), storage.clone(), src_elem_ty));
    let elem_ir = lower_expr(elem, ctx);
    let cond_ir = match cond {
        Some(c) => lower_condition(c, ctx).map(Some),
        Option::None => Ok(Option::None),
    };
    ctx.comp_renames.pop();
    let elem_ir = elem_ir?;
    let cond_ir = cond_ir?;

    let elem_ty = elem_of(elem_ir.ty, elem.span)?;
    let _ = var_span;

    // ---- result list ----
    let presized = looping.cap.is_some();
    let cap_expr = looping.cap.unwrap_or(int_const(4));
    let res_t = ctx.fresh_temp("comp.res", ir::list_of(elem_ty));
    stmts.push(ir::Stmt::Assign {
        name: res_t.clone(),
        value: ir::Expr {
            ty: ir::list_of(elem_ty),
            kind: ir::ExprKind::ListNew {
                cap: Box::new(cap_expr),
            },
        },
    });
    let res_local = ir::Expr {
        ty: ir::list_of(elem_ty),
        kind: ir::ExprKind::Local(res_t.clone()),
    };

    // ---- loop body: var = element; [if cond:] append(elem) ----
    let append = if presized {
        ir::Stmt::ListAppendUnchecked {
            list: res_local.clone(),
            value: elem_ir,
        }
    } else {
        ir::Stmt::ListAppend {
            list: res_local.clone(),
            value: elem_ir,
        }
    };
    let mut body = vec![ir::Stmt::Assign {
        name: storage,
        value: looping.element,
    }];
    match cond_ir {
        Some(c) => body.push(ir::Stmt::If {
            branches: vec![(c, vec![append])],
            orelse: vec![],
        }),
        Option::None => body.push(append),
    }

    stmts.push(ir::Stmt::While {
        cond: looping.cond,
        body,
        step: looping.step,
    });

    let _ = span;
    Ok(ir::Expr {
        ty: ir::list_of(elem_ty),
        kind: ir::ExprKind::Block {
            stmts,
            result: Box::new(ir::Expr {
                ty: ir::list_of(elem_ty),
                kind: ir::ExprKind::Local(res_t),
            }),
        },
    })
}

fn lower_list_lit(
    items: &[ast::Expr],
    expected: Option<ir::Ty>,
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    if items.is_empty() {
        let elem = expected.ok_or_else(|| {
            err(
                "cannot infer the element type of an empty list; annotate the \
                 variable, e.g. 'xs: list[int] = []'",
                span,
            )
        })?;
        return Ok(ir::Expr {
            ty: ir::list_of(elem),
            kind: ir::ExprKind::ListLit(vec![]),
        });
    }

    let mut lowered = Vec::new();
    for item in items {
        lowered.push((lower_expr(item, ctx)?, item.span));
    }

    let elem = match expected {
        Some(e) => e,
        None => {
            // numeric join: any float → float; bool widens to int if mixed
            let mut ty = lowered[0].0.ty;
            for (item, item_span) in &lowered[1..] {
                ty = join_elem_types(ty, item.ty).ok_or_else(|| {
                    err(
                        format!(
                            "list elements must share one type; found {} and {}",
                            ty, item.ty
                        ),
                        *item_span,
                    )
                })?;
            }
            elem_of(ty, span)?
        }
    };

    let mut coerced = Vec::new();
    for (item, item_span) in lowered {
        coerced.push(coerce(item, elem, item_span, "list element")?);
    }

    Ok(ir::Expr {
        ty: ir::list_of(elem),
        kind: ir::ExprKind::ListLit(coerced),
    })
}

fn join_elem_types(a: ir::Ty, b: ir::Ty) -> Option<ir::Ty> {
    match (a, b) {
        _ if a == b => Some(a),
        (ir::Ty::Float, ir::Ty::Int)
        | (ir::Ty::Int, ir::Ty::Float)
        | (ir::Ty::Float, ir::Ty::Bool)
        | (ir::Ty::Bool, ir::Ty::Float) => Some(ir::Ty::Float),
        (ir::Ty::Int, ir::Ty::Bool) | (ir::Ty::Bool, ir::Ty::Int) => Some(ir::Ty::Int),
        _ => Option::None,
    }
}

/// Type-check and lower a call to a function with a known signature.
fn lower_call_with_sig(
    display: &str,
    ir_name: String,
    sig: &FuncSig,
    args: &[ast::Expr],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let n = sig.params.len();
    if args.len() > n {
        return Err(err(
            format!(
                "function '{display}' takes {} argument(s) but {} were given",
                n,
                args.len()
            ),
            span,
        ));
    }

    let mut slots: Vec<Option<ir::Expr>> = (0..n).map(|_| None).collect();
    let mut filled = vec![false; n];

    for (i, arg) in args.iter().enumerate() {
        let expected = sig.params[i].ty;
        let a = if let (ast::ExprKind::ListLit(items), ir::Ty::List(elem)) = (&arg.kind, expected) {
            lower_list_lit(items, Some(*elem), arg.span, ctx)?
        } else {
            lower_expr(arg, ctx)?
        };
        let a = coerce(
            a,
            expected,
            arg.span,
            &format!("argument {} of '{display}'", i + 1),
        )?;
        slots[i] = Some(a);
        filled[i] = true;
    }

    for kw in keywords {
        let Some(idx) = sig.params.iter().position(|p| p.name == kw.name) else {
            return Err(err(
                format!(
                    "function '{display}' got an unexpected keyword argument '{name}'",
                    name = kw.name
                ),
                kw.name_span,
            ));
        };
        if filled[idx] {
            return Err(err(
                format!(
                    "function '{display}' got multiple values for argument '{name}'",
                    name = kw.name
                ),
                kw.name_span,
            ));
        }
        let expected = sig.params[idx].ty;
        let a = if let (ast::ExprKind::ListLit(items), ir::Ty::List(elem)) =
            (&kw.value.kind, expected)
        {
            lower_list_lit(items, Some(*elem), kw.value.span, ctx)?
        } else {
            lower_expr(&kw.value, ctx)?
        };
        let a = coerce(
            a,
            expected,
            kw.value.span,
            &format!("argument '{name}' of '{display}'", name = kw.name),
        )?;
        slots[idx] = Some(a);
        filled[idx] = true;
    }

    let mut lowered_args = Vec::with_capacity(n);
    for (i, p) in sig.params.iter().enumerate() {
        if let Some(a) = slots[i].take() {
            lowered_args.push(a);
            continue;
        }
        if let Some(def) = &p.default {
            let a = if let (ast::ExprKind::ListLit(items), ir::Ty::List(elem)) = (&def.kind, p.ty) {
                lower_list_lit(items, Some(*elem), def.span, ctx)?
            } else {
                lower_expr(def, ctx)?
            };
            let a = coerce(
                a,
                p.ty,
                def.span,
                &format!(
                    "default for parameter '{name}' of '{display}'",
                    name = p.name
                ),
            )?;
            lowered_args.push(a);
        } else {
            return Err(err(
                format!(
                    "function '{display}' missing required argument '{name}'",
                    name = p.name
                ),
                span,
            ));
        }
    }

    Ok(ir::Expr {
        ty: sig.ret,
        kind: ir::ExprKind::Call {
            func: ir_name,
            args: lowered_args,
        },
    })
}

/// `module.func(args)` — a call into another module.
fn lower_module_call(
    real: &str,
    method: &str,
    method_span: Span,
    args: &[ast::Expr],
    keywords: &[ast::Keyword],
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    let data = &ctx.mctx.mods[real];
    if let Some(sig) = data.funcs.get(method).cloned() {
        return lower_call_with_sig(
            method,
            qual(real, method),
            &sig,
            args,
            keywords,
            method_span,
            ctx,
        );
    }
    if data.globals.contains_key(method) {
        return Err(err(
            format!("'{real}.{method}' is a value, not a function"),
            method_span,
        ));
    }
    Err(err(
        format!("module '{real}' has no attribute '{method}'"),
        method_span,
    ))
}

fn lower_call(
    func: &str,
    func_span: Span,
    args: &[ast::Expr],
    keywords: &[ast::Keyword],
    span: Span,
    ctx: &mut FnCtx,
) -> SResult<ir::Expr> {
    // a function defined in this module
    if ctx.funcs().contains_key(func) {
        let sig = ctx.funcs().get(func).cloned().unwrap();
        return lower_call_with_sig(func, ctx.own_func(func), &sig, args, keywords, span, ctx);
    }
    // a function pulled in by `from other import func`
    if let Some(ImportBinding::Symbol { module, name }) = ctx.mctx.imports.get(func).cloned() {
        if let Some(sig) = ctx.mctx.mods[&module].funcs.get(&name).cloned() {
            return lower_call_with_sig(
                func,
                qual(&module, &name),
                &sig,
                args,
                keywords,
                span,
                ctx,
            );
        }
        return Err(err(
            format!("'{func}' is a value imported from '{module}', not a function"),
            func_span,
        ));
    }
    // a module alias used as if it were a function
    if ctx.module_alias(func).is_some() || ctx.sys_alias(func) {
        return Err(err(
            format!("'{func}' is a module, not a function"),
            func_span,
        ));
    }
    if let Some(kw) = keywords.first() {
        return Err(err(
            format!("'{func}()' does not take keyword arguments"),
            kw.name_span,
        ));
    }
    {
        match func {
            "print" => Err(err(
                "print(...) does not return a value and cannot be used \
                     in an expression",
                span,
            )),
            "len" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("len() takes exactly one argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(&args[0], ctx)?;
                if !matches!(arg.ty, ir::Ty::Str | ir::Ty::List(_)) {
                    return Err(err(
                        format!("object of type '{}' has no len()", arg.ty),
                        args[0].span,
                    ));
                }
                Ok(ir::Expr {
                    ty: ir::Ty::Int,
                    kind: ir::ExprKind::Len(Box::new(arg)),
                })
            }
            "abs" => {
                if args.len() != 1 {
                    return Err(err(
                        format!("abs() takes exactly one argument ({} given)", args.len()),
                        span,
                    ));
                }
                let arg = lower_expr(&args[0], ctx)?;
                // bool → int (abs(True) is 1); int/float keep their type
                let arg = match arg.ty {
                    ir::Ty::Bool => ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::BoolToInt(Box::new(arg)),
                    },
                    ir::Ty::Int | ir::Ty::Float => arg,
                    other => {
                        return Err(err(
                            format!("bad operand type for abs(): '{other}'"),
                            args[0].span,
                        ));
                    }
                };
                Ok(ir::Expr {
                    ty: arg.ty,
                    kind: ir::ExprKind::Abs(Box::new(arg)),
                })
            }
            "min" | "max" => {
                if args.len() != 2 {
                    return Err(err(
                        format!(
                            "{func}() takes exactly 2 arguments ({} given);                              iterable form is not supported yet",
                            args.len()
                        ),
                        span,
                    ));
                }
                let left = lower_expr(&args[0], ctx)?;
                let right = lower_expr(&args[1], ctx)?;
                let (left, right, ty) = unify_numeric(left, right, span, &format!("{func}()"))?;
                let kind = if func == "min" {
                    ir::ExprKind::Min {
                        left: Box::new(left),
                        right: Box::new(right),
                    }
                } else {
                    ir::ExprKind::Max {
                        left: Box::new(left),
                        right: Box::new(right),
                    }
                };
                Ok(ir::Expr { ty, kind })
            }
            "sum" => {
                if args.len() != 1 {
                    return Err(err(
                        format!(
                            "sum() takes exactly 1 argument ({} given); \
                             start= is not supported yet",
                            args.len()
                        ),
                        span,
                    ));
                }
                let arg = lower_expr(&args[0], ctx)?;
                let elem = match arg.ty {
                    ir::Ty::List(e) => *e,
                    other => {
                        return Err(err(
                            format!("sum() expects a list of numbers, found {other}"),
                            args[0].span,
                        ));
                    }
                };
                match elem {
                    ir::Ty::Int | ir::Ty::Float => Ok(ir::Expr {
                        ty: elem,
                        kind: ir::ExprKind::Sum(Box::new(arg)),
                    }),
                    other => Err(err(
                        format!(
                            "sum() is only supported for list[int] and list[float], \
                             found list[{other}]"
                        ),
                        args[0].span,
                    )),
                }
            }
            "sorted" => {
                if args.len() != 1 {
                    return Err(err(
                        format!(
                            "sorted() takes exactly 1 argument ({} given); \
                             key=/reverse= are not supported yet",
                            args.len()
                        ),
                        span,
                    ));
                }
                let arg = lower_expr(&args[0], ctx)?;
                let elem = match arg.ty {
                    ir::Ty::List(e) => *e,
                    other => {
                        return Err(err(
                            format!("sorted() expects a list, found {other}"),
                            args[0].span,
                        ));
                    }
                };
                ensure_sortable_list_elem(elem, args[0].span)?;
                // copy via `xs * 1`, sort the copy, yield it
                let ty = arg.ty;
                let tmp = ctx.fresh_temp("sorted", ty);
                let copy = ir::Expr {
                    ty,
                    kind: ir::ExprKind::Binary {
                        op: ir::BinOp::Mul,
                        left: Box::new(arg),
                        right: Box::new(int_const(1)),
                    },
                };
                let local = |name: String| ir::Expr {
                    ty,
                    kind: ir::ExprKind::Local(name),
                };
                Ok(ir::Expr {
                    ty,
                    kind: ir::ExprKind::Block {
                        stmts: vec![
                            ir::Stmt::Assign {
                                name: tmp.clone(),
                                value: copy,
                            },
                            ir::Stmt::ListSort {
                                list: local(tmp.clone()),
                            },
                        ],
                        result: Box::new(local(tmp)),
                    },
                })
            }
            "range" => Err(err(
                "range(...) is only supported as the iterable of a 'for' loop",
                span,
            )),
            "input" => {
                let prompt = match args {
                    [] => Option::None,
                    [p] => {
                        let v = lower_expr(p, ctx)?;
                        if v.ty != ir::Ty::Str {
                            return Err(err(
                                format!(
                                    "input() prompt must be a str, found {} \
                                     (wrap it in str(...))",
                                    v.ty
                                ),
                                p.span,
                            ));
                        }
                        Some(Box::new(v))
                    }
                    _ => {
                        return Err(err(
                            format!("input() takes at most one argument ({} given)", args.len()),
                            span,
                        ));
                    }
                };
                Ok(ir::Expr {
                    ty: ir::Ty::Str,
                    kind: ir::ExprKind::Input { prompt },
                })
            }
            "open" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(err(
                        format!("open() takes 1 or 2 arguments ({} given)", args.len()),
                        span,
                    ));
                }
                let path = lower_expr(&args[0], ctx)?;
                if path.ty != ir::Ty::Str {
                    return Err(err(
                        format!("open() path must be a str, found {}", path.ty),
                        args[0].span,
                    ));
                }
                let mode = match args.get(1) {
                    Some(m) => {
                        let v = lower_expr(m, ctx)?;
                        if v.ty != ir::Ty::Str {
                            return Err(err(
                                format!("open() mode must be a str, found {}", v.ty),
                                m.span,
                            ));
                        }
                        // constant modes are validated now, like Python would
                        // at runtime
                        if let ir::ExprKind::ConstStr(mode_s) = &v.kind
                            && !matches!(mode_s.as_str(), "r" | "w" | "a")
                        {
                            return Err(err(
                                format!(
                                    "invalid mode: '{mode_s}' (supported: 'r', \
                                     'w', 'a')"
                                ),
                                m.span,
                            ));
                        }
                        v
                    }
                    Option::None => ir::Expr {
                        ty: ir::Ty::Str,
                        kind: ir::ExprKind::ConstStr("r".to_string()),
                    },
                };
                Ok(ir::Expr {
                    ty: ir::Ty::File,
                    kind: ir::ExprKind::Open {
                        path: Box::new(path),
                        mode: Box::new(mode),
                    },
                })
            }
            _ => Err(err(format!("function '{func}' is not defined"), func_span)),
        }
    }
}

fn lower_cast(ty: ast::TypeName, value: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match ty {
        ast::TypeName::Int => match value.ty {
            ir::Ty::Int => Ok(value),
            ir::Ty::Float => Ok(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::FloatToInt(Box::new(value)),
            }),
            ir::Ty::Bool => Ok(ir::Expr {
                ty: ir::Ty::Int,
                kind: ir::ExprKind::BoolToInt(Box::new(value)),
            }),
            other => Err(err(format!("int() cannot convert {other}"), span)),
        },
        ast::TypeName::Float => match value.ty {
            ir::Ty::Float => Ok(value),
            ir::Ty::Int | ir::Ty::Bool => {
                let as_int = if value.ty == ir::Ty::Bool {
                    ir::Expr {
                        ty: ir::Ty::Int,
                        kind: ir::ExprKind::BoolToInt(Box::new(value)),
                    }
                } else {
                    value
                };
                Ok(ir::Expr {
                    ty: ir::Ty::Float,
                    kind: ir::ExprKind::IntToFloat(Box::new(as_int)),
                })
            }
            other => Err(err(format!("float() cannot convert {other}"), span)),
        },
        ast::TypeName::Bool => match value.ty {
            ir::Ty::Bool => Ok(value),
            ir::Ty::Int | ir::Ty::Float | ir::Ty::Str | ir::Ty::List(_) => Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::ToBool(Box::new(value)),
            }),
            other => Err(err(format!("bool() cannot convert {other}"), span)),
        },
        ast::TypeName::Str => match value.ty {
            ir::Ty::Str => Ok(value),
            ir::Ty::Int => Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::IntToStr(Box::new(value)),
            }),
            ir::Ty::Float => Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::FloatToStr(Box::new(value)),
            }),
            ir::Ty::Bool => Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::BoolToStr(Box::new(value)),
            }),
            other => Err(err(format!("str() cannot convert {other} yet"), span)),
        },
        ast::TypeName::List(_) => Err(err("list(...) conversions are not supported", span)),
        ast::TypeName::File => Err(err(
            "file() is not a conversion; use open(path) to open a file",
            span,
        )),
        ast::TypeName::None => Err(err("None is not a conversion", span)),
    }
}

/// bool → int; int/float pass through; anything else is an error.
fn promote_numeric(value: ir::Expr, span: Span, what: &str) -> SResult<ir::Expr> {
    match value.ty {
        ir::Ty::Int | ir::Ty::Float => Ok(value),
        ir::Ty::Bool => Ok(ir::Expr {
            ty: ir::Ty::Int,
            kind: ir::ExprKind::BoolToInt(Box::new(value)),
        }),
        other => Err(err(
            format!("{what} is not supported for values of type {other}"),
            span,
        )),
    }
}

/// Promote both operands to a common numeric type (int unless either side
/// is float).
fn unify_numeric(
    l: ir::Expr,
    r: ir::Expr,
    span: Span,
    what: &str,
) -> SResult<(ir::Expr, ir::Expr, ir::Ty)> {
    let l = promote_numeric(l, span, what)?;
    let r = promote_numeric(r, span, what)?;
    match (l.ty, r.ty) {
        (ir::Ty::Int, ir::Ty::Int) => Ok((l, r, ir::Ty::Int)),
        (ir::Ty::Float, ir::Ty::Float) => Ok((l, r, ir::Ty::Float)),
        (ir::Ty::Int, ir::Ty::Float) => {
            let l = ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(l)),
            };
            Ok((l, r, ir::Ty::Float))
        }
        (ir::Ty::Float, ir::Ty::Int) => {
            let r = ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::IntToFloat(Box::new(r)),
            };
            Ok((l, r, ir::Ty::Float))
        }
        _ => unreachable!("promote_numeric only returns int/float"),
    }
}

fn lower_binary(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    let describe = format!("operator '{op}'");

    // membership tests work on str and list; check before type dispatch
    if matches!(op, ast::BinOp::In | ast::BinOp::NotIn) {
        return lower_contains(op, l, r, span);
    }

    // ---- string operations ----
    if l.ty == ir::Ty::Str || r.ty == ir::Ty::Str {
        return lower_str_binary(op, l, r, span);
    }
    // ---- list + / * ----
    if matches!(l.ty, ir::Ty::List(_)) || matches!(r.ty, ir::Ty::List(_)) {
        return lower_list_binary(op, l, r, span);
    }

    match op {
        ast::BinOp::Add
        | ast::BinOp::Sub
        | ast::BinOp::Mul
        | ast::BinOp::FloorDiv
        | ast::BinOp::Mod => {
            let (l, r, ty) = unify_numeric(l, r, span, &describe)?;
            let ir_op = match op {
                ast::BinOp::Add => ir::BinOp::Add,
                ast::BinOp::Sub => ir::BinOp::Sub,
                ast::BinOp::Mul => ir::BinOp::Mul,
                ast::BinOp::FloorDiv => ir::BinOp::FloorDiv,
                ast::BinOp::Mod => ir::BinOp::Mod,
                _ => unreachable!(),
            };
            Ok(ir::Expr {
                ty,
                kind: ir::ExprKind::Binary {
                    op: ir_op,
                    left: Box::new(l),
                    right: Box::new(r),
                },
            })
        }
        // int ** int stays int, except a negative constant exponent which
        // is a float in Python (2 ** -1 == 0.5); dynamic negative exponents
        // trap at runtime. Floats use llvm.pow.
        ast::BinOp::Pow => {
            let (l, r, ty) = unify_numeric(l, r, span, &describe)?;
            let (l, r, ty) =
                if ty == ir::Ty::Int && matches!(r.kind, ir::ExprKind::ConstInt(k) if k < 0) {
                    let to_float = |e: ir::Expr| ir::Expr {
                        ty: ir::Ty::Float,
                        kind: ir::ExprKind::IntToFloat(Box::new(e)),
                    };
                    (to_float(l), to_float(r), ir::Ty::Float)
                } else {
                    (l, r, ty)
                };
            Ok(ir::Expr {
                ty,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Pow,
                    left: Box::new(l),
                    right: Box::new(r),
                },
            })
        }
        // true division always produces float (Python semantics)
        ast::BinOp::Div => {
            let (l, r, _) = unify_numeric(l, r, span, &describe)?;
            let to_float = |e: ir::Expr| {
                if e.ty == ir::Ty::Float {
                    e
                } else {
                    ir::Expr {
                        ty: ir::Ty::Float,
                        kind: ir::ExprKind::IntToFloat(Box::new(e)),
                    }
                }
            };
            Ok(ir::Expr {
                ty: ir::Ty::Float,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Div,
                    left: Box::new(to_float(l)),
                    right: Box::new(to_float(r)),
                },
            })
        }
        ast::BinOp::Eq
        | ast::BinOp::NotEq
        | ast::BinOp::Lt
        | ast::BinOp::LtEq
        | ast::BinOp::Gt
        | ast::BinOp::GtEq => {
            let (l, r, _) = unify_numeric(l, r, span, &describe)?;
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: comparison_ir_op(op),
                    left: Box::new(l),
                    right: Box::new(r),
                },
            })
        }
        ast::BinOp::And | ast::BinOp::Or => {
            unreachable!("and/or are handled in lower_expr")
        }
        ast::BinOp::In | ast::BinOp::NotIn => {
            unreachable!("in/not-in are handled above")
        }
    }
}

fn comparison_ir_op(op: ast::BinOp) -> ir::BinOp {
    match op {
        ast::BinOp::Eq => ir::BinOp::Eq,
        ast::BinOp::NotEq => ir::BinOp::Ne,
        ast::BinOp::Lt => ir::BinOp::Lt,
        ast::BinOp::LtEq => ir::BinOp::Le,
        ast::BinOp::Gt => ir::BinOp::Gt,
        ast::BinOp::GtEq => ir::BinOp::Ge,
        _ => unreachable!("not a comparison"),
    }
}

/// `needle in haystack` / `not in`: substring test or list membership.
fn lower_contains(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    let needle = match r.ty {
        ir::Ty::Str => {
            if l.ty != ir::Ty::Str {
                return Err(err(
                    format!("'in <str>' requires a str on the left, found {}", l.ty),
                    span,
                ));
            }
            l
        }
        ir::Ty::List(ir::Ty::List(_)) => {
            return Err(err("'in' is not supported for lists of lists yet", span));
        }
        ir::Ty::List(elem) => coerce(l, *elem, span, "'in' operand")?,
        other => {
            return Err(err(format!("'{other}' does not support 'in'"), span));
        }
    };
    let contains = ir::Expr {
        ty: ir::Ty::Bool,
        kind: ir::ExprKind::Contains {
            needle: Box::new(needle),
            haystack: Box::new(r),
        },
    };
    if op == ast::BinOp::NotIn {
        return Ok(ir::Expr {
            ty: ir::Ty::Bool,
            kind: ir::ExprKind::Unary {
                op: ir::UnOp::Not,
                operand: Box::new(contains),
            },
        });
    }
    Ok(contains)
}

fn lower_list_binary(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match op {
        // xs + ys — same element type required
        ast::BinOp::Add => match (l.ty, r.ty) {
            (ir::Ty::List(a), ir::Ty::List(b)) if a == b => Ok(ir::Expr {
                ty: ir::Ty::List(a),
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Add,
                    left: Box::new(l),
                    right: Box::new(r),
                },
            }),
            (ir::Ty::List(a), ir::Ty::List(b)) => Err(err(
                format!("cannot concatenate list[{a}] and list[{b}]"),
                span,
            )),
            _ => {
                let other = if matches!(l.ty, ir::Ty::List(_)) {
                    &r.ty
                } else {
                    &l.ty
                };
                Err(err(
                    format!("can only concatenate list (not \"{other}\") to list"),
                    span,
                ))
            }
        },
        // xs * n / n * xs — count normalized to the right
        ast::BinOp::Mul => {
            let (xs, n) = match (l.ty, r.ty) {
                (ir::Ty::List(_), _) => (l, r),
                (_, ir::Ty::List(_)) => (r, l),
                _ => unreachable!("lower_list_binary only when a side is list"),
            };
            let n = promote_numeric(n, span, "list repetition")?;
            if n.ty != ir::Ty::Int {
                return Err(err("a list can only be multiplied by an int", span));
            }
            Ok(ir::Expr {
                ty: xs.ty,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Mul,
                    left: Box::new(xs),
                    right: Box::new(n),
                },
            })
        }
        // element-wise equality (same list element type)
        ast::BinOp::Eq | ast::BinOp::NotEq => match (l.ty, r.ty) {
            (ir::Ty::List(a), ir::Ty::List(b)) if a == b => Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: if matches!(op, ast::BinOp::Eq) {
                        ir::BinOp::Eq
                    } else {
                        ir::BinOp::Ne
                    },
                    left: Box::new(l),
                    right: Box::new(r),
                },
            }),
            (ir::Ty::List(a), ir::Ty::List(b)) => {
                Err(err(format!("cannot compare list[{a}] and list[{b}]"), span))
            }
            _ => Err(err(
                format!("'{}' is not comparable with '{}'", l.ty, r.ty),
                span,
            )),
        },
        other => Err(err(
            format!("operator '{other}' is not supported for lists yet"),
            span,
        )),
    }
}

fn lower_str_binary(op: ast::BinOp, l: ir::Expr, r: ir::Expr, span: Span) -> SResult<ir::Expr> {
    match op {
        // "a" + "b"
        ast::BinOp::Add if l.ty == ir::Ty::Str && r.ty == ir::Ty::Str => Ok(ir::Expr {
            ty: ir::Ty::Str,
            kind: ir::ExprKind::Binary {
                op: ir::BinOp::Add,
                left: Box::new(l),
                right: Box::new(r),
            },
        }),
        ast::BinOp::Add => Err(err(
            "can only concatenate str to str; use str(...) to convert",
            span,
        )),
        // "ab" * 3 / 3 * "ab" — the count is normalized to the right
        ast::BinOp::Mul => {
            let (s, n) = if l.ty == ir::Ty::Str { (l, r) } else { (r, l) };
            let n = promote_numeric(n, span, "string repetition")?;
            if n.ty != ir::Ty::Int {
                return Err(err("a string can only be multiplied by an int", span));
            }
            Ok(ir::Expr {
                ty: ir::Ty::Str,
                kind: ir::ExprKind::Binary {
                    op: ir::BinOp::Mul,
                    left: Box::new(s),
                    right: Box::new(n),
                },
            })
        }
        // lexicographic comparisons
        ast::BinOp::Eq
        | ast::BinOp::NotEq
        | ast::BinOp::Lt
        | ast::BinOp::LtEq
        | ast::BinOp::Gt
        | ast::BinOp::GtEq => {
            if l.ty != ir::Ty::Str || r.ty != ir::Ty::Str {
                return Err(err(
                    format!("'{}' is not comparable with '{}'", l.ty, r.ty),
                    span,
                ));
            }
            Ok(ir::Expr {
                ty: ir::Ty::Bool,
                kind: ir::ExprKind::Binary {
                    op: comparison_ir_op(op),
                    left: Box::new(l),
                    right: Box::new(r),
                },
            })
        }
        other => Err(err(
            format!("operator '{other}' is not supported for str"),
            span,
        )),
    }
}

// ---- return-path analysis ----

fn block_returns(stmts: &[ir::Stmt]) -> bool {
    stmts.iter().any(stmt_returns)
}

fn stmt_returns(stmt: &ir::Stmt) -> bool {
    match stmt {
        ir::Stmt::Return(_) => true,
        // Die exits the process; the path cannot fall through
        ir::Stmt::Die(_) => true,
        ir::Stmt::If { branches, orelse } => {
            !orelse.is_empty()
                && branches.iter().all(|(_, body)| block_returns(body))
                && block_returns(orelse)
        }
        // `while True:` without a break never falls through
        ir::Stmt::While { cond, body, .. } => {
            matches!(cond.kind, ir::ExprKind::ConstBool(true)) && !loop_breaks(body)
        }
        _ => false,
    }
}

/// Does this loop body contain a `break` for *this* loop (not a nested one)?
fn loop_breaks(stmts: &[ir::Stmt]) -> bool {
    stmts.iter().any(|s| match s {
        ir::Stmt::Break => true,
        ir::Stmt::If { branches, orelse } => {
            branches.iter().any(|(_, b)| loop_breaks(b)) || loop_breaks(orelse)
        }
        // a break inside a nested while belongs to that while
        ir::Stmt::While { .. } => false,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(matches!(entry.body[0], ir::Stmt::Print(_)));
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
        let ir::ExprKind::Let { body, .. } = &value.kind else {
            panic!("expected Let, got {:?}", value.kind);
        };
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
        let e = analyze_err("x = min(1)\n");
        assert!(e.message.contains("exactly 2 arguments"), "{}", e.message);
    }

    #[test]
    fn sum_list_int_and_float() {
        let m = analyze_ok("a = sum([1, 2, 3])\nb = sum([1.5, 2.5])\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Int);
        assert!(matches!(value.kind, ir::ExprKind::Sum(_)));
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[1] else {
            panic!();
        };
        assert_eq!(value.ty, ir::Ty::Float);
        assert!(matches!(value.kind, ir::ExprKind::Sum(_)));
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
    fn list_literal_infers_type_and_promotes() {
        let m = analyze_ok("xs = [1, 2.5, True]\n");
        let entry = find_func(&m, ENTRY_NAME);
        let ir::Stmt::GlobalAssign { value, .. } = &entry.body[0] else {
            panic!();
        };
        assert_eq!(value.ty, ir::list_of(ir::Ty::Float));
    }

    #[test]
    fn empty_list_needs_annotation() {
        let e = analyze_err("xs = []\n");
        assert!(e.message.contains("annotate"), "{}", e.message);
        analyze_ok("xs: list[int] = []\n");
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
    fn for_else_without_break_emits_not_broke_if() {
        let m = analyze_ok(
            "\
for i in range(2):
    pass
else:
    print(1)
",
        );
        let entry = find_func(&m, ENTRY_NAME);
        // broke=False, While, If(not broke)
        assert!(
            entry
                .body
                .iter()
                .any(|s| matches!(s, ir::Stmt::While { .. })),
            "{:?}",
            entry.body
        );
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
            "expected if-not-broke for for-else, body={:?}",
            entry.body
        );
    }

    #[test]
    fn while_else_desugars_to_broke_flag() {
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
            entry.body.iter().any(|s| matches!(
                s,
                ir::Stmt::Assign {
                    value: ir::Expr {
                        kind: ir::ExprKind::ConstBool(false),
                        ..
                    },
                    ..
                }
            )),
            "expected broke=False init, body={:?}",
            entry.body
        );
        assert!(
            entry
                .body
                .iter()
                .any(|s| matches!(s, ir::Stmt::While { .. })),
            "{:?}",
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
            ir::Stmt::Print(args) if args.len() == 1
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
    fn heterogeneous_list_is_error() {
        let e = analyze_err("xs = [1, \"a\"]\n");
        assert!(e.message.contains("share one type"), "{}", e.message);
    }

    #[test]
    fn error_no_entry_point() {
        let e = analyze_err("def helper() -> int:\n    return 1\n");
        assert!(e.message.contains("entry point"), "{}", e.message);
    }

    #[test]
    fn error_variable_changes_type() {
        let e = analyze_err("x = 1\nx = 2.5\n");
        assert!(e.message.contains("fixed"), "{}", e.message);
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
    fn error_fstring_of_list() {
        let e = analyze_err("xs = [1]\ns = f\"{xs}\"\nprint(s)\n");
        assert!(e.message.contains("convert"), "{}", e.message);
    }
}
