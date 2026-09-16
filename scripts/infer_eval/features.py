"""Features and labels for the learned type predictors.

A predictor sees, for one parameter, what a compiler can see without call
sites: the names (parameter, function, class), the default, and every use of
the parameter in the function body, each reduced to a *usage event* -- the
operation the use sits in and the kind of the other operand. This module is
the reference definition. `semantic/src/predict.rs` implements the same
events over the PyRs AST, and `parity.py` checks the two agree on the corpus.

Labels are the representation vocabulary PyRs compiles to, not Python's
annotation vocabulary: a user class is `class` (which class is chosen from the
program), and anything PyRs cannot name statically is `other`.
"""
import ast
import re

LABELS = [
    "int", "float", "str", "bool",
    "list[int]", "list[float]", "list[str]", "list[other]",
    "dict[str,int]", "dict[str,str]", "dict[str,float]", "dict[other]",
    "set", "tuple", "callable", "iterator", "class",
    "int?", "float?", "str?", "bool?", "class?", "other?",
    "any", "other",
]
LABEL_INDEX = {l: i for i, l in enumerate(LABELS)}

MAX_EVENTS = 48


def subtokens(name):
    parts = []
    for chunk in name.strip("_").split("_"):
        parts += [p.lower() for p in re.findall(r"[A-Z]+(?![a-z])|[A-Z]?[a-z]+|\d+", chunk)]
    return parts or ([name.strip("_").lower()] if name.strip("_") else [])


def expr_kind(e):
    """A coarse kind for the other operand of a use."""
    if isinstance(e, ast.Constant):
        v = e.value
        if isinstance(v, bool):
            return "bool"
        if isinstance(v, int):
            return "int"
        if isinstance(v, float):
            return "float"
        if isinstance(v, str):
            return "str"
        if v is None:
            return "none"
        return "other"
    if isinstance(e, ast.UnaryOp) and isinstance(e.op, ast.USub) and isinstance(e.operand, ast.Constant) \
            and type(e.operand.value) in (int, float):
        return "int" if type(e.operand.value) is int else "float"
    if isinstance(e, ast.JoinedStr):
        return "str"
    if isinstance(e, ast.Name):
        return "name"
    if isinstance(e, ast.Call):
        if isinstance(e.func, ast.Name) and e.func.id in ("int", "float", "str", "bool", "len"):
            return "int" if e.func.id == "len" else e.func.id
        return "call"
    if isinstance(e, ast.Attribute):
        return "attr"
    if isinstance(e, ast.Subscript):
        return "sub"
    if isinstance(e, (ast.List, ast.ListComp)):
        return "list"
    if isinstance(e, (ast.Dict, ast.DictComp)):
        return "dict"
    if isinstance(e, ast.Tuple):
        return "tuple"
    if isinstance(e, ast.BinOp):
        return "binop"
    return "other"


BINOPS = {ast.Add: "add", ast.Sub: "sub", ast.Mult: "mul", ast.Div: "div", ast.FloorDiv: "floordiv",
          ast.Mod: "mod", ast.Pow: "pow", ast.BitAnd: "bitand", ast.BitOr: "bitor", ast.BitXor: "bitxor",
          ast.LShift: "lshift", ast.RShift: "rshift", ast.MatMult: "matmul"}
CMPOPS = {ast.Eq: "eq", ast.NotEq: "ne", ast.Lt: "lt", ast.LtE: "le", ast.Gt: "gt", ast.GtE: "ge",
          ast.In: "in", ast.NotIn: "in", ast.Is: "is", ast.IsNot: "is"}
UNOPS = {ast.USub: "neg", ast.UAdd: "pos", ast.Not: "not", ast.Invert: "invert"}


def default_kind(d):
    if d is None:
        return "absent"
    k = expr_kind(d)
    return k if k in ("int", "float", "str", "bool", "none", "list", "dict", "tuple") else "other"


def usage_events(func, name):
    """The usage events of parameter `name` in `func`'s body, in source order."""
    parents = {}
    for node in ast.walk(func):
        for child in ast.iter_child_nodes(node):
            parents[child] = node
    events = []
    body_nodes = []
    for stmt in func.body:
        body_nodes.extend(ast.walk(stmt))
    occurrences = [n for n in body_nodes if isinstance(n, ast.Name) and n.id == name]
    occurrences.sort(key=lambda n: (n.lineno, n.col_offset))
    for n in occurrences:
        ev = event_for(n, parents)
        if ev:
            events.append(ev)
        if len(events) >= MAX_EVENTS:
            break
    return events


