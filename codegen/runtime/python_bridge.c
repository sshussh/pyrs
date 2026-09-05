/* Experimental CPython adapter. The native ABI stays owned by runtime.c. */
#define PY_SSIZE_T_CLEAN
#include <Python.h>
#include "runtime.c"

typedef struct {
    PyObject *object;              /* borrowed from the call's args/kwargs */
    PyObject *temporary;           /* owned during integer conversion */
    Py_buffer view;                /* owns the buffer export until cleanup */
    void *owned;                  /* copied list slots, allocated by PyMem */
    long long integer;
    double floating;
    _Bool boolean;
    PyrsList list;
    PyrsStr *string;               /* native result, rooted with this array */
} BridgeArg;

static uint64_t bridge_owner;
static int bridge_initialized;
static int bridge_busy;

static int bridge_module_exec(PyObject *module) {
    (void)module;
    if (PyInterpreterState_Get() != PyInterpreterState_Main()) {
        PyErr_SetString(PyExc_ImportError, "PyRs extensions require the main interpreter");
        return -1;
    }
    uint64_t thread = PyThreadState_GetID(PyThreadState_Get());
    if (bridge_initialized && bridge_owner != thread) {
        PyErr_SetString(PyExc_ImportError, "PyRs extension must use its original importing thread");
        return -1;
    }
    bridge_owner = thread;
    if (!bridge_initialized) {
        /* The owner thread cannot change. Discover its full stack mapping once
         * at import, rather than reparsing /proc/self/maps on every call. */
        pyrs_gc_init(&thread);
    }
    bridge_initialized = 1;
    return 0;
}

static PyObject *bridge_exception(int type) {
    switch (type) {
    case PYRS_EXC_VALUE: return PyExc_ValueError;
    case PYRS_EXC_KEY: return PyExc_KeyError;
    case PYRS_EXC_INDEX: return PyExc_IndexError;
    case PYRS_EXC_ZERODIV: return PyExc_ZeroDivisionError;
    case PYRS_EXC_TYPE: return PyExc_TypeError;
    case PYRS_EXC_RUNTIME: return PyExc_RuntimeError;
    case PYRS_EXC_GENEXIT: return PyExc_GeneratorExit;
    case PYRS_EXC_OVERFLOW: return PyExc_OverflowError;
    case PYRS_EXC_EOF: return PyExc_EOFError;
    case PYRS_EXC_FILENOTFOUND: return PyExc_FileNotFoundError;
    case PYRS_EXC_OS: return PyExc_OSError;
    case PYRS_EXC_NAME: return PyExc_NameError;
    case PYRS_EXC_UNBOUNDLOCAL: return PyExc_UnboundLocalError;
    case PYRS_EXC_STOPITER: return PyExc_StopIteration;
    case PYRS_EXC_EXCEPTION: return PyExc_Exception;
    case PYRS_EXC_PERMISSION: return PyExc_PermissionError;
    case PYRS_EXC_ISADIR: return PyExc_IsADirectoryError;
    case PYRS_EXC_ASSERT: return PyExc_AssertionError;
    default: return PyExc_RuntimeError;
    }
}

static int bridge_integer(BridgeArg *arg) {
    int overflow = 0;
    long long value = PyLong_AsLongLongAndOverflow(arg->object, &overflow);
    if (PyErr_Occurred()) return -1;
    if (!overflow) {
        arg->integer = pyrs_int_from_i64(value);
        return 0;
    }
    /* Hexadecimal avoids CPython's decimal digit limit, including for values
     * much larger than machine integers. Exact ints cannot override this. */
    arg->temporary = PyNumber_ToBase(arg->object, 16);
    if (arg->temporary == NULL) return -1;
    Py_ssize_t length;
    const char *digits = PyUnicode_AsUTF8AndSize(arg->temporary, &length);
    if (digits == NULL) return -1;
    int sign = digits[0] == '-' ? -1 : 1;
    Py_ssize_t start = sign < 0 ? 3 : 2; /* [-]0x */
    size_t n = (size_t)(length - start);
    size_t count = (n + 15) / 16;
    unsigned long long *limbs = calloc(count, sizeof(*limbs));
    if (limbs == NULL) { PyErr_NoMemory(); return -1; }
    for (size_t i = 0; i < n; i++) {
        unsigned char digit = (unsigned char)digits[length - 1 - (Py_ssize_t)i];
        limbs[i / 16] |= (unsigned long long)int_digit_val(digit) << (4 * (i % 16));
    }
    arg->integer = int_from_sign_limbs(sign, limbs, (long long)count);
    Py_CLEAR(arg->temporary);
    return 0;
}

