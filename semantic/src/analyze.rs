//! Program entry points: `analyze`, `analyze_library`, `analyze_program`.

use std::collections::{HashMap, HashSet};

use common::Span;
use parser::ast;

use crate::prelude::*;

/// Analyze a single module (no file imports beyond `sys`). Used by tests
/// and by the driver for single-file programs.
pub fn analyze(module: &ast::Module) -> SResult<ir::Module> {
    analyze_program(&[ModuleInput {
        name: ENTRY_NAME.to_string(),
        ast: module,
    }])
}

/// Analyze a library without requiring or automatically calling `main()`.
/// Initialization IR is retained for an adapter to invoke explicitly.
pub fn analyze_library(module: &ast::Module) -> SResult<ir::Module> {
    analyze_target(
        &[ModuleInput {
            name: ENTRY_NAME.to_string(),
            ast: module,
        }],
        false,
    )
}

/// Analyze a whole program: several modules with cross-file imports.
/// `modules` is in topological order (dependencies first, root last);
/// diagnostics are tagged with the module index as their file id.
pub fn analyze_program(modules: &[ModuleInput]) -> SResult<ir::Module> {
    analyze_target(modules, true)
}

pub(crate) fn analyze_target(modules: &[ModuleInput], executable: bool) -> SResult<ir::Module> {
    assert!(!modules.is_empty(), "a program needs at least one module");
    clear_closure_defaults();
    clear_class_env();
    clear_class_consts();
    clear_synth_param_tys();
    SYNTH_YIELD_TYS.with(|m| m.borrow_mut().clear());
    // Before regular class collection: an exception class must not acquire a
    // ClassId, a layout or a vtable.
    collect_exception_classes(modules)?;
    let root_idx = modules.len() - 1;

    // pass 0: class ids → import aliases for bases/annotations → bases → layouts
    let mut class_asts = collect_class_asts(modules)?;
    register_class_ids(&class_asts)?;
    register_class_consts(&class_asts)?;
    inject_class_import_aliases(modules);
    resolve_class_bases(&class_asts)?;
    // Needs resolved bases: whether a class should get a default `__ne__`
    // depends on what its ancestors already provide.
    synthesize_default_ne_methods(&mut class_asts);
    let class_asts = class_asts;
    pre_register_no_return(modules);
    let free_func_rets = pre_infer_free_func_rets(modules);
    finalize_class_layouts(&class_asts, &free_func_rets)?;

    // pass 1: every module's own signatures and assignment-name surface
    let mut own_funcs: HashMap<String, HashMap<String, FuncSig>> = HashMap::new();
    let mut all_orders: Vec<Vec<&ast::FuncDef>> = Vec::new();
    // Method defs to lower per module: (ir_name, class_id, FuncDef)
    let mut method_orders: HashMap<String, Vec<(String, ir::ClassId, &ast::FuncDef)>> =
        HashMap::new();
    let mut assigned_names: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    // Classes defined per module (for exports / from-import).
    let mut module_classes: HashMap<String, HashMap<String, ir::ClassId>> = HashMap::new();
    let module_names: Vec<String> = modules.iter().map(|m| m.name.clone()).collect();
    let submodules = build_submodule_map(&module_names);
    for (i, m) in modules.iter().enumerate() {
        // Bare class names + import aliases available for annotations in collect_sigs.
        set_class_current_module(&m.name);
        let (mut funcs, order) = collect_sigs(m.ast).map_err(|d| d.with_file(i))?;
        // Class methods: register under IR names (`Point.__init__`) so
        // `lower_function` picks up the typed self parameter via known_sig.
        let mut methods = Vec::new();
        let mut classes_here: HashMap<String, ir::ClassId> = HashMap::new();
        for c in class_asts.iter().filter(|c| c.module == m.name) {
            let class_id = lookup_class_in_module(&c.module, &c.name).expect("class registered");
            classes_here.insert(c.name.clone(), class_id);
            for cm in &c.methods {
                let ir_name = method_ir_name(&c.module, c.is_root, &c.name, &cm.def.name);
                let kind = method_kind_from_decorators(&cm.def.decorators, cm.def.span)
                    .map_err(|d| d.with_file(i))?;
                let sig =
                    method_func_sig(class_id, &c.name, cm.def, kind).map_err(|d| d.with_file(i))?;
                register_method_sig(&ir_name, sig.clone());
                register_method_kind(&ir_name, kind);
                if kind == MethodKind::Property {
                    register_property(class_id, &cm.def.name, &ir_name);
                }
                methods.push((ir_name.clone(), class_id, cm.def));
                funcs.insert(ir_name, sig);
            }
        }
        module_classes.insert(m.name.clone(), classes_here);
        method_orders.insert(m.name.clone(), methods);
        own_funcs.insert(m.name.clone(), funcs);
        all_orders.push(order);
        let mut names = collect_assigned_names(m.ast);
        // Classes are exportable names (from m import C / m.C).
        for c in class_asts.iter().filter(|c| c.module == m.name) {
            names.insert(c.name.clone());
        }
        assigned_names.insert(m.name.clone(), names);
    }
    // Override ABI: reject incompatible ret/arity after all method sigs exist.
    check_override_compatibility(&class_asts)?;
    // pass 1b: export surface includes package re-exports; last-binding map
    let export_funcs = expand_export_funcs(modules, &own_funcs);
    let export_values = expand_export_values(modules, &assigned_names, &export_funcs, &submodules);
    let last_exports = compute_last_exports(modules, &submodules, &export_funcs, &export_values);
    let partial_prelim = build_partial_prelim(modules);
    let partial_funcs = build_partial_funcs(modules);
    let partial_reexports = build_partial_reexports(modules);
    let package_final_values = build_package_final_values(modules);

    // pass 2: import bindings (validated against the export surface)
    let asts = module_ast_map(modules);
    let mut all_imports: Vec<HashMap<String, ImportBinding>> = Vec::new();
    for (i, m) in modules.iter().enumerate() {
        let imports = collect_imports(
            m.ast,
            &m.name,
            &last_exports,
            &export_funcs,
            &export_values,
            &submodules,
            &asts,
        )
        .map_err(|d| d.with_file(i))?;
        all_imports.push(imports);
    }

    // Re-export origins for deferred parent attribute/call resolution.
    let reexport_origins = build_reexport_origins(modules, &all_imports, &last_exports);

    // pass 3: lower each module in dependency order, accumulating results
    let mut mods: HashMap<String, ModuleData> = HashMap::new();
    let mut out_funcs: Vec<ir::Function> = Vec::new();
    let mut out_globals: Vec<(String, ir::Ty)> = Vec::new();

    for (i, m) in modules.iter().enumerate() {
        let is_root = i == root_idx;
        let funcs = &own_funcs[&m.name];
        let mctx = ModuleCtx {
            module: &m.name,
            is_root,
            funcs,
            imports: &all_imports[i],
            mods: &mods,
            submodules: &submodules,
            partial_prelim: &partial_prelim,
            partial_funcs: &partial_funcs,
            package_final_values: &package_final_values,
            all_own_funcs: &own_funcs,
            last_exports: &last_exports,
            reexport_origins: &reexport_origins,
            partial_reexports: &partial_reexports,
        };

        let mut globals: HashMap<String, ir::Ty> = HashMap::new();
        let mut globals_order: Vec<(String, ir::Ty)> = Vec::new();
        let script: Vec<ast::Stmt> = m
            .ast
            .body
            .iter()
            .filter(|s| {
                !matches!(
                    s.kind,
                    ast::StmtKind::FuncDef(_) | ast::StmtKind::ClassDef(_)
                )
            })
            .cloned()
            .collect();

        // Pre-seed globals from simple top-level assigns so functions can
        // reference them before init runs (order: functions then init).
        set_seed_funcs(funcs, &mctx.prefix());
        seed_globals_from_script(&script, &mut globals, &mut globals_order, is_root, &m.name);
        clear_seed_funcs();

        // Lower top-level functions first so inferred return types (e.g.
        // returning a nested closure) are available when lowering the
        // module init / entry script that calls them.
        // Lower each top-level function, patching own_funcs immediately so
        // later functions see inferred return types / generator frame sizes.
        #[allow(clippy::drop_non_drop)]
        {
            drop(mctx);
        }
        // Lower class methods first (bodies may be called from free functions).
        set_class_current_module(&m.name);
        // Imported class aliases: bare name in this module resolves for annotations
        // and construction (`from shapes import Point` → `Point(...)`).
        for (local, binding) in &all_imports[i] {
            let id = match binding {
                ImportBinding::Class(id) => Some(*id),
                ImportBinding::Symbol { module, name } => lookup_class_in_module(module, name),
                _ => None,
            };
            if let Some(id) = id {
                with_class_env_mut(|e| {
                    e.by_key
                        .entry((m.name.clone(), local.clone()))
                        .or_insert(id);
                });
            }
        }
        if let Some(methods) = method_orders.get(&m.name) {
            for (ir_name, class_id, def) in methods {
                let mut method_def = (*def).clone();
                method_def.name = ir_name.clone();
                let funcs_view = &own_funcs[&m.name];
                let mctx_fn = ModuleCtx {
                    module: &m.name,
                    is_root,
                    funcs: funcs_view,
                    imports: &all_imports[i],
                    mods: &mods,
                    submodules: &submodules,
                    partial_prelim: &partial_prelim,
                    partial_funcs: &partial_funcs,
                    package_final_values: &package_final_values,
                    all_own_funcs: &own_funcs,
                    last_exports: &last_exports,
                    reexport_origins: &reexport_origins,
                    partial_reexports: &partial_reexports,
                };
                let (f, nested) = lower_function_with_class(
                    &method_def,
                    &mctx_fn,
                    &mut globals,
                    &mut globals_order,
                    false,
                    Some(*class_id),
                )
                .map_err(|d| d.with_file(i))?;
                // Patch inferred return type (same as free functions).
                if !f.is_generator {
                    if let Some(sig) = own_funcs.get_mut(&m.name).and_then(|m| m.get_mut(ir_name)) {
                        sig.ret = f.ret;
                    }
                    register_method_sig(ir_name, {
                        let mut s = method_sig_lookup(ir_name).unwrap_or_else(|| FuncSig {
                            // Rebuilt from an `ir::Function`, which carries no
                            // `/` or `*` markers; the registered sig above is
                            // the one that has them.
                            posonly_end: 0,
                            kwonly_start: Option::None,
                            params: f
                                .params
                                .iter()
                                .map(|(n, t)| ParamSig {
                                    name: n.clone(),
                                    ty: *t,
                                    default: None,
                                })
                                .collect(),
                            vararg: None,
                            kwarg: None,
                            ret: f.ret,
                            span: Span::default(),
                            is_generator: false,
                            yield_ty: None,
                            gen_frame_slots: 0,
                        });
                        s.ret = f.ret;
                        s
                    });
                }
                out_funcs.push(f);
                out_funcs.extend(nested);
            }
        }
        // Free functions with a single bare-name decorator (applied at init).
        let mut free_decorators: Vec<(String, String, Span)> = Vec::new();
        for fd in &all_orders[i] {
            let patch = {
                let funcs_view = &own_funcs[&m.name];
                let mctx_fn = ModuleCtx {
                    module: &m.name,
                    is_root,
                    funcs: funcs_view,
                    imports: &all_imports[i],
                    mods: &mods,
                    submodules: &submodules,
                    partial_prelim: &partial_prelim,
                    partial_funcs: &partial_funcs,
                    package_final_values: &package_final_values,
                    all_own_funcs: &own_funcs,
                    last_exports: &last_exports,
                    reexport_origins: &reexport_origins,
                    partial_reexports: &partial_reexports,
                };
                let (f, nested) =
                    lower_function(fd, &mctx_fn, &mut globals, &mut globals_order, false)
                        .map_err(|d| d.with_file(i))?;
                let name = fd.name.clone();
                if let Some(d) = fd.decorators.first() {
                    free_decorators.push((name.clone(), d.name.clone(), d.span));
                }
                let patch = if f.is_generator {
                    Err((f.params.len() + f.locals.len()) as i64)
                } else {
                    Ok(f.ret)
                };
                out_funcs.push(f);
                out_funcs.extend(nested);
                (name, patch)
            };
            let (name, patch) = patch;
            if let Some(sig) = own_funcs.get_mut(&m.name).and_then(|m| m.get_mut(&name)) {
                match patch {
                    Ok(ret) => sig.ret = ret,
                    Err(slots) => sig.gen_frame_slots = slots,
                }
            }
        }

        // the module's top-level statements become its init function; for
        // the root that IS the entry, otherwise `<mod>.__init__` guarded to
        // run once
        let init_name = if is_root {
            ENTRY_NAME.to_string()
        } else {
            qual(&m.name, "__init__")
        };

        let funcs = &own_funcs[&m.name];
        let mctx = ModuleCtx {
            module: &m.name,
            is_root,
            funcs,
            imports: &all_imports[i],
            mods: &mods,
            submodules: &submodules,
            partial_prelim: &partial_prelim,
            partial_funcs: &partial_funcs,
            package_final_values: &package_final_values,
            all_own_funcs: &own_funcs,
            last_exports: &last_exports,
            reexport_origins: &reexport_origins,
            partial_reexports: &partial_reexports,
        };

        // When free functions are decorated, ensure an init function exists even
        // if the script is otherwise empty (decoration runs at import/entry).
        // Pre-register Closure globals so Call sites during init lower see them.
        let need_init_for_deco = !free_decorators.is_empty();
        if need_init_for_deco {
            for (fname, dname, dspan) in &free_decorators {
                let deco_sig = funcs.get(dname.as_str()).cloned().ok_or_else(|| {
                    err(
                        format!(
                            "decorator '{dname}' is not a function in this module \
                             (only same-module free functions are supported)"
                        ),
                        *dspan,
                    )
                    .with_file(i)
                })?;
                let result_ty = deco_sig.ret;
                if !matches!(result_ty, ir::Ty::Closure { .. }) {
                    return Err(err(
                        format!(
                            "decorator '{dname}' must return a function/closure, found {result_ty}"
                        ),
                        *dspan,
                    )
                    .with_file(i));
                }
                let gname = if is_root {
                    fname.clone()
                } else {
                    format!("{}.{}", m.name, fname)
                };
                globals.insert(fname.clone(), result_ty);
                if let Some((_, t)) = globals_order.iter_mut().find(|(n, _)| n == &gname) {
                    *t = result_ty;
                } else {
                    globals_order.push((gname, result_ty));
                }
            }
        }
        let init = if executable && is_root && script.is_empty() && !need_init_for_deco {
            // PyRs convenience: a root that is only definitions calls main()
            if let Some(sig) = funcs.get("main") {
                if !sig.params.is_empty() || sig.vararg.is_some() || sig.kwarg.is_some() {
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
                    is_generator: false,
                    yield_ty: None,
                })
            } else {
                None
            }
        } else if is_root && script.is_empty() && need_init_for_deco {
            // Empty script but need init for @decorators only.
            Some(ir::Function {
                name: ENTRY_NAME.to_string(),
                params: vec![],
                ret: ir::Ty::None,
                locals: vec![],
                body: vec![],
                is_generator: false,
                yield_ty: None,
            })
        } else {
            let init_def = ast::FuncDef {
                name: init_name.clone(),
                params: vec![],
                // Compiler-synthesized: no `/` or `*` markers.
                posonly_end: 0,
                kwonly_start: Option::None,
                vararg: None,
                kwarg: None,
                ret: None,
                body: script,
                span: Span::default(),
                decorators: Vec::new(),
            };
            let (mut f, nested) =
                lower_function(&init_def, &mctx, &mut globals, &mut globals_order, true)
                    .map_err(|d| d.with_file(i))?;
            out_funcs.extend(nested);
            if !is_root {
                add_init_guard(&mut f, &m.name, &mut globals_order);
            }
            Some(f)
        };

        // Apply free-function decorators: `h = deco(<closure of h>)` at init start.
        let mut init = init;
        if !free_decorators.is_empty() {
            let f = init.as_mut().ok_or_else(|| {
                err(
                    "internal: missing init for free-function decorators",
                    Span::default(),
                )
                .with_file(i)
            })?;
            let mut deco_stmts = Vec::new();
            let mut deco_locals: Vec<(String, ir::Ty)> = Vec::new();
            for (fname, dname, dspan) in &free_decorators {
                let sig = funcs.get(fname).cloned().ok_or_else(|| {
                    err(
                        format!("internal: missing sig for decorated '{fname}'"),
                        *dspan,
                    )
                    .with_file(i)
                })?;
                let params: Vec<ir::Ty> = sig.params.iter().map(|p| p.ty).collect();
                let ir_name = if is_root {
                    fname.clone()
                } else {
                    format!("{}.{}", m.name, fname)
                };
                let clos_ty = ir::closure_of_full(&params, sig.ret, &[], &ir_name);
                let clos_tmp = format!(".deco.clos.{fname}");
                deco_locals.push((clos_tmp.clone(), clos_ty));
                deco_stmts.push(ir::Stmt::Assign {
                    name: clos_tmp.clone(),
                    value: ir::Expr {
                        ty: clos_ty,
                        kind: ir::ExprKind::MakeClosure {
                            func: ir_name,
                            captures: vec![],
                            capture_is_cell: vec![],
                        },
                    },
                });
                // Call decorator: may be free function or nested... free only.
                let deco_sig = funcs.get(dname.as_str()).cloned().ok_or_else(|| {
                    err(
                        format!(
                            "decorator '{dname}' is not a function in this module \
                             (only same-module free functions are supported)"
                        ),
                        *dspan,
                    )
                    .with_file(i)
                })?;
                if deco_sig.params.len() != 1 {
                    return Err(err(
                        format!(
                            "decorator '{dname}' must take exactly one argument (the function)"
                        ),
                        *dspan,
                    )
                    .with_file(i));
                }
                // Coerce clos into the decorator's param type if it's Closure-shaped.
                let deco_param_ty = deco_sig.params[0].ty;
                let clos_arg = if deco_param_ty == clos_ty {
                    ir::Expr {
                        ty: clos_ty,
                        kind: ir::ExprKind::Local(clos_tmp.clone()),
                    }
                } else if let ir::Ty::Closure { .. } = deco_param_ty {
                    // Retype for homogeneous closure param.
                    ir::Expr {
                        ty: deco_param_ty,
                        kind: ir::ExprKind::Local(clos_tmp.clone()),
                    }
                } else {
                    return Err(err(
                        format!(
                            "decorator '{dname}' parameter type must be a function/closure, \
                             found {deco_param_ty}"
                        ),
                        *dspan,
                    )
                    .with_file(i));
                };
                let deco_call = ir::Expr {
                    ty: deco_sig.ret,
                    kind: ir::ExprKind::Call {
                        func: if is_root {
                            dname.clone()
                        } else {
                            format!("{}.{}", m.name, dname)
                        },
                        args: vec![clos_arg],
                    },
                };
                let gname = if is_root {
                    fname.clone()
                } else {
                    format!("{}.{}", m.name, fname)
                };
                deco_stmts.push(ir::Stmt::GlobalAssign {
                    name: gname,
                    value: deco_call,
                });
            }
            // Prepend decorator application; register temps as locals.
            f.locals.extend(deco_locals);
            let mut body = deco_stmts;
            body.append(&mut f.body);
            f.body = body;
        }

        match init {
            Some(f) => out_funcs.push(f),
            None if executable && is_root => {
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
        let own_func_names: std::collections::HashSet<String> = funcs.keys().cloned().collect();
        let mut data = ModuleData {
            funcs: funcs.clone(),
            globals,
            classes: module_classes.get(&m.name).cloned().unwrap_or_default(),
            reexports: HashMap::new(),
        };
        // Dependencies are already in `mods` (topo order), so re-exports can
        // resolve origin types/sigs. Parent packages that import children are
        // lowered after those children.
        apply_reexports(&mut data, &own_func_names, &all_imports[i], &mods, m.ast);
        mods.insert(m.name.clone(), data);
    }

    let classes = with_class_env(|e| e.infos.clone());
    let exc_classes = with_exc_env(|e| e.classes.clone());
    Ok(ir::Module {
        funcs: out_funcs,
        globals: out_globals,
        classes,
        exc_classes,
        entry: ENTRY_NAME.to_string(),
    })
}

thread_local! {
    // Bare name -> closure type, for the module whose globals are being
    // seeded. Seeding runs before any expression is typed, so it cannot ask
    // the usual machinery what `cmd_build` is; this is the same shape as the
    // class environment it already consults for `Point(1, 2)`.
    pub(crate) static SEED_FUNCS: std::cell::RefCell<HashMap<String, ir::Ty>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Record the module-level functions that may appear as *values* in a global
/// initializer. A function taking `*args`/`**kwargs`, carrying defaults, or
/// generating is skipped: those cannot be closure values, and leaving them out
/// means the global is simply unseeded, which is the pre-existing behaviour.
pub(crate) fn set_seed_funcs(funcs: &HashMap<String, FuncSig>, prefix: &str) {
    let mut map = HashMap::new();
    for (name, sig) in funcs {
        if sig.vararg.is_some()
            || sig.kwarg.is_some()
            || sig.is_generator
            || sig.params.iter().any(|p| p.default.is_some())
        {
            continue;
        }
        let params: Vec<ir::Ty> = sig.params.iter().map(|p| p.ty).collect();
        let ir_name = format!("{prefix}{name}");
        map.insert(
            name.clone(),
            ir::closure_of_full(&params, sig.ret, &[], &ir_name),
        );
    }
    SEED_FUNCS.with(|f| *f.borrow_mut() = map);
}

pub(crate) fn clear_seed_funcs() {
    SEED_FUNCS.with(|f| f.borrow_mut().clear());
}

pub(crate) fn seed_func_ty(name: &str) -> Option<ir::Ty> {
    SEED_FUNCS.with(|f| f.borrow().get(name).copied())
}

/// Seed module globals from top-level assignments (literals / names), joining
/// multiple assignment types so bare multi-assign yields a union storage type.
/// Explicit annotations fix storage and are never widened by later bare assigns.
pub(crate) fn seed_globals_from_script(
    script: &[ast::Stmt],
    globals: &mut HashMap<String, ir::Ty>,
    globals_order: &mut Vec<(String, ir::Ty)>,
    is_root: bool,
    module: &str,
) {
    let own = |name: &str| -> String {
        if is_root {
            name.to_string()
        } else {
            format!("{module}.{name}")
        }
    };
    let mut annotated: HashSet<String> = HashSet::new();
    for st in script {
        if let ast::StmtKind::Assign {
            targets,
            annotation,
            value,
        } = &st.kind
        {
            // Annotation wins as the storage type (do not widen past it).
            if let Some(ann) = annotation {
                if let Ok(ty) = resolve_type_checked(*ann, st.span) {
                    for t in targets {
                        if let ast::AssignTarget::Name { name, .. } = t {
                            use std::collections::hash_map::Entry;
                            if let Entry::Vacant(e) = globals.entry(name.clone()) {
                                e.insert(ty);
                                globals_order.push((own(name), ty));
                            }
                            annotated.insert(name.clone());
                        }
                    }
                }
                continue;
            }
            let Some(ty) = seed_ty_from_expr(value) else {
                continue;
            };
            for t in targets {
                if let ast::AssignTarget::Name { name, .. } = t {
                    if annotated.contains(name) {
                        continue; // keep annotated storage; bind_name will error on bad RHS
                    }
                    use std::collections::hash_map::Entry;
                    match globals.entry(name.clone()) {
                        Entry::Vacant(e) => {
                            e.insert(ty);
                            globals_order.push((own(name), ty));
                        }
                        Entry::Occupied(mut e) => {
                            let joined = join_types(*e.get(), ty);
                            if joined != *e.get() {
                                e.insert(joined);
                                let q = own(name);
                                if let Some((_, t)) =
                                    globals_order.iter_mut().find(|(n, _)| n == &q)
                                {
                                    *t = joined;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Join two seeded element types.
///
/// The literal rule (`join_elem_types`) is what lowering will use, so the seed
/// has to agree with it or the global's storage type and its initializer
/// disagree. It declines a provisional `list[Any]` from an empty literal,
/// though — `{"a": ["b"], "d": []}` is a `dict[str, list[str]]` — so that one
/// case falls back to the assignment join, which resolves it the same way.
pub(crate) fn seed_join(a: ir::Ty, b: ir::Ty) -> Option<ir::Ty> {
    if let Some(t) = join_elem_types(a, b) {
        return Some(t);
    }
    let provisional = |t: ir::Ty| matches!(t, ir::Ty::List(e) if *e == ir::Ty::Any);
    if provisional(a) || provisional(b) {
        return Some(join_types(a, b));
    }
    Option::None
}

pub(crate) fn seed_ty_from_expr(e: &ast::Expr) -> Option<ir::Ty> {
    match &e.kind {
        ast::ExprKind::Int(_) | ast::ExprKind::IntDigits(_) => Some(ir::Ty::Int),
        ast::ExprKind::Float(_) => Some(ir::Ty::Float),
        ast::ExprKind::Bool(_) => Some(ir::Ty::Bool),
        ast::ExprKind::Str(_) | ast::ExprKind::JoinedStr(_) => Some(ir::Ty::Str),
        ast::ExprKind::NoneLit => Some(ir::Ty::None),
        // Module-level empty lists: pre-seed as list[Any] so nested free reads
        // (before entry init runs) resolve the name.
        ast::ExprKind::ListLit(items) if items.is_empty() => Some(ir::list_of(ir::Ty::Any)),
        // Container literals, so a module-level table or config is visible to
        // functions the way a module-level scalar already is. Any element this
        // cannot type leaves the whole global unseeded, which is the safe
        // direction: the name is then simply not in scope, as before.
        ast::ExprKind::ListLit(items) => {
            let mut elem: Option<ir::Ty> = Option::None;
            for it in items {
                let ast::ListElem::Item(e) = it else {
                    return Option::None;
                };
                let t = seed_ty_from_expr(e)?;
                elem = Some(match elem {
                    Option::None => t,
                    Some(prev) => seed_join(prev, t)?,
                });
            }
            Some(ir::list_of(elem?))
        }
        ast::ExprKind::TupleLit(items) => {
            let mut ts = Vec::with_capacity(items.len());
            for it in items {
                ts.push(seed_ty_from_expr(it)?);
            }
            Some(ir::tuple_of(&ts))
        }
        ast::ExprKind::DictLit(items) if !items.is_empty() => {
            let mut kt: Option<ir::Ty> = Option::None;
            let mut vt: Option<ir::Ty> = Option::None;
            for (k, v) in items {
                let k = seed_ty_from_expr(k)?;
                let v = seed_ty_from_expr(v)?;
                kt = Some(match kt {
                    Option::None => k,
                    Some(prev) => seed_join(prev, k)?,
                });
                vt = Some(match vt {
                    Option::None => v,
                    Some(prev) => seed_join(prev, v)?,
                });
            }
            Some(ir::dict_of(kt?, vt?))
        }
        ast::ExprKind::SetLit(items) if !items.is_empty() => {
            let mut elem: Option<ir::Ty> = Option::None;
            for it in items {
                let t = seed_ty_from_expr(it)?;
                elem = Some(match elem {
                    Option::None => t,
                    Some(prev) => seed_join(prev, t)?,
                });
            }
            Some(ir::set_of(elem?))
        }
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg | ast::UnaryOp::Invert,
            operand,
        } => seed_ty_from_expr(operand),
        // A module-level function used as a value: `HANDLER = run`, or one
        // inside a table. Without this the global is unseeded and the name is
        // invisible inside every function — which is exactly where a command
        // dispatch table gets read.
        ast::ExprKind::Name(n) => seed_func_ty(n),
        // Class construction: `Point(1, 2)` → Class type for multi-assign join.
        ast::ExprKind::Call { func, .. } => lookup_class(func).map(ir::Ty::Class),
        ast::ExprKind::MethodCall { base, method, .. } => {
            // `mod.Class(...)` construction as a method call form.
            if let Some(mod_name) = match &base.kind {
                ast::ExprKind::Name(n) => Some(n.as_str()),
                _ => None,
            } {
                // Only if Name is an imported module — best-effort: any class in any module.
                lookup_class_in_module(mod_name, method)
                    .or_else(|| {
                        // Also try bare method name as class in current module.
                        lookup_class(method)
                    })
                    .map(ir::Ty::Class)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Prepend a run-once guard to a module init: `if <mod>.__done__: return;
/// <mod>.__done__ = True; ...`.
pub(crate) fn add_init_guard(
    f: &mut ir::Function,
    module: &str,
    globals_order: &mut Vec<(String, ir::Ty)>,
) {
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