def event_for(n, parents):
    p = parents.get(n)
    if isinstance(n.ctx, ast.Store):
        return "rebound"
    if isinstance(p, ast.Attribute) and p.value is n:
        gp = parents.get(p)
        if isinstance(gp, ast.Call) and gp.func is p:
            return f"meth:{p.attr}"
        if isinstance(p.ctx, ast.Store):
            return f"setattr:{p.attr}"
        return f"attr:{p.attr}"
    if isinstance(p, ast.Call):
        if p.func is n:
            return f"called:{min(len(p.args), 4)}"
        callee = None
        if isinstance(p.func, ast.Name):
            callee = p.func.id
        elif isinstance(p.func, ast.Attribute):
            callee = "." + p.func.attr
        if callee is not None:
            for i, a in enumerate(p.args):
                if a is n:
                    return f"arg:{callee}:{min(i, 3)}"
            return f"kwarg:{callee}"
        return "arg:?"
    if isinstance(p, ast.keyword):
        call = parents.get(p)
        callee = "?"
        if isinstance(call, ast.Call):
            if isinstance(call.func, ast.Name):
                callee = call.func.id
            elif isinstance(call.func, ast.Attribute):
                callee = "." + call.func.attr
        return f"kwarg:{callee}"
    if isinstance(p, ast.BinOp):
        other = p.right if p.left is n else p.left
        side = "l" if p.left is n else "r"
        return f"binop:{BINOPS.get(type(p.op), 'other')}:{side}:{expr_kind(other)}"
    if isinstance(p, ast.BoolOp):
        return "boolop"
    if isinstance(p, ast.Compare):
        operands = [p.left] + p.comparators
        i = operands.index(n)
        op = p.ops[i - 1] if i > 0 else p.ops[0]
        other = operands[i - 1] if i > 0 else operands[1]
        opname = CMPOPS.get(type(op), "other")
        if opname == "is":
            return "is:" + expr_kind(other)
        if opname == "in":
            return ("in:needle:" if i == 0 else "in:container:") + expr_kind(other)
        return f"cmp:{opname}:{expr_kind(other)}"
    if isinstance(p, ast.UnaryOp):
        return f"unary:{UNOPS.get(type(p.op), 'other')}"
    if isinstance(p, ast.Subscript):
        if p.value is n:
            if isinstance(p.slice, ast.Slice):
                return "slice"
            return ("setitem:" if isinstance(p.ctx, ast.Store) else "getitem:") + expr_kind(p.slice)
        return "index_of"
    if isinstance(p, ast.Slice):
        return "slice_bound"
    if isinstance(p, (ast.For, ast.comprehension)) and p.iter is n:
        return "iterated"
    if isinstance(p, (ast.If, ast.While, ast.IfExp, ast.Assert)) and p.test is n:
        return "truth"
    if isinstance(p, ast.Return):
        return "returned"
    if isinstance(p, ast.Yield):
        return "yielded"
    if isinstance(p, (ast.Assign, ast.AnnAssign)):
        targets = p.targets if isinstance(p, ast.Assign) else [p.target]
        t = targets[0]
        if isinstance(t, ast.Attribute):
            return f"stored:{t.attr}"
        if isinstance(t, ast.Subscript):
            return "stored:item"
        return "assigned"
    if isinstance(p, ast.AugAssign):
        if p.target is n:
            return f"aug:{BINOPS.get(type(p.op), 'other')}:{expr_kind(p.value)}"
        return f"aug_value:{BINOPS.get(type(p.op), 'other')}"
    if isinstance(p, ast.FormattedValue):
        return "fstring"
    if isinstance(p, (ast.List, ast.Tuple, ast.Set)):
        return "in_literal"
    if isinstance(p, ast.Dict):
        return "in_dict"
    return "other_use"


def features(func, param_index, cls_name, is_static):
    """Name, default and usage features for parameter `param_index` (user index)."""
    args = func.args.posonlyargs + func.args.args
    offset = 1 if (cls_name and not is_static and args) else 0
    user = args[offset:]
    a = user[param_index]
    defaults = [None] * (len(args) - len(func.args.defaults)) + list(func.args.defaults)
    d = defaults[offset + param_index]
    short = func.name
    feats = []
    feats += [f"pn:{t}" for t in subtokens(a.arg)]
    feats.append(f"pnf:{a.arg.lower()}")
    feats += [f"fn:{t}" for t in subtokens(short)]
    if cls_name:
        feats += [f"cls:{t}" for t in subtokens(cls_name)]
        if short.startswith("__") and short.endswith("__"):
            feats.append(f"dunder:{short}")
    feats.append(f"pos:{min(param_index, 5)}")
    feats.append(f"npar:{min(len(user), 6)}")
    feats.append(f"def:{default_kind(d)}")
    events = usage_events(func, a.arg)
    feats.append(f"nuse:{min(len(events), 5)}")
    return feats, events


def label_of(ann, classes):
    """Map a Python annotation to LABELS (None when unlabelled)."""
    if ann is None:
        return None
    if isinstance(ann, ast.Constant) and isinstance(ann.value, str):
        try:
            ann = ast.parse(ann.value, mode="eval").body
        except SyntaxError:
            return "other"

    def name(x):
        if isinstance(x, ast.Name):
            return x.id
        if isinstance(x, ast.Attribute):
            return x.attr
        return None

    def members(x):
        if isinstance(x, ast.BinOp) and isinstance(x.op, ast.BitOr):
            return members(x.left) + members(x.right)
        if isinstance(x, ast.Subscript) and name(x.value) == "Union":
            s = x.slice
            return sum((members(e) for e in (s.elts if isinstance(s, ast.Tuple) else [s])), [])
        if isinstance(x, ast.Subscript) and name(x.value) == "Optional":
            return members(x.slice) + [ast.Constant(None)]
        return [x]

    ms = members(ann)
    nullable = any(isinstance(m, ast.Constant) and m.value is None for m in ms)
    ms = [m for m in ms if not (isinstance(m, ast.Constant) and m.value is None)]
    if len(ms) != 1:
        return "other?" if nullable else "other"
    base = single_label(ms[0], classes)
    if nullable:
        return f"{base}?" if base in ("int", "float", "str", "bool", "class") else "other?"
    return base