static int bridge_sequence(BridgeArg *arg, const char *function, const char *name) {
    if (PyList_CheckExact(arg->object)) {
        Py_ssize_t n = PyList_GET_SIZE(arg->object);
        if ((size_t)n > SIZE_MAX / sizeof(long long)) {
            PyErr_NoMemory();
            return -1;
        }
        arg->owned = PyMem_Malloc(n ? (size_t)n * sizeof(long long) : 1);
        if (arg->owned == NULL) { PyErr_NoMemory(); return -1; }
        arg->list.len = n;
        /* PyMem memory: the runtime's libc allocator must never resize it, and
         * `arg->owned` must stay the exact pointer PyMem_Free receives. */
        arg->list.cap = PYRS_LIST_BORROWED_CAP;
        arg->list.data = arg->owned;
        for (Py_ssize_t i = 0; i < n; i++) {
            PyObject *value = PyList_GET_ITEM(arg->object, i);
            if (!PyFloat_CheckExact(value)) {
                PyErr_Format(PyExc_TypeError, "%s(): '%s' requires a list of exact floats", function, name);
                return -1;
            }
            double number = PyFloat_AS_DOUBLE(value);
            memcpy(&arg->list.data[i], &number, sizeof(number));
        }
        return 0;
    }
    if (PyObject_GetBuffer(arg->object, &arg->view, PyBUF_FORMAT | PyBUF_STRIDES) < 0) {
        /* Keep exporter exceptions (including MemoryError and user-defined
         * failures) intact. Unsupported layouts use our validation below. */
        return -1;
    }
    Py_buffer *view = &arg->view;
    const char *format = view->format;
    if (view->ndim != 1 || view->itemsize != sizeof(double) || format == NULL
        || (strcmp(format, "d") && strcmp(format, "@d") && strcmp(format, "=d"))
        || view->len < 0 || view->len % sizeof(double) != 0
        || view->shape == NULL || view->shape[0] != view->len / (Py_ssize_t)sizeof(double)
        || !PyBuffer_IsContiguous(view, 'C')
        || (view->suboffsets != NULL && view->suboffsets[0] >= 0)
        || (view->len && (view->buf == NULL || (uintptr_t)view->buf % _Alignof(double)))) {
        PyErr_Format(PyExc_TypeError,
            "%s(): '%s' requires an aligned contiguous 1D native float64 buffer", function, name);
        return -1;
    }
    arg->list.len = view->shape[0];
    /* Caller-owned exporter memory (NumPy/pandas). Resizing it would free a
     * foreign pointer and invalidate the export we still have to release. */
    arg->list.cap = PYRS_LIST_BORROWED_CAP;
    arg->list.data = view->buf;
    return 0;
}

static int bridge_convert(BridgeArg *arg, int kind, const char *function, const char *name) {
    PyObject *value = arg->object;
    if (kind == 4) return bridge_sequence(arg, function, name);
    if ((kind == 0 && !PyLong_CheckExact(value))
        || (kind == 1 && !PyFloat_CheckExact(value))
        || (kind == 2 && !PyBool_Check(value))
        || (kind == 3 && value != Py_None)) {
        const char *types[] = {"int", "float", "bool", "None"};
        PyErr_Format(PyExc_TypeError, "%s(): '%s' requires exact %s", function, name, types[kind]);
        return -1;
    }
    if (kind == 0) return bridge_integer(arg);
    if (kind == 1) arg->floating = PyFloat_AS_DOUBLE(value);
    if (kind == 2) arg->boolean = value == Py_True;
    return 0;
}

static PyObject *bridge_result(BridgeArg *arg, int kind) {
    switch (kind) {
    case 0: {
        if (pyrs_int_is_small(arg->integer)) {
            return PyLong_FromLongLong(pyrs_int_small_val(arg->integer));
        }
        long long length;
        char *digits = int_to_base_str(arg->integer, 16, 0, &length);
        PyObject *result = PyLong_FromString(digits, NULL, 16);
        free(digits);
        return result;
    }
    case 1: return PyFloat_FromDouble(arg->floating);
    case 2: return PyBool_FromLong(arg->boolean);
    case 5:
        if (arg->string == NULL) {
            PyErr_SetString(PyExc_RuntimeError, "native string result is null");
            return NULL;
        }
        if (arg->string->len < 0 || (unsigned long long)arg->string->len > PY_SSIZE_T_MAX) {
            PyErr_SetString(PyExc_OverflowError, "native string result is too large");
            return NULL;
        }
        /* Copy into a Python-owned string before the native root is released.
         * The explicit length preserves embedded NULs. Byte-oriented native
         * string operations can produce invalid UTF-8; report it strictly. */
        return PyUnicode_DecodeUTF8(arg->string->data, (Py_ssize_t)arg->string->len, "strict");
    default: return Py_NewRef(Py_None);
    }
}