def single_label(m, classes):
    def name(x):
        if isinstance(x, ast.Name):
            return x.id
        if isinstance(x, ast.Attribute):
            return x.attr
        return None
    base = name(m.value) if isinstance(m, ast.Subscript) else name(m)
    args = []
    if isinstance(m, ast.Subscript):
        args = m.slice.elts if isinstance(m.slice, ast.Tuple) else [m.slice]
    arg_labels = [single_label(x, classes) for x in args]
    if base in ("int", "float", "str", "bool"):
        return base
    if base in ("list", "List"):
        if len(arg_labels) == 1 and arg_labels[0] in ("int", "float", "str"):
            return f"list[{arg_labels[0]}]"
        return "list[other]"
    if base in ("Sequence", "MutableSequence"):
        return "list[other]"
    if base in ("dict", "Dict"):
        if len(arg_labels) == 2 and arg_labels[0] == "str" and arg_labels[1] in ("int", "str", "float"):
            return f"dict[str,{arg_labels[1]}]"
        return "dict[other]"
    if base in ("Mapping", "MutableMapping"):
        return "dict[other]"
    if base in ("set", "Set", "frozenset", "FrozenSet", "AbstractSet"):
        return "set"
    if base in ("tuple", "Tuple"):
        return "tuple"
    if base in ("Callable",):
        return "callable"
    if base in ("Iterator", "Iterable", "Generator"):
        return "iterator"
    if base in ("Any", "object"):
        return "any"
    if base in classes:
        return "class"
    return "other"


def pyrs_type_label(t, class_names):
    """Map a `pyrs check --show-types` type to LABELS."""
    t = t.strip()
    members = [m.strip() for m in split_union(t)]
    nullable = "None" in members
    members = [m for m in members if m != "None"]
    if len(members) != 1:
        return "other?" if nullable else "other"
    base = pyrs_single(members[0], class_names)
    if nullable:
        return f"{base}?" if base in ("int", "float", "str", "bool", "class") else "other?"
    return base


def split_union(t):
    out, depth, cur = [], 0, ""
    for c in t:
        if c in "[(":
            depth += 1
        elif c in "])":
            depth -= 1
        if c == "|" and depth == 0:
            out.append(cur)
            cur = ""
        else:
            cur += c
    out.append(cur)
    return out


def pyrs_single(m, class_names):
    if m in ("int", "float", "str", "bool"):
        return m
    if m in ("list[int]", "list[float]", "list[str]"):
        return m
    if m.startswith("list["):
        return "list[other]"
    if m in ("dict[str, int]", "dict[str, str]", "dict[str, float]"):
        return m.replace(", ", ",")
    if m.startswith("dict["):
        return "dict[other]"
    if m.startswith("set["):
        return "set"
    if m.startswith("tuple["):
        return "tuple"
    if m.startswith("Callable["):
        return "callable"
    if m.startswith("Iterator["):
        return "iterator"
    if m == "Any":
        return "any"
    if m.split(".")[-1] in class_names:
        return "class"
    return "other"


def class_names(src):
    try:
        tree = ast.parse(src)
    except (SyntaxError, ValueError):
        return set()
    return {n.name for n in ast.walk(tree) if isinstance(n, ast.ClassDef)}


def examples_from_source(src, origin, project, project_classes=None):
    """Every labelled-or-not parameter of every function in `src`.

    A class defined anywhere in the same project is a user class: a whole
    program compiles it, wherever it is imported from."""
    try:
        tree = ast.parse(src)
    except (SyntaxError, ValueError):
        return []
    classes = {n.name for n in ast.walk(tree) if isinstance(n, ast.ClassDef)}
    if project_classes:
        classes |= project_classes
    out = []

    def visit(body, cls):
        for node in body:
            if isinstance(node, ast.ClassDef):
                visit(node.body, node.name)
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                static = any(isinstance(d, ast.Name) and d.id == "staticmethod" for d in node.decorator_list)
                args = node.args.posonlyargs + node.args.args
                offset = 1 if (cls and not static and args) else 0
                for i, a in enumerate(args[offset:]):
                    feats, events = features(node, i, cls, static)
                    out.append({
                        "project": project, "origin": origin,
                        "func": (cls + "." if cls else "") + node.name, "param": a.arg,
                        "label": label_of(a.annotation, classes),
                        "feats": feats, "events": events,
                    })
                visit(node.body, None)

    visit(tree.body, None)
    return out