static int bridge_bind(BridgeArg *bound, PyObject *args, PyObject *kwargs,
                       const char *function, size_t count, const char **names) {
    Py_ssize_t positional = PyTuple_GET_SIZE(args);
    if ((size_t)positional > count) {
        PyErr_Format(PyExc_TypeError, "%s() takes %zu arguments (%zd given)", function, count, positional);
        return -1;
    }
    for (Py_ssize_t i = 0; i < positional; i++) bound[i].object = PyTuple_GET_ITEM(args, i);
    Py_ssize_t position = 0;
    PyObject *key, *value;
    while (kwargs != NULL && PyDict_Next(kwargs, &position, &key, &value)) {
        if (!PyUnicode_Check(key)) {
            PyErr_SetString(PyExc_TypeError, "keywords must be strings");
            return -1;
        }
        size_t i;
        for (i = 0; i < count; i++) {
            if (PyUnicode_CompareWithASCIIString(key, names[i]) == 0) break;
        }
        if (i == count) {
            PyErr_Format(PyExc_TypeError, "%s() got an unexpected keyword argument '%U'", function, key);
            return -1;
        }
        if (bound[i].object != NULL) {
            PyErr_Format(PyExc_TypeError, "%s() got multiple values for argument '%s'", function, names[i]);
            return -1;
        }
        bound[i].object = value;
    }
    for (size_t i = 0; i < count; i++) {
        if (bound[i].object == NULL) {
            PyErr_Format(PyExc_TypeError, "%s() missing required argument '%s'", function, names[i]);
            return -1;
        }
    }
    return 0;
}

static PyObject *bridge_call(PyObject *module, PyObject *args, PyObject *kwargs,
                            const char *function, size_t count, const char **names,
                            const int *kinds, int result_kind, void (*invoke)(BridgeArg *)) {
    (void)module;
    if (PyInterpreterState_Get() != PyInterpreterState_Main()
        || bridge_owner != PyThreadState_GetID(PyThreadState_Get())) {
        PyErr_SetString(PyExc_RuntimeError, "PyRs extensions currently require their importing thread in the main interpreter");
        return NULL;
    }
    if (bridge_busy) {
        PyErr_SetString(PyExc_RuntimeError, "reentrant PyRs extension calls are not supported yet");
        return NULL;
    }
    BridgeArg *bound = PyMem_Calloc(count + 1, sizeof(*bound));
    if (bound == NULL) return PyErr_NoMemory();
    if (bridge_bind(bound, args, kwargs, function, count, names) < 0) {
        PyMem_Free(bound);
        return NULL;
    }
    bridge_busy = 1;
    PyrsGcRoot root;
    pyrs_gc_root_push(&root, bound, (count + 1) * sizeof(*bound));
    PyrsExcFrame *frame = pyrs_try_push();
    /* A heap argument/result array survives longjmp and explicitly roots every
     * native value. Only the result local changes across the setjmp boundary. */
    PyObject *volatile result = NULL;
    if (setjmp(frame->buf) == 0) {
        size_t i;
        for (i = 0; i < count; i++) {
            if (bridge_convert(&bound[i], kinds[i], function, names[i]) < 0) break;
        }
        if (i == count) {
            invoke(bound);
            result = bridge_result(&bound[count], result_kind);
        }
    } else {
        PyErr_SetString(bridge_exception(g_exc_type), exc_msg_body(g_exc_msg));
    }
    pyrs_try_pop();
    pyrs_exc_clear();
    pyrs_gc_root_pop(&root);
    for (size_t i = 0; i <= count; i++) {
        if (bound[i].view.obj != NULL) PyBuffer_Release(&bound[i].view);
        Py_XDECREF(bound[i].temporary);
        PyMem_Free(bound[i].owned);
    }
    PyMem_Free(bound);
    bridge_busy = 0;
    return result;
}
