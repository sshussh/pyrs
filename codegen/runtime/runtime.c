/* PyRs runtime: tiny C support library linked into every compiled program.
 *
 * Printing matches CPython:
 * - floats use the shortest representation that round-trips, and whole
 *   floats keep their ".0" (1.0 prints as "1.0", not "1")
 * - bools print True/False; lists/tuples/dicts/sets print like CPython
 * - runtime errors (ZeroDivisionError, IndexError, ...) print to stderr
 *   and exit(1), unless a try-frame is active (then longjmp to handler)
 *
 * Heap objects use the nonmoving tracing collector in gc.c.  Payload
 * addresses remain stable because generated LLVM and runtime slots expose raw
 * pointers; object-owned native buffers are released with their owner.
 *
 * Slot tags (shared list/tuple/dict/set): 0=int 1=float 2=bool 3=str
 * 4+8*inner = nested list, 5 = tuple (self-describing), 6 = dict,
 * 7 = set.
 */

#include <errno.h>
#include <limits.h>
#include <math.h>
#include <setjmp.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include "gc.h"

/* exception type tags — keep in sync with ir::ExcType. Matching uses
 * CPython-like subclass checks via pyrs_exc_matches (Exception base,
 * OSError hierarchy). OTHER is catchable by bare `except:` and by
 * `except Exception:` (not by leaf types like RuntimeError). */
#define PYRS_EXC_VALUE 1
#define PYRS_EXC_KEY 2
#define PYRS_EXC_INDEX 3
#define PYRS_EXC_ZERODIV 4
#define PYRS_EXC_TYPE 5
#define PYRS_EXC_RUNTIME 6
#define PYRS_EXC_GENEXIT 7
#define PYRS_EXC_OVERFLOW 8
#define PYRS_EXC_EOF 9
#define PYRS_EXC_FILENOTFOUND 10
#define PYRS_EXC_OS 11
#define PYRS_EXC_NAME 12
#define PYRS_EXC_UNBOUNDLOCAL 13
#define PYRS_EXC_STOPITER 14
#define PYRS_EXC_EXCEPTION 15
#define PYRS_EXC_PERMISSION 16
#define PYRS_EXC_ISADIR 17
#define PYRS_EXC_ASSERT 18
#define PYRS_EXC_OTHER 99

/* value tags for heterogeneous containers */
#define TAG_INT 0
#define TAG_FLOAT 1
#define TAG_BOOL 2
#define TAG_STR 3
#define TAG_TUPLE 5
#define TAG_DICT 6
#define TAG_SET 7
/* heap box for union/Optional values in containers: { i32 print_tag, i64 payload }
 * print_tag = -1 means None; otherwise a normal TAG_* for the active member. */
#define TAG_UNION 8
#define TAG_CLOSURE 9
#define TAG_GENERATOR 10
#define TAG_EXC 11
/* Class instances: 13 + 8*class_id (distinct per class; avoids list 4+8*k). */
#define TAG_CLASS_BASE 13
/* list tags: 4 + 8 * elem_tag */

/* Class display names filled by compiled main (optional; null → "<object>"). */
static const char **g_class_names = NULL;
static long long g_class_n = 0;

void pyrs_set_class_names(const char **names, long long n) {
    g_class_names = names;
    g_class_n = n;
}

void pyrs_print_class_instance(void *obj) {
    if (obj == NULL) {
        fputs("<object>", stdout);
        return;
    }
    long long tid = *(long long *)obj;
    if (g_class_names != NULL && tid >= 0 && tid < g_class_n && g_class_names[tid] != NULL) {
        fputc('<', stdout);
        fputs(g_class_names[tid], stdout);
        fputs(" object>", stdout);
        return;
    }
    fputs("<object>", stdout);
}

/* layout shared with codegen: leading i64 length, then bytes (+ NUL) */
typedef struct {
    long long len;
    char data[];
} PyrsStr;

/* GC-managed first-class exception instance bound by `except E as e`. */
typedef struct {
    int type_tag;
    PyrsStr *msg;
} PyrsExc;

/* stable header; data grows by reallocation */
typedef struct {
    long long len;
    long long cap;
    long long *data;
} PyrsList;

/* used by the str splitters before the list section defines them */
PyrsList *pyrs_list_new(long long cap);
void pyrs_list_push(PyrsList *l, long long slot);
PyrsList *pyrs_list_copy(const PyrsList *src);

/* ---- exceptions (setjmp/longjmp try frames) ----
 * Single-threaded process-global state (PyRs programs are not multi-threaded). */

struct PyrsExcFrame;

typedef void (*PyrsCleanupFn)(void *context);

/* Native runtime temporaries cannot rely on ordinary C cleanup after a
 * longjmp.  A small per-try LIFO lets helpers register stack-owned cleanup
 * records while their malloc-backed scratch state is live. */
typedef struct PyrsCleanup {
    PyrsCleanupFn run;
    void *context;
    struct PyrsCleanup *prev;
    struct PyrsExcFrame *frame;
} PyrsCleanup;

typedef struct PyrsExcFrame {
    jmp_buf buf;
    struct PyrsExcFrame *prev;
    PyrsCleanup *cleanups;
    /* setjmp saves registers outside the native stack. Generated functions
     * containing a try reserve the native frame-pointer register, because
     * glibc pointer-mangles that jmp_buf slot; the other saved value registers
     * and this complete frame remain visible to conservative root discovery. */
    PyrsGcRoot gc_root;
} PyrsExcFrame;

static PyrsExcFrame *g_exc_frames = NULL;
static int g_exc_type = 0;
static char g_exc_msg[512];

static void *xmalloc(size_t n);

static void pyrs_cleanup_push(PyrsCleanup *cleanup, PyrsCleanupFn run, void *context) {
    cleanup->run = run;
    cleanup->context = context;
    cleanup->prev = NULL;
    cleanup->frame = g_exc_frames;
    if (cleanup->frame != NULL) {
        cleanup->prev = cleanup->frame->cleanups;
        cleanup->frame->cleanups = cleanup;
    }
}

static void pyrs_cleanup_pop(PyrsCleanup *cleanup) {
    PyrsExcFrame *frame = cleanup->frame;
    if (frame == NULL) {
        return;
    }
    if (frame->cleanups != cleanup) {
        fputs("RuntimeError: native cleanup stack corrupted\n", stderr);
        abort();
    }
    frame->cleanups = cleanup->prev;
    cleanup->frame = NULL;
}

static void pyrs_run_cleanups(PyrsExcFrame *frame) {
    while (frame->cleanups != NULL) {
        PyrsCleanup *cleanup = frame->cleanups;
        frame->cleanups = cleanup->prev;
        cleanup->frame = NULL;
        cleanup->run(cleanup->context);
    }
}

static _Noreturn void pyrs_jump_current(void) {
    PyrsExcFrame *frame = g_exc_frames;
    pyrs_run_cleanups(frame);
    longjmp(frame->buf, 1);
}

static const char *exc_type_name(int ty) {
    switch (ty) {
    case PYRS_EXC_VALUE:
        return "ValueError";
    case PYRS_EXC_KEY:
        return "KeyError";
    case PYRS_EXC_INDEX:
        return "IndexError";
    case PYRS_EXC_ZERODIV:
        return "ZeroDivisionError";
    case PYRS_EXC_TYPE:
        return "TypeError";
    case PYRS_EXC_RUNTIME:
        return "RuntimeError";
    case PYRS_EXC_GENEXIT:
        return "GeneratorExit";
    case PYRS_EXC_OVERFLOW:
        return "OverflowError";
    case PYRS_EXC_EOF:
        return "EOFError";
    case PYRS_EXC_FILENOTFOUND:
        return "FileNotFoundError";
    case PYRS_EXC_OS:
        return "OSError";
    case PYRS_EXC_NAME:
        return "NameError";
    case PYRS_EXC_UNBOUNDLOCAL:
        return "UnboundLocalError";
    case PYRS_EXC_STOPITER:
        return "StopIteration";
    case PYRS_EXC_EXCEPTION:
        return "Exception";
    case PYRS_EXC_PERMISSION:
        return "PermissionError";
    case PYRS_EXC_ISADIR:
        return "IsADirectoryError";
    case PYRS_EXC_ASSERT:
        return "AssertionError";
    default:
        return "Exception";
    }
}

static int classify_exc_msg(const char *msg) {
    /* Longer / more-specific prefixes first where they share a head. */
    if (strncmp(msg, "ZeroDivisionError", 17) == 0) {
        return PYRS_EXC_ZERODIV;
    }
    if (strncmp(msg, "UnboundLocalError", 17) == 0) {
        return PYRS_EXC_UNBOUNDLOCAL;
    }
    if (strncmp(msg, "IsADirectoryError", 17) == 0) {
        return PYRS_EXC_ISADIR;
    }
    if (strncmp(msg, "FileNotFoundError", 17) == 0) {
        return PYRS_EXC_FILENOTFOUND;
    }
    if (strncmp(msg, "PermissionError", 15) == 0) {
        return PYRS_EXC_PERMISSION;
    }
    if (strncmp(msg, "StopIteration", 13) == 0) {
        return PYRS_EXC_STOPITER;
    }
    if (strncmp(msg, "OverflowError", 13) == 0) {
        return PYRS_EXC_OVERFLOW;
    }
    if (strncmp(msg, "RuntimeError", 12) == 0) {
        return PYRS_EXC_RUNTIME;
    }
    if (strncmp(msg, "GeneratorExit", 13) == 0) {
        return PYRS_EXC_GENEXIT;
    }
    if (strncmp(msg, "ValueError", 10) == 0) {
        return PYRS_EXC_VALUE;
    }
    if (strncmp(msg, "IndexError", 10) == 0) {
        return PYRS_EXC_INDEX;
    }
    if (strncmp(msg, "TypeError", 9) == 0) {
        return PYRS_EXC_TYPE;
    }
    if (strncmp(msg, "Exception", 9) == 0) {
        return PYRS_EXC_EXCEPTION;
    }
    if (strncmp(msg, "NameError", 9) == 0) {
        return PYRS_EXC_NAME;
    }
    if (strncmp(msg, "KeyError", 8) == 0) {
        return PYRS_EXC_KEY;
    }
    if (strncmp(msg, "EOFError", 8) == 0) {
        return PYRS_EXC_EOF;
    }
    if (strncmp(msg, "OSError", 7) == 0) {
        return PYRS_EXC_OS;
    }
    if (strncmp(msg, "AssertionError", 14) == 0) {
        return PYRS_EXC_ASSERT;
    }
    /* MemoryError and other untyped traps — bare except / Exception. */
    return PYRS_EXC_OTHER;
}

/* CPython-like subclass check for except filters / isinstance(exc, T). */
int pyrs_exc_matches(int filter, int actual) {
    if (filter == actual) {
        return 1;
    }
    /* Exception catches everything under Exception, not GeneratorExit
     * (BaseException-only) and not an empty/unset tag. */
    if (filter == PYRS_EXC_EXCEPTION) {
        return actual != PYRS_EXC_GENEXIT && actual != 0;
    }
    /* OSError catches FileNotFoundError / PermissionError / IsADirectoryError. */
    if (filter == PYRS_EXC_OS) {
        return actual == PYRS_EXC_FILENOTFOUND || actual == PYRS_EXC_PERMISSION ||
               actual == PYRS_EXC_ISADIR;
    }
    return 0;
}

/* strip "Type: " prefix for the bound exception message */
static const char *exc_msg_body(const char *full) {
    const char *colon = strchr(full, ':');
    if (colon != NULL && colon[1] == ' ') {
        return colon + 2;
    }
    return full;
}

_Noreturn static void die_uncaught(const char *msg) {
    fflush(stdout);
    fputs(msg, stderr);
    fputc('\n', stderr);
    exit(1);
}

_Noreturn void pyrs_raise(int type, const char *msg) {
    g_exc_type = type;
    snprintf(g_exc_msg, sizeof g_exc_msg, "%s: %s", exc_type_name(type), msg ? msg : "");
    if (g_exc_frames != NULL) {
        pyrs_jump_current();
    }
    die_uncaught(g_exc_msg);
}

_Noreturn void pyrs_die(const char *msg) {
    int ty = classify_exc_msg(msg);
    g_exc_type = ty;
    snprintf(g_exc_msg, sizeof g_exc_msg, "%s", msg);
    if (g_exc_frames != NULL) {
        pyrs_jump_current();
    }
    die_uncaught(msg);
}

static void *xmalloc(size_t n) {
    void *p = malloc(n);
    if (p == NULL) {
        /* Native scratch/backing allocation also gets one chance to reclaim
         * unreachable managed owners before treating OOM as fatal. */
        pyrs_gc_collect();
        p = malloc(n);
    }
    if (p == NULL) {
        /* bypass catch frames — OOM is fatal */
        fflush(stdout);
        fputs("MemoryError: out of memory\n", stderr);
        exit(1);
    }
    return p;
}

PyrsExcFrame *pyrs_try_push(void) {
    PyrsExcFrame *f = xmalloc(sizeof(PyrsExcFrame));
    f->prev = g_exc_frames;
    f->cleanups = NULL;
    pyrs_gc_root_push(&f->gc_root, f, sizeof(*f));
    g_exc_frames = f;
    return f;
}

/* Note: do not wrap setjmp in a C function — longjmp must restore to the
 * LLVM call site of setjmp (jmp_buf is the first field of PyrsExcFrame). */

void pyrs_try_pop(void) {
    if (g_exc_frames != NULL) {
        PyrsExcFrame *old = g_exc_frames;
        g_exc_frames = old->prev;
        pyrs_gc_root_pop(&old->gc_root);
        free(old);
    }
}

int pyrs_exc_type(void) {
    return g_exc_type;
}

/* message body only (no "Type: " prefix), as a PyrsStr */
PyrsStr *pyrs_exc_message(void) {
    const char *body = exc_msg_body(g_exc_msg);
    size_t n = strlen(body);
    PyrsStr *s = pyrs_gc_alloc(sizeof(long long) + n + 1, PYRS_GC_STRING);
    s->len = (long long)n;
    memcpy(s->data, body, n + 1);
    return s;
}

/* Build a first-class exception object from the active pending exception. */
PyrsExc *pyrs_exc_object(void) {
    PyrsExc *e = pyrs_gc_alloc(sizeof(PyrsExc), PYRS_GC_EXCEPTION);
    e->type_tag = g_exc_type;
    e->msg = pyrs_exc_message();
    return e;
}

/* print(e) / str(e) → message body only (CPython). */
void pyrs_print_exc(PyrsExc *e) {
    if (e == NULL || e->msg == NULL) {
        return;
    }
    fwrite(e->msg->data, 1, (size_t)e->msg->len, stdout);
}

PyrsStr *pyrs_str_from_exc(PyrsExc *e) {
    if (e == NULL || e->msg == NULL) {
        PyrsStr *s =
            pyrs_gc_alloc(sizeof(long long) + 1, PYRS_GC_STRING);
        s->len = 0;
        s->data[0] = '\0';
        return s;
    }
    return e->msg;
}

/* e.args as list[str] (empty or one message) — list ABI for variable length. */
PyrsList *pyrs_exc_args(PyrsExc *e) {
    if (e == NULL || e->msg == NULL || e->msg->len == 0) {
        return pyrs_list_new(0);
    }
    PyrsList *r = pyrs_list_new(1);
    pyrs_list_push(r, (long long)(uintptr_t)e->msg);
    return r;
}

/* repr(e) → "ExcType('msg')" like CPython (simplified). */
PyrsStr *pyrs_repr_from_exc(PyrsExc *e) {
    const char *name = e ? exc_type_name(e->type_tag) : "Exception";
    const char *body =
        (e && e->msg && e->msg->len > 0) ? e->msg->data : "";
    char buf[640];
    if (body[0] == '\0') {
        snprintf(buf, sizeof buf, "%s()", name);
    } else {
        snprintf(buf, sizeof buf, "%s('%s')", name, body);
    }
    size_t n = strlen(buf);
    PyrsStr *s = pyrs_gc_alloc(sizeof(long long) + n + 1, PYRS_GC_STRING);
    s->len = (long long)n;
    memcpy(s->data, buf, n + 1);
    return s;
}

/* isinstance(exc, filter_tag) with hierarchy. */
int pyrs_exc_isinstance(PyrsExc *e, int filter) {
    if (e == NULL) {
        return 0;
    }
    return pyrs_exc_matches(filter, e->type_tag);
}

void pyrs_exc_clear(void) {
    g_exc_type = 0;
    g_exc_msg[0] = '\0';
}

/* Set pending exception without longjmp (used so except-handlers can
 * still run their try's finally before re-raising). */
void pyrs_set_exc(int type, const char *msg) {
    g_exc_type = type;
    snprintf(g_exc_msg, sizeof g_exc_msg, "%s: %s", exc_type_name(type), msg ? msg : "");
}

/* Like pyrs_set_exc but `msg` is already a full "Type: body" or bare body
 * from a die string — classify and store. */
void pyrs_set_exc_msg(const char *msg) {
    g_exc_type = classify_exc_msg(msg);
    snprintf(g_exc_msg, sizeof g_exc_msg, "%s", msg ? msg : "RuntimeError");
}

/* re-raise the current exception (no active frame → print and exit) */
_Noreturn void pyrs_reraise(void) {
    if (g_exc_frames != NULL) {
        pyrs_jump_current();
    }
    die_uncaught(g_exc_msg[0] ? g_exc_msg : "RuntimeError: unknown error");
}

/* Raise from a first-class exception object (after exc_clear in a handler). */
_Noreturn void pyrs_raise_exc(PyrsExc *e) {
    if (e == NULL) {
        pyrs_raise(PYRS_EXC_RUNTIME, "unknown error");
    }
    g_exc_type = e->type_tag;
    const char *body =
        (e->msg != NULL && e->msg->data != NULL) ? e->msg->data : "";
    snprintf(g_exc_msg, sizeof g_exc_msg, "%s: %s", exc_type_name(e->type_tag), body);
    if (g_exc_frames != NULL) {
        pyrs_jump_current();
    }
    die_uncaught(g_exc_msg);
}

/* zero-initialized (null) str/list locals read before assignment land here
 * instead of segfaulting */
static void check_ref(const void *p) {
    if (p == NULL) {
        pyrs_die("UnboundLocalError: value used before assignment");
    }
}

static PyrsStr *str_alloc(long long len) {
    PyrsStr *s = pyrs_gc_alloc(sizeof(long long) + (size_t)len + 1,
                              PYRS_GC_STRING);
    s->len = len;
    s->data[len] = '\0';
    return s;
}

/* ---- printing ---- */

/* CPython repr: the fewest significant digits that round-trip, printed in
 * fixed notation when the decimal exponent is in [-4, 16) and scientific
 * otherwise (10.0 -> "10.0", 1e16 -> "1e+16", 1e-05 -> "1e-05");
 * buf must hold >= 40 bytes */
static void format_double(double v, char *buf) {
    if (isnan(v)) {
        strcpy(buf, "nan");
        return;
    }
    if (isinf(v)) {
        strcpy(buf, v < 0 ? "-inf" : "inf");
        return;
    }
    char sci[40];
    int digits = 17;
    for (int d = 1; d <= 17; d++) {
        snprintf(sci, sizeof sci, "%.*e", d - 1, v);
        if (strtod(sci, NULL) == v) {
            digits = d;
            break;
        }
    }
    int exp = atoi(strchr(sci, 'e') + 1);
    if (-4 <= exp && exp < 16) {
        int prec = digits - 1 - exp;
        if (prec < 0) {
            prec = 0;
        }
        snprintf(buf, 40, "%.*f", prec, v);
        if (strchr(buf, '.') == NULL) {
            strcat(buf, ".0");
        }
    } else {
        memcpy(buf, sci, strlen(sci) + 1);
    }
}

/* ---- arbitrary-precision int (tagged i64) ----
 * LSB=1 → small: value = tagged >> 1 (signed, range ±2^62)
 * LSB=0 → pointer to a GC-managed heap PyrsInt
 * Zero is always the small tag 1 (((0)<<1)|1).
 *
 * This file is #include'd into runtime.c (not compiled standalone).
 */

typedef struct PyrsInt {
    int sign; /* -1, 0, +1 */
    long long nlimbs;
    unsigned long long *limbs; /* little-endian base 2^64 */
} PyrsInt;

#define PYRS_SMALL_MIN (-(1LL << 62))
#define PYRS_SMALL_MAX ((1LL << 62) - 1)
#define PYRS_LIMB_BITS 64

static int pyrs_int_is_small(long long t) {
    return ((unsigned long long)t & 1ULL) != 0ULL;
}

static long long pyrs_int_small_val(long long t) {
    return t >> 1;
}

static long long pyrs_int_tag_small(long long v) {
    return (long long)(((unsigned long long)v << 1) | 1ULL);
}

static PyrsInt *pyrs_int_heap_ptr(long long t) {
    return (PyrsInt *)(uintptr_t)t;
}

static void int_trim(unsigned long long *limbs, long long *nlimbs) {
    while (*nlimbs > 0 && limbs[*nlimbs - 1] == 0ULL) {
        (*nlimbs)--;
    }
}

static long long int_from_sign_limbs(int sign, unsigned long long *limbs,
                                     long long nlimbs) {
    int_trim(limbs, &nlimbs);
    if (nlimbs == 0 || sign == 0) {
        free(limbs);
        return pyrs_int_tag_small(0);
    }
    if (nlimbs == 1) {
        unsigned long long mag = limbs[0];
        if (sign > 0 && mag <= (unsigned long long)PYRS_SMALL_MAX) {
            free(limbs);
            return pyrs_int_tag_small((long long)mag);
        }
        if (sign < 0 && mag <= (unsigned long long)(-PYRS_SMALL_MIN)) {
            free(limbs);
            return pyrs_int_tag_small(-(long long)mag);
        }
    }
    PyrsInt *h = pyrs_gc_alloc(sizeof(PyrsInt), PYRS_GC_BIGINT);
    h->sign = sign > 0 ? 1 : -1;
    h->nlimbs = nlimbs;
    h->limbs = limbs;
    if ((unsigned long long)nlimbs <= SIZE_MAX / sizeof(*limbs)) {
        pyrs_gc_external_allocated(h, (size_t)nlimbs * sizeof(*limbs));
    }
    return (long long)(uintptr_t)h;
}

/* Read magnitude; if *owned, caller frees the returned buffer. */
static unsigned long long *int_read_mag(long long t, int *sign, long long *n,
                                        int *owned) {
    if (pyrs_int_is_small(t)) {
        long long v = pyrs_int_small_val(t);
        if (v == 0) {
            *sign = 0;
            *n = 0;
            *owned = 0;
            return NULL;
        }
        *sign = v < 0 ? -1 : 1;
        *n = 1;
        *owned = 1;
        unsigned long long *p = xmalloc(sizeof(unsigned long long));
        p[0] = (unsigned long long)(v < 0 ? -v : v);
        return p;
    }
    PyrsInt *h = pyrs_int_heap_ptr(t);
    *sign = h->sign;
    *n = h->nlimbs;
    *owned = 0;
    return h->limbs;
}

static unsigned long long *int_copy_limbs(const unsigned long long *src,
                                          long long n) {
    if (n <= 0) {
        return NULL;
    }
    unsigned long long *p = xmalloc((size_t)n * sizeof(unsigned long long));
    memcpy(p, src, (size_t)n * sizeof(unsigned long long));
    return p;
}

long long pyrs_int_from_i64(long long v) {
    if (v >= PYRS_SMALL_MIN && v <= PYRS_SMALL_MAX) {
        return pyrs_int_tag_small(v);
    }
    unsigned long long *limbs = xmalloc(sizeof(unsigned long long));
    int sign;
    if (v < 0) {
        sign = -1;
        limbs[0] = (v == LLONG_MIN) ? (1ULL << 63) : (unsigned long long)(-v);
    } else {
        sign = (v == 0) ? 0 : 1;
        limbs[0] = (unsigned long long)v;
    }
    return int_from_sign_limbs(sign, limbs, 1);
}

long long pyrs_int_from_str(const char *s, long long len) {
    if (s == NULL || len <= 0) {
        return pyrs_int_tag_small(0);
    }
    long long i = 0;
    int sign = 1;
    if (s[i] == '+') {
        i++;
    } else if (s[i] == '-') {
        sign = -1;
        i++;
    }
    while (i < len && s[i] == '0') {
        i++;
    }
    if (i >= len) {
        return pyrs_int_tag_small(0);
    }
    unsigned long long *limbs = NULL;
    long long nlimbs = 0;
    long long cap = 0;
    for (; i < len; i++) {
        unsigned char c = (unsigned char)s[i];
        if (c < '0' || c > '9') {
            free(limbs);
            pyrs_die("ValueError: invalid literal for int()");
        }
        unsigned long long carry = (unsigned long long)(c - '0');
        for (long long k = 0; k < nlimbs; k++) {
            __uint128_t prod = (__uint128_t)limbs[k] * 10ULL + carry;
            limbs[k] = (unsigned long long)prod;
            carry = (unsigned long long)(prod >> 64);
        }
        if (carry) {
            if (nlimbs == cap) {
                long long ncap = cap == 0 ? 2 : cap * 2;
                unsigned long long *nl =
                    xmalloc((size_t)ncap * sizeof(unsigned long long));
                if (limbs) {
                    memcpy(nl, limbs,
                           (size_t)nlimbs * sizeof(unsigned long long));
                    free(limbs);
                }
                limbs = nl;
                cap = ncap;
            }
            limbs[nlimbs++] = carry;
        }
    }
    if (nlimbs == 0) {
        free(limbs);
        return pyrs_int_tag_small(0);
    }
    return int_from_sign_limbs(sign, limbs, nlimbs);
}

PyrsStr *pyrs_str_repr(const PyrsStr *s);

static int is_int_space(char c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' || c == '\f';
}

static int int_digit_val(unsigned char c) {
    if (c >= '0' && c <= '9') {
        return (int)(c - '0');
    }
    if (c >= 'a' && c <= 'z') {
        return (int)(c - 'a' + 10);
    }
    if (c >= 'A' && c <= 'Z') {
        return (int)(c - 'A' + 10);
    }
    return -1;
}

static void die_invalid_int_literal(const PyrsStr *s, long long base) {
    PyrsStr *r = pyrs_str_repr(s);
    char head[96];
    int hn = snprintf(head, sizeof head,
                      "ValueError: invalid literal for int() with base %lld: ",
                      base);
    if (hn < 0) {
        pyrs_die("ValueError: invalid literal for int()");
    }
    size_t n = (size_t)hn + (size_t)r->len + 1;
    char *buf = xmalloc(n);
    memcpy(buf, head, (size_t)hn);
    memcpy(buf + hn, r->data, (size_t)r->len);
    buf[hn + r->len] = '\0';
    pyrs_die(buf);
}

/* CPython `int(s[, base])` for a PyrsStr. `base_tag` is a tagged PyRs int. */
long long pyrs_int_from_pystr(const PyrsStr *s, long long base_tag) {
    check_ref(s);
    long long given_base;
    if (pyrs_int_is_small(base_tag)) {
        given_base = pyrs_int_small_val(base_tag);
    } else {
        pyrs_die("OverflowError: Python int too large to convert to C ssize_t");
    }
    if (given_base != 0 && (given_base < 2 || given_base > 36)) {
        pyrs_die("ValueError: int() base must be >= 2 and <= 36, or 0");
    }

    const char *p = s->data;
    long long n = s->len;
    long long i = 0;
    long long end = n;
    while (i < end && is_int_space(p[i])) {
        i++;
    }
    while (end > i && is_int_space(p[end - 1])) {
        end--;
    }
    if (i >= end) {
        die_invalid_int_literal(s, given_base);
    }

    int sign = 1;
    if (p[i] == '+') {
        i++;
    } else if (p[i] == '-') {
        sign = -1;
        i++;
    }
    if (i >= end) {
        die_invalid_int_literal(s, given_base);
    }

    int saw_prefix = 0;
    long long effective = given_base;
    if (end - i >= 2 && p[i] == '0') {
        char k = p[i + 1];
        int pref = 0;
        if (k == 'x' || k == 'X') {
            pref = 16;
        } else if (k == 'b' || k == 'B') {
            pref = 2;
        } else if (k == 'o' || k == 'O') {
            pref = 8;
        }
        if (pref && (given_base == 0 || given_base == pref)) {
            effective = pref;
            i += 2;
            saw_prefix = 1;
        }
    }
    if (given_base == 0 && !saw_prefix) {
        effective = 10;
        if (p[i] == '0') {
            for (long long j = i + 1; j < end; j++) {
                if (p[j] == '_') {
                    continue;
                }
                if (p[j] != '0') {
                    die_invalid_int_literal(s, given_base);
                }
            }
        }
    }
    if (effective == 0) {
        effective = 10;
    }

    unsigned long long *limbs = NULL;
    long long nlimbs = 0;
    long long cap = 0;
    int got_digit = 0;
    int last_us = 0;
    int allow_us = saw_prefix;
    for (; i < end; i++) {
        unsigned char c = (unsigned char)p[i];
        if (c == '_') {
            if (last_us || (!got_digit && !allow_us)) {
                free(limbs);
                die_invalid_int_literal(s, given_base);
            }
            last_us = 1;
            allow_us = 0;
            continue;
        }
        allow_us = 0;
        last_us = 0;
        int d = int_digit_val(c);
        if (d < 0 || (long long)d >= effective) {
            free(limbs);
            die_invalid_int_literal(s, given_base);
        }
        got_digit = 1;
        unsigned long long carry = (unsigned long long)d;
        for (long long k = 0; k < nlimbs; k++) {
            __uint128_t prod =
                (__uint128_t)limbs[k] * (unsigned long long)effective + carry;
            limbs[k] = (unsigned long long)prod;
            carry = (unsigned long long)(prod >> 64);
        }
        if (carry) {
            if (nlimbs == cap) {
                long long ncap = cap == 0 ? 2 : cap * 2;
                unsigned long long *nl =
                    xmalloc((size_t)ncap * sizeof(unsigned long long));
                if (limbs) {
                    memcpy(nl, limbs, (size_t)nlimbs * sizeof(unsigned long long));
                    free(limbs);
                }
                limbs = nl;
                cap = ncap;
            }
            limbs[nlimbs++] = carry;
        }
    }
    if (last_us || !got_digit) {
        free(limbs);
        die_invalid_int_literal(s, given_base);
    }
    if (nlimbs == 0) {
        free(limbs);
        return pyrs_int_tag_small(0);
    }
    return int_from_sign_limbs(sign, limbs, nlimbs);
}

static void die_invalid_float_literal(const PyrsStr *s) {
    PyrsStr *r = pyrs_str_repr(s);
    const char *head = "ValueError: could not convert string to float: ";
    size_t hn = strlen(head);
    size_t n = hn + (size_t)r->len + 1;
    char *buf = xmalloc(n);
    memcpy(buf, head, hn);
    memcpy(buf + hn, r->data, (size_t)r->len);
    buf[hn + r->len] = '\0';
    pyrs_die(buf);
}

static int ascii_ieq(const char *p, long long n, const char *lit) {
    long long i = 0;
    for (; lit[i] != '\0'; i++) {
        if (i >= n) {
            return 0;
        }
        char c = p[i];
        if (c >= 'A' && c <= 'Z') {
            c = (char)(c + 32);
        }
        char d = lit[i];
        if (d >= 'A' && d <= 'Z') {
            d = (char)(d + 32);
        }
        if (c != d) {
            return 0;
        }
    }
    return i == n;
}

static int is_dec_digit(char c) {
    return c >= '0' && c <= '9';
}

static int float_syntax_ok(const char *b, long long n) {
    long long j = 0;
    int got_digit = 0;
    while (j < n && is_dec_digit(b[j])) {
        got_digit = 1;
        j++;
    }
    if (j < n && b[j] == '.') {
        j++;
        while (j < n && is_dec_digit(b[j])) {
            got_digit = 1;
            j++;
        }
    }
    if (!got_digit) {
        return 0;
    }
    if (j < n && (b[j] == 'e' || b[j] == 'E')) {
        j++;
        if (j < n && (b[j] == '+' || b[j] == '-')) {
            j++;
        }
        int exp_digit = 0;
        while (j < n && is_dec_digit(b[j])) {
            exp_digit = 1;
            j++;
        }
        if (!exp_digit) {
            return 0;
        }
    }
    return j == n;
}

double pyrs_float_from_pystr(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    long long n = s->len;
    long long i = 0;
    long long end = n;
    while (i < end && is_int_space(p[i])) {
        i++;
    }
    while (end > i && is_int_space(p[end - 1])) {
        end--;
    }
    if (i >= end) {
        die_invalid_float_literal(s);
    }
    int sign = 1;
    if (p[i] == '+') {
        i++;
    } else if (p[i] == '-') {
        sign = -1;
        i++;
    }
    if (i >= end) {
        die_invalid_float_literal(s);
    }
    if (ascii_ieq(p + i, end - i, "inf") || ascii_ieq(p + i, end - i, "infinity")) {
        return sign < 0 ? -INFINITY : INFINITY;
    }
    if (ascii_ieq(p + i, end - i, "nan")) {
        return NAN;
    }

    char *buf = xmalloc((size_t)(end - i) + 1);
    long long blen = 0;
    for (long long j = i; j < end; j++) {
        if (p[j] == '_') {
            if (j == i || j + 1 >= end || !is_dec_digit(p[j - 1]) ||
                !is_dec_digit(p[j + 1])) {
                free(buf);
                die_invalid_float_literal(s);
            }
            continue;
        }
        buf[blen++] = p[j];
    }
    buf[blen] = '\0';
    if (!float_syntax_ok(buf, blen)) {
        free(buf);
        die_invalid_float_literal(s);
    }
    char *endp = NULL;
    errno = 0;
    double v = strtod(buf, &endp);
    if (endp != buf + blen) {
        free(buf);
        die_invalid_float_literal(s);
    }
    free(buf);
    if (sign < 0) {
        v = -v;
    }
    return v;
}

long long pyrs_int_as_i64(long long t) {
    if (pyrs_int_is_small(t)) {
        return pyrs_int_small_val(t);
    }
    PyrsInt *h = pyrs_int_heap_ptr(t);
    if (h->sign == 0 || h->nlimbs == 0) {
        return 0;
    }
    if (h->nlimbs > 1) {
        pyrs_die("OverflowError: Python int too large to convert to C long");
    }
    unsigned long long mag = h->limbs[0];
    if (h->sign > 0) {
        if (mag > (unsigned long long)LLONG_MAX) {
            pyrs_die("OverflowError: Python int too large to convert to C long");
        }
        return (long long)mag;
    }
    if (mag > (unsigned long long)LLONG_MAX + 1ULL) {
        pyrs_die("OverflowError: Python int too large to convert to C long");
    }
    if (mag == (unsigned long long)LLONG_MAX + 1ULL) {
        return LLONG_MIN;
    }
    return -(long long)mag;
}

double pyrs_int_to_float(long long t) {
    if (pyrs_int_is_small(t)) {
        return (double)pyrs_int_small_val(t);
    }
    PyrsInt *h = pyrs_int_heap_ptr(t);
    if (h->sign == 0 || h->nlimbs == 0) {
        return 0.0;
    }
    /* Build from high limb for better rounding (CPython uses similar). */
    double acc = 0.0;
    for (long long i = h->nlimbs - 1; i >= 0; i--) {
        acc = acc * 18446744073709551616.0 + (double)h->limbs[i];
    }
    return h->sign < 0 ? -acc : acc;
}

long long pyrs_int_from_float(double v) {
    if (isnan(v)) {
        pyrs_die("ValueError: cannot convert float NaN to integer");
    }
    if (isinf(v)) {
        pyrs_die("OverflowError: cannot convert float infinity to integer");
    }
    int neg = signbit(v) && v != 0.0;
    if (v < 0) {
        v = -v;
    }
    /* truncate toward zero already by using floor on magnitude of |v| for
     * positive path; for original negative, trunc toward 0 is ceil of -|v|
     * which is -floor(|v|). */
    v = floor(v); /* |v| truncated toward 0 for non-negative original */
    if (v == 0.0) {
        return pyrs_int_tag_small(0);
    }
    if (!neg && v <= (double)PYRS_SMALL_MAX && v >= 0.0) {
        return pyrs_int_tag_small((long long)v);
    }
    if (neg && v <= (double)(-PYRS_SMALL_MIN)) {
        return pyrs_int_tag_small(-(long long)v);
    }
    /* Convert large finite float: extract limbs via repeated mod 2^64. */
    const double B = 18446744073709551616.0;
    long long cap = 4;
    unsigned long long *limbs =
        xmalloc((size_t)cap * sizeof(unsigned long long));
    long long n = 0;
    while (v >= 1.0) {
        if (n == cap) {
            long long ncap = cap * 2;
            unsigned long long *nl =
                xmalloc((size_t)ncap * sizeof(unsigned long long));
            memcpy(nl, limbs, (size_t)n * sizeof(unsigned long long));
            free(limbs);
            limbs = nl;
            cap = ncap;
        }
        double hi = floor(v / B);
        double lo = v - hi * B;
        if (lo < 0) {
            lo = 0;
        }
        limbs[n++] = (unsigned long long)lo;
        v = hi;
    }
    return int_from_sign_limbs(neg ? -1 : 1, limbs, n);
}

static int u_cmp(const unsigned long long *a, long long na,
                 const unsigned long long *b, long long nb) {
    if (na != nb) {
        return na < nb ? -1 : 1;
    }
    for (long long i = na - 1; i >= 0; i--) {
        if (a[i] != b[i]) {
            return a[i] < b[i] ? -1 : 1;
        }
    }
    return 0;
}

int pyrs_int_cmp(long long a, long long b) {
    if (pyrs_int_is_small(a) && pyrs_int_is_small(b)) {
        long long av = pyrs_int_small_val(a);
        long long bv = pyrs_int_small_val(b);
        return (av > bv) - (av < bv);
    }
    int sa, sb, oa, ob;
    long long na, nb;
    unsigned long long *da = int_read_mag(a, &sa, &na, &oa);
    unsigned long long *db = int_read_mag(b, &sb, &nb, &ob);
    int r;
    if (sa != sb) {
        r = sa < sb ? -1 : 1;
    } else if (sa == 0) {
        r = 0;
    } else {
        int uc = u_cmp(da, na, db, nb);
        r = sa < 0 ? -uc : uc;
    }
    if (oa) {
        free(da);
    }
    if (ob) {
        free(db);
    }
    return r;
}

int pyrs_int_eq(long long a, long long b) {
    if (a == b) {
        return 1;
    }
    /* two heap pointers or mixed: content equality */
    return pyrs_int_cmp(a, b) == 0;
}

int pyrs_int_truth(long long a) {
    if (pyrs_int_is_small(a)) {
        return pyrs_int_small_val(a) != 0;
    }
    PyrsInt *h = pyrs_int_heap_ptr(a);
    return h->sign != 0 && h->nlimbs > 0;
}

unsigned long long pyrs_int_hash(long long a) {
    /* Hash on mathematical value so small and heap equal values collide. */
    if (pyrs_int_is_small(a)) {
        long long v = pyrs_int_small_val(a);
        unsigned long long x = (unsigned long long)v;
        x ^= x >> 30;
        x *= 0xbf58476d1ce4e5b9ULL;
        x ^= x >> 27;
        x *= 0x94d049bb133111ebULL;
        x ^= x >> 31;
        return x;
    }
    PyrsInt *h = pyrs_int_heap_ptr(a);
    unsigned long long x = 0x9e3779b97f4a7c15ULL;
    for (long long i = 0; i < h->nlimbs; i++) {
        x ^= h->limbs[i] + 0x9e3779b97f4a7c15ULL + (x << 6) + (x >> 2);
    }
    if (h->sign < 0) {
        x = ~x;
    }
    x ^= x >> 30;
    x *= 0xbf58476d1ce4e5b9ULL;
    x ^= x >> 27;
    x *= 0x94d049bb133111ebULL;
    x ^= x >> 31;
    return x;
}

static unsigned long long *u_add(const unsigned long long *a, long long na,
                                 const unsigned long long *b, long long nb,
                                 long long *rn) {
    long long n = na > nb ? na : nb;
    unsigned long long *r =
        xmalloc((size_t)(n + 1) * sizeof(unsigned long long));
    unsigned long long carry = 0;
    for (long long i = 0; i < n; i++) {
        unsigned long long av = i < na ? a[i] : 0;
        unsigned long long bv = i < nb ? b[i] : 0;
        __uint128_t s = (__uint128_t)av + bv + carry;
        r[i] = (unsigned long long)s;
        carry = (unsigned long long)(s >> 64);
    }
    r[n] = carry;
    *rn = n + (carry ? 1 : 0);
    return r;
}

static unsigned long long *u_sub(const unsigned long long *a, long long na,
                                 const unsigned long long *b, long long nb,
                                 long long *rn) {
    /* assume a >= b */
    unsigned long long *r = xmalloc((size_t)na * sizeof(unsigned long long));
    unsigned long long borrow = 0;
    for (long long i = 0; i < na; i++) {
        unsigned long long bv = i < nb ? b[i] : 0;
        unsigned long long av = a[i];
        unsigned long long tmp = av - borrow;
        unsigned long long borrow1 = av < borrow;
        borrow = borrow1 || (tmp < bv);
        r[i] = tmp - bv;
    }
    *rn = na;
    int_trim(r, rn);
    return r;
}

static long long int_add_signed(int sa, const unsigned long long *a, long long na,
                                int sb, const unsigned long long *b, long long nb) {
    if (sa == 0) {
        return int_from_sign_limbs(sb, int_copy_limbs(b, nb), nb);
    }
    if (sb == 0) {
        return int_from_sign_limbs(sa, int_copy_limbs(a, na), na);
    }
    if (sa == sb) {
        long long rn;
        unsigned long long *r = u_add(a, na, b, nb, &rn);
        return int_from_sign_limbs(sa, r, rn);
    }
    /* opposite signs: subtract magnitudes */
    int c = u_cmp(a, na, b, nb);
    if (c == 0) {
        return pyrs_int_tag_small(0);
    }
    if (c > 0) {
        long long rn;
        unsigned long long *r = u_sub(a, na, b, nb, &rn);
        return int_from_sign_limbs(sa, r, rn);
    }
    long long rn;
    unsigned long long *r = u_sub(b, nb, a, na, &rn);
    return int_from_sign_limbs(sb, r, rn);
}

long long pyrs_int_add(long long a, long long b) {
    if (pyrs_int_is_small(a) && pyrs_int_is_small(b)) {
        long long av = pyrs_int_small_val(a);
        long long bv = pyrs_int_small_val(b);
        /* checked add in i128 */
        __int128 s = (__int128)av + (__int128)bv;
        if (s >= PYRS_SMALL_MIN && s <= PYRS_SMALL_MAX) {
            return pyrs_int_tag_small((long long)s);
        }
        return pyrs_int_from_i64((long long)s); /* may still be in i64 */
    }
    int sa, sb, oa, ob;
    long long na, nb;
    unsigned long long *da = int_read_mag(a, &sa, &na, &oa);
    unsigned long long *db = int_read_mag(b, &sb, &nb, &ob);
    long long r = int_add_signed(sa, da, na, sb, db, nb);
    if (oa) {
        free(da);
    }
    if (ob) {
        free(db);
    }
    return r;
}

long long pyrs_int_neg(long long a) {
    if (pyrs_int_is_small(a)) {
        long long v = pyrs_int_small_val(a);
        if (v == PYRS_SMALL_MIN) {
            /* -(-2^62) = 2^62 which is outside small max 2^62-1 */
            return pyrs_int_from_i64(-v);
        }
        return pyrs_int_tag_small(-v);
    }
    PyrsInt *h = pyrs_int_heap_ptr(a);
    if (h->sign == 0) {
        return pyrs_int_tag_small(0);
    }
    return int_from_sign_limbs(-h->sign, int_copy_limbs(h->limbs, h->nlimbs),
                               h->nlimbs);
}

long long pyrs_int_sub(long long a, long long b) {
    return pyrs_int_add(a, pyrs_int_neg(b));
}

long long pyrs_int_abs(long long a) {
    if (pyrs_int_is_small(a)) {
        long long v = pyrs_int_small_val(a);
        if (v >= 0) {
            return a;
        }
        if (v == PYRS_SMALL_MIN) {
            return pyrs_int_from_i64(-v);
        }
        return pyrs_int_tag_small(-v);
    }
    PyrsInt *h = pyrs_int_heap_ptr(a);
    if (h->sign >= 0) {
        return a;
    }
    return int_from_sign_limbs(1, int_copy_limbs(h->limbs, h->nlimbs),
                               h->nlimbs);
}

static unsigned long long *u_mul(const unsigned long long *a, long long na,
                                 const unsigned long long *b, long long nb,
                                 long long *rn) {
    if (na == 0 || nb == 0) {
        *rn = 0;
        return NULL;
    }
    long long n = na + nb;
    unsigned long long *r = xmalloc((size_t)n * sizeof(unsigned long long));
    memset(r, 0, (size_t)n * sizeof(unsigned long long));
    for (long long i = 0; i < na; i++) {
        unsigned long long carry = 0;
        for (long long j = 0; j < nb; j++) {
            __uint128_t cur =
                (__uint128_t)r[i + j] + (__uint128_t)a[i] * b[j] + carry;
            r[i + j] = (unsigned long long)cur;
            carry = (unsigned long long)(cur >> 64);
        }
        r[i + nb] = carry;
    }
    *rn = n;
    int_trim(r, rn);
    return r;
}

long long pyrs_int_mul(long long a, long long b) {
    if (pyrs_int_is_small(a) && pyrs_int_is_small(b)) {
        __int128 p = (__int128)pyrs_int_small_val(a) * (__int128)pyrs_int_small_val(b);
        if (p >= PYRS_SMALL_MIN && p <= PYRS_SMALL_MAX) {
            return pyrs_int_tag_small((long long)p);
        }
        if (p >= (__int128)LLONG_MIN && p <= (__int128)LLONG_MAX) {
            return pyrs_int_from_i64((long long)p);
        }
        /* need two limbs */
        int sign = p < 0 ? -1 : 1;
        unsigned long long mag =
            p < 0 ? (unsigned long long)(-p) : (unsigned long long)p;
        /* p may need full 128 bits */
        __uint128_t um = p < 0 ? (__uint128_t)(-p) : (__uint128_t)p;
        unsigned long long *limbs = xmalloc(2 * sizeof(unsigned long long));
        limbs[0] = (unsigned long long)um;
        limbs[1] = (unsigned long long)(um >> 64);
        long long n = limbs[1] ? 2 : 1;
        (void)mag;
        return int_from_sign_limbs(sign, limbs, n);
    }
    int sa, sb, oa, ob;
    long long na, nb;
    unsigned long long *da = int_read_mag(a, &sa, &na, &oa);
    unsigned long long *db = int_read_mag(b, &sb, &nb, &ob);
    if (sa == 0 || sb == 0) {
        if (oa) {
            free(da);
        }
        if (ob) {
            free(db);
        }
        return pyrs_int_tag_small(0);
    }
    long long rn;
    unsigned long long *r = u_mul(da, na, db, nb, &rn);
    int sign = sa * sb;
    if (oa) {
        free(da);
    }
    if (ob) {
        free(db);
    }
    return int_from_sign_limbs(sign, r, rn);
}

/* Division: Knuth schoolbook, returns floor-div and mod with CPython signs. */

static void u_divmod(const unsigned long long *num, long long nn,
                     const unsigned long long *den, long long nd,
                     unsigned long long **q_out, long long *nq,
                     unsigned long long **r_out, long long *nr) {
    if (nd == 0) {
        pyrs_die("ZeroDivisionError: division by zero");
    }
    if (nn == 0 || u_cmp(num, nn, den, nd) < 0) {
        *q_out = NULL;
        *nq = 0;
        *r_out = int_copy_limbs(num, nn);
        *nr = nn;
        return;
    }
    if (nd == 1) {
        unsigned long long d = den[0];
        unsigned long long *q = xmalloc((size_t)nn * sizeof(unsigned long long));
        unsigned long long rem = 0;
        for (long long i = nn - 1; i >= 0; i--) {
            __uint128_t cur = ((__uint128_t)rem << 64) | num[i];
            q[i] = (unsigned long long)(cur / d);
            rem = (unsigned long long)(cur % d);
        }
        *nq = nn;
        int_trim(q, nq);
        *q_out = q;
        if (rem == 0) {
            *r_out = NULL;
            *nr = 0;
        } else {
            *r_out = xmalloc(sizeof(unsigned long long));
            (*r_out)[0] = rem;
            *nr = 1;
        }
        return;
    }
    /* General multi-limb: binary long division (simple, not fastest). */
    unsigned long long *rem = int_copy_limbs(num, nn);
    long long nr_ = nn;
    long long qbits = (nn - nd + 1) * 64;
    unsigned long long *q =
        xmalloc((size_t)(nn - nd + 2) * sizeof(unsigned long long));
    memset(q, 0, (size_t)(nn - nd + 2) * sizeof(unsigned long long));
    long long nq_ = nn - nd + 1;

    /* Align den to top of rem and subtract when possible */
    for (long long shift = (nn - nd) * 64 + 63; shift >= 0; shift--) {
        long long limb_shift = shift / 64;
        int bit = (int)(shift % 64);
        /* compare rem >= den << shift */
        long long need = nd + limb_shift + (bit ? 1 : 0);
        if (nr_ > need) {
            /* rem larger */
        } else if (nr_ < nd + limb_shift) {
            continue;
        }
        /* Build shifted den comparison without full alloc when possible:
         * compare rem[limb_shift..] with den shifted by bit */
        int ge = 0;
        {
            unsigned long long carry = 0;
            /* We'll try subtract den<<shift from rem; if borrow remains, undo */
            long long maxn = nr_ > (nd + limb_shift + 1) ? nr_
                                                         : (nd + limb_shift + 1);
            unsigned long long *tmp =
                xmalloc((size_t)maxn * sizeof(unsigned long long));
            memset(tmp, 0, (size_t)maxn * sizeof(unsigned long long));
            for (long long i = 0; i < nd; i++) {
                __uint128_t v = (__uint128_t)den[i] << bit;
                unsigned long long lo = (unsigned long long)v;
                unsigned long long hi = (unsigned long long)(v >> 64);
                long long j = i + limb_shift;
                __uint128_t s = (__uint128_t)tmp[j] + lo;
                tmp[j] = (unsigned long long)s;
                carry = (unsigned long long)(s >> 64);
                if (hi || carry) {
                    s = (__uint128_t)tmp[j + 1] + hi + carry;
                    tmp[j + 1] = (unsigned long long)s;
                    carry = (unsigned long long)(s >> 64);
                    if (carry) {
                        tmp[j + 2] += carry;
                    }
                }
            }
            long long tn = maxn;
            int_trim(tmp, &tn);
            if (u_cmp(rem, nr_, tmp, tn) >= 0) {
                long long rn2;
                unsigned long long *diff = u_sub(rem, nr_, tmp, tn, &rn2);
                free(rem);
                rem = diff;
                nr_ = rn2;
                ge = 1;
            }
            free(tmp);
        }
        if (ge) {
            long long qi = shift / 64;
            q[qi] |= 1ULL << (shift % 64);
        }
        (void)qbits;
    }
    int_trim(q, &nq_);
    *q_out = q;
    *nq = nq_;
    *r_out = rem;
    *nr = nr_;
}

static void divmod_floor(long long a, long long b, long long *q_out,
                         long long *r_out) {
    if (!pyrs_int_truth(b)) {
        pyrs_die("ZeroDivisionError: division by zero");
    }
    int sa, sb, oa, ob;
    long long na, nb;
    unsigned long long *da = int_read_mag(a, &sa, &na, &oa);
    unsigned long long *db = int_read_mag(b, &sb, &nb, &ob);
    if (sa == 0) {
        *q_out = pyrs_int_tag_small(0);
        *r_out = pyrs_int_tag_small(0);
        if (oa) {
            free(da);
        }
        if (ob) {
            free(db);
        }
        return;
    }
    unsigned long long *uq, *ur;
    long long nq, nr;
    u_divmod(da, na, db, nb, &uq, &nq, &ur, &nr);
    /* trunc toward zero quotient has sign sa*sb, rem has sign sa */
    int qs = sa * sb;
    long long q;
    if (nq == 0 || uq == NULL) {
        free(uq);
        q = pyrs_int_tag_small(0);
    } else {
        q = int_from_sign_limbs(qs, uq, nq);
    }
    long long r;
    if (nr == 0 || ur == NULL) {
        free(ur);
        r = pyrs_int_tag_small(0);
    } else {
        r = int_from_sign_limbs(sa, ur, nr);
    }
    /* Floor adjust: if rem != 0 and signs of a,b differ, q -= 1 and r += b */
    if (pyrs_int_truth(r) && sa != sb) {
        q = pyrs_int_sub(q, pyrs_int_tag_small(1));
        r = pyrs_int_add(r, b);
    }
    *q_out = q;
    *r_out = r;
    if (oa) {
        free(da);
    }
    if (ob) {
        free(db);
    }
}

long long pyrs_int_floordiv(long long a, long long b) {
    long long q, r;
    divmod_floor(a, b, &q, &r);
    (void)r;
    return q;
}

long long pyrs_int_mod(long long a, long long b) {
    long long q, r;
    divmod_floor(a, b, &q, &r);
    (void)q;
    return r;
}

long long pyrs_int_pow(long long base, long long exp) {
    if (pyrs_int_is_small(exp)) {
        long long e = pyrs_int_small_val(exp);
        if (e < 0) {
            pyrs_die(
                "ValueError: integer to a negative power is not supported; "
                "use a float base (e.g. 2.0 ** -1)");
        }
        long long result = pyrs_int_tag_small(1);
        long long b = base;
        while (e > 0) {
            if (e & 1) {
                result = pyrs_int_mul(result, b);
            }
            b = pyrs_int_mul(b, b);
            e >>= 1;
        }
        return result;
    }
    /* huge exponent: only  (-1|0|1)**big is practical */
    if (!pyrs_int_truth(exp) || pyrs_int_cmp(exp, pyrs_int_tag_small(0)) < 0) {
        if (pyrs_int_cmp(exp, pyrs_int_tag_small(0)) < 0) {
            pyrs_die(
                "ValueError: integer to a negative power is not supported; "
                "use a float base (e.g. 2.0 ** -1)");
        }
    }
    int sb = pyrs_int_cmp(base, pyrs_int_tag_small(0));
    if (sb == 0) {
        return pyrs_int_tag_small(0);
    }
    long long absb = pyrs_int_abs(base);
    if (pyrs_int_eq(absb, pyrs_int_tag_small(1))) {
        /* (-1)**e or 1**e */
        if (sb > 0) {
            return pyrs_int_tag_small(1);
        }
        /* exp odd/even: look at low bit of exp */
        int sa, oa;
        long long na;
        unsigned long long *d = int_read_mag(exp, &sa, &na, &oa);
        int odd = na > 0 && (d[0] & 1ULL);
        if (oa) {
            free(d);
        }
        return odd ? pyrs_int_tag_small(-1) : pyrs_int_tag_small(1);
    }
    pyrs_die("ValueError: exponent too large");
    return pyrs_int_tag_small(0);
}

long long pyrs_ipow(long long base, long long exp) {
    return pyrs_int_pow(base, exp);
}

/* Extended gcd: a*x + b*y = g. Uses Python floored div. */
static long long int_egcd(long long a, long long b, long long *x_out, long long *y_out) {
    long long old_r = a;
    long long r = b;
    long long old_s = pyrs_int_tag_small(1);
    long long s = pyrs_int_tag_small(0);
    long long old_t = pyrs_int_tag_small(0);
    long long t = pyrs_int_tag_small(1);
    while (pyrs_int_truth(r)) {
        long long q = pyrs_int_floordiv(old_r, r);
        long long nr = pyrs_int_sub(old_r, pyrs_int_mul(q, r));
        long long ns = pyrs_int_sub(old_s, pyrs_int_mul(q, s));
        long long nt = pyrs_int_sub(old_t, pyrs_int_mul(q, t));
        old_r = r;
        r = nr;
        old_s = s;
        s = ns;
        old_t = t;
        t = nt;
    }
    *x_out = old_s;
    *y_out = old_t;
    return old_r;
}

static long long int_modinv(long long a, long long m) {
    long long x, y;
    long long g = int_egcd(a, m, &x, &y);
    (void)y;
    if (!pyrs_int_eq(pyrs_int_abs(g), pyrs_int_tag_small(1))) {
        pyrs_die("ValueError: base is not invertible for the given modulus");
    }
    if (pyrs_int_cmp(g, pyrs_int_tag_small(0)) < 0) {
        x = pyrs_int_neg(x);
    }
    return pyrs_int_mod(x, m);
}

static long long int_pow_mod_nonneg(long long base, long long exp, long long mod) {
    base = pyrs_int_mod(base, mod);
    if (!pyrs_int_truth(exp)) {
        return pyrs_int_mod(pyrs_int_tag_small(1), mod);
    }
    long long result = pyrs_int_tag_small(1);
    long long two = pyrs_int_tag_small(2);
    long long zero = pyrs_int_tag_small(0);
    while (pyrs_int_cmp(exp, zero) > 0) {
        if (pyrs_int_truth(pyrs_int_mod(exp, two))) {
            result = pyrs_int_mod(pyrs_int_mul(result, base), mod);
        }
        base = pyrs_int_mod(pyrs_int_mul(base, base), mod);
        exp = pyrs_int_floordiv(exp, two);
    }
    return result;
}

long long pyrs_int_pow_mod(long long base, long long exp, long long mod) {
    if (!pyrs_int_truth(mod)) {
        pyrs_die("ValueError: pow() 3rd argument cannot be 0");
    }
    if (pyrs_int_eq(pyrs_int_abs(mod), pyrs_int_tag_small(1))) {
        return pyrs_int_tag_small(0);
    }
    if (pyrs_int_cmp(exp, pyrs_int_tag_small(0)) < 0) {
        base = int_modinv(base, mod);
        exp = pyrs_int_neg(exp);
    }
    return int_pow_mod_nonneg(base, exp, mod);
}

long long pyrs_float_round_to_int(double v) {
    if (isnan(v)) {
        pyrs_die("ValueError: cannot convert float NaN to integer");
    }
    if (isinf(v)) {
        pyrs_die("OverflowError: cannot convert float infinity to integer");
    }
    return pyrs_int_from_float(nearbyint(v));
}

double pyrs_float_round_digits(double v, long long ndigits_tag) {
    if (!pyrs_int_is_small(ndigits_tag)) {
        /* Huge ndigits: no change if positive; signed zero if negative. */
        if (pyrs_int_cmp(ndigits_tag, pyrs_int_tag_small(0)) >= 0) {
            return v;
        }
        return copysign(0.0, v);
    }
    long long nd = pyrs_int_small_val(ndigits_tag);
    if (nd > 22) {
        return v;
    }
    if (nd < -22) {
        return copysign(0.0, v);
    }
    if (isnan(v) || isinf(v)) {
        return v;
    }
    double p = pow(10.0, (double)nd);
    return nearbyint(v * p) / p;
}

long long pyrs_int_round(long long n, long long ndigits) {
    if (pyrs_int_cmp(ndigits, pyrs_int_tag_small(0)) >= 0) {
        return n;
    }
    long long neg_nd = pyrs_int_sub(pyrs_int_tag_small(0), ndigits);
    /* 10 ** |ndigits|; huge exponents collapse to 0. */
    if (!pyrs_int_is_small(neg_nd) || pyrs_int_small_val(neg_nd) > 4000) {
        return pyrs_int_tag_small(0);
    }
    long long factor = pyrs_int_pow(pyrs_int_tag_small(10), neg_nd);
    int neg = pyrs_int_cmp(n, pyrs_int_tag_small(0)) < 0;
    long long absn = neg ? pyrs_int_sub(pyrs_int_tag_small(0), n) : n;
    long long q = pyrs_int_floordiv(absn, factor);
    long long r = pyrs_int_mod(absn, factor);
    long long half = pyrs_int_floordiv(factor, pyrs_int_tag_small(2));
    int cmp = pyrs_int_cmp(r, half);
    long long two = pyrs_int_tag_small(2);
    int q_odd = pyrs_int_cmp(pyrs_int_mod(q, two), pyrs_int_tag_small(0)) != 0;
    if (cmp > 0 || (cmp == 0 && q_odd)) {
        q = pyrs_int_add(q, pyrs_int_tag_small(1));
    }
    long long result = pyrs_int_mul(q, factor);
    if (neg) {
        result = pyrs_int_sub(pyrs_int_tag_small(0), result);
    }
    return result;
}

/* Two's complement bit ops: convert to infinite sign-extended limb form. */

static void to_twos(long long t, unsigned long long **limbs, long long *n,
                    int *neg_inf) {
    /* For bitwise, Python uses infinite two's complement.
     * Represent negative as bitwise not of (mag-1). */
    int s, o;
    long long nn;
    unsigned long long *mag = int_read_mag(t, &s, &nn, &o);
    if (s >= 0) {
        *limbs = o ? mag : int_copy_limbs(mag, nn);
        *n = nn;
        *neg_inf = 0;
        return;
    }
    /* negative: limbs = ~(mag - 1) = -mag in two's complement */
    unsigned long long *m = o ? mag : int_copy_limbs(mag, nn);
    /* m := m - 1 */
    unsigned long long borrow = 1;
    for (long long i = 0; i < nn; i++) {
        unsigned long long v = m[i];
        m[i] = v - borrow;
        borrow = v < borrow;
    }
    /* invert */
    for (long long i = 0; i < nn; i++) {
        m[i] = ~m[i];
    }
    *limbs = m;
    *n = nn;
    *neg_inf = 1;
}

static long long from_twos(unsigned long long *limbs, long long n, int neg_inf) {
    if (!neg_inf) {
        return int_from_sign_limbs(n == 0 ? 0 : 1, limbs, n);
    }
    /* invert then add 1 → mag; sign -1 */
    for (long long i = 0; i < n; i++) {
        limbs[i] = ~limbs[i];
    }
    unsigned long long carry = 1;
    for (long long i = 0; i < n; i++) {
        __uint128_t s = (__uint128_t)limbs[i] + carry;
        limbs[i] = (unsigned long long)s;
        carry = (unsigned long long)(s >> 64);
    }
    if (carry) {
        unsigned long long *nl =
            xmalloc((size_t)(n + 1) * sizeof(unsigned long long));
        memcpy(nl, limbs, (size_t)n * sizeof(unsigned long long));
        nl[n] = carry;
        free(limbs);
        limbs = nl;
        n++;
    }
    return int_from_sign_limbs(-1, limbs, n);
}

static long long bit_binop(long long a, long long b, int op) {
    /* op: 0=and 1=or 2=xor */
    unsigned long long *la, *lb;
    long long na, nb;
    int nia, nib;
    to_twos(a, &la, &na, &nia);
    to_twos(b, &lb, &nb, &nib);
    long long n = na > nb ? na : nb;
    /* extend with sign limbs (0 or all-ones) */
    unsigned long long *ra = xmalloc((size_t)n * sizeof(unsigned long long));
    unsigned long long *rb = xmalloc((size_t)n * sizeof(unsigned long long));
    unsigned long long fa = nia ? ~0ULL : 0ULL;
    unsigned long long fb = nib ? ~0ULL : 0ULL;
    for (long long i = 0; i < n; i++) {
        ra[i] = i < na ? la[i] : fa;
        rb[i] = i < nb ? lb[i] : fb;
    }
    free(la);
    free(lb);
    unsigned long long *r = xmalloc((size_t)n * sizeof(unsigned long long));
    for (long long i = 0; i < n; i++) {
        if (op == 0) {
            r[i] = ra[i] & rb[i];
        } else if (op == 1) {
            r[i] = ra[i] | rb[i];
        } else {
            r[i] = ra[i] ^ rb[i];
        }
    }
    free(ra);
    free(rb);
    int ni = 0;
    if (op == 0) {
        ni = nia && nib;
    } else if (op == 1) {
        ni = nia || nib;
    } else {
        ni = nia ^ nib;
    }
    /* if result positive, may need trim; if neg_inf, keep at least 1 limb */
    if (!ni) {
        int_trim(r, &n);
        return int_from_sign_limbs(n == 0 ? 0 : 1, r, n);
    }
    return from_twos(r, n, 1);
}

long long pyrs_int_and(long long a, long long b) {
    if (pyrs_int_is_small(a) && pyrs_int_is_small(b)) {
        return pyrs_int_tag_small(pyrs_int_small_val(a) & pyrs_int_small_val(b));
    }
    return bit_binop(a, b, 0);
}

long long pyrs_int_or(long long a, long long b) {
    if (pyrs_int_is_small(a) && pyrs_int_is_small(b)) {
        return pyrs_int_tag_small(pyrs_int_small_val(a) | pyrs_int_small_val(b));
    }
    return bit_binop(a, b, 1);
}

long long pyrs_int_xor(long long a, long long b) {
    if (pyrs_int_is_small(a) && pyrs_int_is_small(b)) {
        return pyrs_int_tag_small(pyrs_int_small_val(a) ^ pyrs_int_small_val(b));
    }
    return bit_binop(a, b, 2);
}

long long pyrs_int_invert(long long a) {
    /* ~x = -x - 1 */
    return pyrs_int_sub(pyrs_int_neg(a), pyrs_int_tag_small(1));
}

long long pyrs_int_lshift(long long a, long long b) {
    if (pyrs_int_cmp(b, pyrs_int_tag_small(0)) < 0) {
        pyrs_die("ValueError: negative shift count");
    }
    if (!pyrs_int_truth(a) || !pyrs_int_truth(b)) {
        return a; /* x<<0 or 0<<n */
    }
    long long sh = pyrs_int_as_i64(b); /* may OverflowError */
    if (sh == 0) {
        return a;
    }
    int s, o;
    long long n;
    unsigned long long *mag = int_read_mag(a, &s, &n, &o);
    if (s == 0) {
        if (o) {
            free(mag);
        }
        return pyrs_int_tag_small(0);
    }
    long long limb_shift = sh / 64;
    int bit = (int)(sh % 64);
    long long rn = n + limb_shift + 1;
    unsigned long long *r = xmalloc((size_t)rn * sizeof(unsigned long long));
    memset(r, 0, (size_t)rn * sizeof(unsigned long long));
    if (bit == 0) {
        memcpy(r + limb_shift, mag, (size_t)n * sizeof(unsigned long long));
    } else {
        unsigned long long carry = 0;
        for (long long i = 0; i < n; i++) {
            __uint128_t v = ((__uint128_t)mag[i] << bit) | carry;
            r[i + limb_shift] = (unsigned long long)v;
            carry = (unsigned long long)(v >> 64);
        }
        r[n + limb_shift] = carry;
    }
    if (o) {
        free(mag);
    }
    return int_from_sign_limbs(s, r, rn);
}

long long pyrs_int_rshift(long long a, long long b) {
    if (pyrs_int_cmp(b, pyrs_int_tag_small(0)) < 0) {
        pyrs_die("ValueError: negative shift count");
    }
    if (!pyrs_int_truth(b)) {
        return a;
    }
    /* Python: a >> b = floor(a / 2^b) */
    long long sh = pyrs_int_as_i64(b);
    if (sh == 0) {
        return a;
    }
    if (pyrs_int_is_small(a)) {
        long long v = pyrs_int_small_val(a);
        if (sh >= 63) {
            return pyrs_int_tag_small(v < 0 ? -1 : 0);
        }
        return pyrs_int_tag_small(v >> sh);
    }
    /* floor div by 2^sh for negatives */
    int s, o;
    long long n;
    unsigned long long *mag = int_read_mag(a, &s, &n, &o);
    if (s >= 0) {
        long long limb_shift = sh / 64;
        int bit = (int)(sh % 64);
        if (limb_shift >= n) {
            if (o) {
                free(mag);
            }
            return pyrs_int_tag_small(0);
        }
        long long rn = n - limb_shift;
        unsigned long long *r = xmalloc((size_t)rn * sizeof(unsigned long long));
        if (bit == 0) {
            memcpy(r, mag + limb_shift, (size_t)rn * sizeof(unsigned long long));
        } else {
            for (long long i = 0; i < rn; i++) {
                unsigned long long cur = mag[i + limb_shift];
                unsigned long long next =
                    (i + limb_shift + 1 < n) ? mag[i + limb_shift + 1] : 0ULL;
                r[i] = (cur >> bit) | (next << (64 - bit));
            }
        }
        if (o) {
            free(mag);
        }
        return int_from_sign_limbs(1, r, rn);
    }
    /* negative: floor = -ceil(mag / 2^sh) = -( (mag + (2^sh - 1)) >> sh ) */
    unsigned long long *mag_copy = o ? mag : int_copy_limbs(mag, n);
    long long mag_t = int_from_sign_limbs(1, mag_copy, n);
    long long one = pyrs_int_lshift(pyrs_int_tag_small(1), b);
    long long adj = pyrs_int_sub(one, pyrs_int_tag_small(1));
    long long num = pyrs_int_add(mag_t, adj);
    long long shifted = pyrs_int_rshift(num, b); /* non-negative */
    return pyrs_int_neg(shifted);
}

/* decimal / base conversion for print and format */

static char *int_to_dec(long long t, long long *out_len) {
    if (pyrs_int_is_small(t)) {
        long long v = pyrs_int_small_val(t);
        char buf[32];
        int n = snprintf(buf, sizeof buf, "%lld", v);
        char *s = xmalloc((size_t)n + 1);
        memcpy(s, buf, (size_t)n + 1);
        *out_len = n;
        return s;
    }
    int s, o;
    long long n;
    unsigned long long *mag = int_read_mag(t, &s, &n, &o);
    if (s == 0) {
        char *z = xmalloc(2);
        z[0] = '0';
        z[1] = '\0';
        *out_len = 1;
        if (o) {
            free(mag);
        }
        return z;
    }
    /* repeated div by 10 */
    unsigned long long *tmp = int_copy_limbs(mag, n);
    long long tn = n;
    if (o) {
        free(mag);
    }
    /* max digits: nlimbs * 20 + 2 */
    long long cap = tn * 20 + 4;
    char *digits = xmalloc((size_t)cap);
    long long nd = 0;
    while (tn > 0) {
        unsigned long long rem = 0;
        for (long long i = tn - 1; i >= 0; i--) {
            __uint128_t cur = ((__uint128_t)rem << 64) | tmp[i];
            tmp[i] = (unsigned long long)(cur / 10ULL);
            rem = (unsigned long long)(cur % 10ULL);
        }
        digits[nd++] = (char)('0' + (int)rem);
        int_trim(tmp, &tn);
    }
    free(tmp);
    long long total = nd + (s < 0 ? 1 : 0);
    char *out = xmalloc((size_t)total + 1);
    long long j = 0;
    if (s < 0) {
        out[j++] = '-';
    }
    for (long long i = nd - 1; i >= 0; i--) {
        out[j++] = digits[i];
    }
    out[j] = '\0';
    free(digits);
    *out_len = total;
    return out;
}

static char *int_to_base_str(long long t, int base, int upper, long long *out_len) {
    if (base == 10) {
        return int_to_dec(t, out_len);
    }
    int s, o;
    long long n;
    unsigned long long *mag = int_read_mag(t, &s, &n, &o);
    if (s == 0) {
        char *z = xmalloc(2);
        z[0] = '0';
        z[1] = '\0';
        *out_len = 1;
        if (o) {
            free(mag);
        }
        return z;
    }
    unsigned long long *tmp = int_copy_limbs(mag, n);
    long long tn = n;
    if (o) {
        free(mag);
    }
    long long cap = tn * 64 + 4; /* worst: base 2 */
    char *digits = xmalloc((size_t)cap);
    long long nd = 0;
    unsigned long long ub = (unsigned long long)base;
    while (tn > 0) {
        unsigned long long rem = 0;
        for (long long i = tn - 1; i >= 0; i--) {
            __uint128_t cur = ((__uint128_t)rem << 64) | tmp[i];
            tmp[i] = (unsigned long long)(cur / ub);
            rem = (unsigned long long)(cur % ub);
        }
        int d = (int)rem;
        if (d < 10) {
            digits[nd++] = (char)('0' + d);
        } else {
            digits[nd++] = (char)((upper ? 'A' : 'a') + (d - 10));
        }
        int_trim(tmp, &tn);
    }
    free(tmp);
    long long total = nd + (s < 0 ? 1 : 0);
    char *out = xmalloc((size_t)total + 1);
    long long j = 0;
    if (s < 0) {
        out[j++] = '-';
    }
    for (long long i = nd - 1; i >= 0; i--) {
        out[j++] = digits[i];
    }
    out[j] = '\0';
    free(digits);
    *out_len = total;
    return out;
}

void pyrs_print_int(long long v) {
    long long n;
    char *s = int_to_dec(v, &n);
    fwrite(s, 1, (size_t)n, stdout);
    free(s);
}

PyrsStr *pyrs_str_from_int(long long v) {
    long long n;
    char *s = int_to_dec(v, &n);
    PyrsStr *r = str_alloc(n);
    memcpy(r->data, s, (size_t)n);
    free(s);
    return r;
}


void pyrs_print_float(double v) {
    char buf[40];
    format_double(v, buf);
    fputs(buf, stdout);
}

void pyrs_print_bool(int v) {
    fputs(v ? "True" : "False", stdout);
}

void pyrs_print_str(const PyrsStr *s) {
    check_ref(s);
    fwrite(s->data, 1, (size_t)s->len, stdout);
}

/* CPython repr of a str: single quotes unless the string contains a
 * single quote and no double quote; \\ \' \n \r \t escapes and \xHH
 * for other control bytes */
static void print_str_repr(const PyrsStr *s) {
    int has_single = 0;
    int has_double = 0;
    for (long long i = 0; i < s->len; i++) {
        if (s->data[i] == '\'') {
            has_single = 1;
        } else if (s->data[i] == '"') {
            has_double = 1;
        }
    }
    char quote = (has_single && !has_double) ? '"' : '\'';
    fputc(quote, stdout);
    for (long long i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        if (c == (unsigned char)quote || c == '\\') {
            fputc('\\', stdout);
            fputc(c, stdout);
        } else if (c == '\n') {
            fputs("\\n", stdout);
        } else if (c == '\r') {
            fputs("\\r", stdout);
        } else if (c == '\t') {
            fputs("\\t", stdout);
        } else if (c < 0x20 || c == 0x7f) {
            printf("\\x%02x", c);
        } else {
            fputc(c, stdout);
        }
    }
    fputc(quote, stdout);
}

/* forward decls for nested printing */
typedef struct PyrsTuple PyrsTuple;
typedef struct PyrsDict PyrsDict;
typedef struct PyrsSet PyrsSet;
void pyrs_print_list(const PyrsList *l, int tag);
void pyrs_print_tuple(const PyrsTuple *t);
void pyrs_print_dict(const PyrsDict *d);
void pyrs_print_set(const PyrsSet *s);

/* Union box layout matches codegen: { i32 print_tag; i64 payload } */
typedef struct {
    int print_tag;
    long long payload;
} PyrsUnionBox;

PyrsUnionBox *pyrs_union_box_new(int print_tag, long long payload) {
    PyrsUnionBox *box =
        pyrs_gc_alloc(sizeof(PyrsUnionBox), PYRS_GC_UNION_BOX);
    box->print_tag = print_tag;
    box->payload = payload;
    return box;
}

static void print_slot(long long slot, int tag) {
    switch (tag) {
    case TAG_INT:
        pyrs_print_int(slot);
        break;
    case TAG_FLOAT: {
        double d;
        memcpy(&d, &slot, sizeof d);
        pyrs_print_float(d);
        break;
    }
    case TAG_BOOL:
        fputs(slot ? "True" : "False", stdout);
        break;
    case TAG_STR:
        print_str_repr((const PyrsStr *)(uintptr_t)slot);
        break;
    case TAG_TUPLE:
        pyrs_print_tuple((const PyrsTuple *)(uintptr_t)slot);
        break;
    case TAG_DICT:
        pyrs_print_dict((const PyrsDict *)(uintptr_t)slot);
        break;
    case TAG_SET:
        pyrs_print_set((const PyrsSet *)(uintptr_t)slot);
        break;
    case TAG_UNION: {
        if (slot == 0) {
            fputs("None", stdout);
            break;
        }
        const PyrsUnionBox *u = (const PyrsUnionBox *)(uintptr_t)slot;
        if (u->print_tag < 0) {
            fputs("None", stdout);
        } else {
            print_slot(u->payload, u->print_tag);
        }
        break;
    }
    case TAG_CLOSURE:
        fputs("<function>", stdout);
        break;
    case TAG_GENERATOR:
        fputs("<generator>", stdout);
        break;
    case TAG_EXC:
        pyrs_print_exc((PyrsExc *)(uintptr_t)slot);
        break;
    default:
        /* Class instance: 13 + 8*class_id — print via type_id on the object. */
        if (tag >= TAG_CLASS_BASE && ((tag - TAG_CLASS_BASE) % 8) == 0) {
            pyrs_print_class_instance((void *)(uintptr_t)slot);
            break;
        }
        /* tag encoding for nested list: 4 + 8 * inner_tag */
        if (tag >= 4 && ((tag - 4) % 8) == 0) {
            pyrs_print_list((const PyrsList *)(uintptr_t)slot, (tag - 4) / 8);
        } else {
            printf("<object>");
        }
        break;
    }
}

/* Print a dynamic Any value (heap box {print_tag, payload} as i64).
 * Top-level print: null → None; str uses content (not repr); other tags
 * via print_slot. List/container printing still uses repr for str elems. */
void pyrs_print_any(long long slot) {
    if (slot == 0) {
        fputs("None", stdout);
        return;
    }
    const PyrsUnionBox *u = (const PyrsUnionBox *)(uintptr_t)slot;
    if (u->print_tag < 0) {
        fputs("None", stdout);
    } else if (u->print_tag == TAG_STR) {
        pyrs_print_str((const PyrsStr *)(uintptr_t)u->payload);
    } else {
        print_slot(u->payload, u->print_tag);
    }
}

/* Truthiness of a boxed print_tag + payload (CPython rules). */
static int any_truth_tag(int tag, long long payload) {
    if (tag < 0) {
        return 0; /* None */
    }
    if (tag == TAG_INT) {
        return pyrs_int_truth(payload);
    }
    if (tag == TAG_FLOAT) {
        double d;
        memcpy(&d, &payload, sizeof d);
        /* Python: 0.0 falsy; NaN truthy (une vs 0.0). */
        return d != 0.0;
    }
    if (tag == TAG_BOOL) {
        return payload != 0;
    }
    if (tag == TAG_STR) {
        const PyrsStr *s = (const PyrsStr *)(uintptr_t)payload;
        return s != NULL && s->len != 0;
    }
    if (tag == TAG_TUPLE || tag == TAG_DICT || tag == TAG_SET) {
        /* shared leading i64 length */
        const long long *hdr = (const long long *)(uintptr_t)payload;
        return hdr != NULL && hdr[0] != 0;
    }
    if (tag == TAG_UNION) {
        /* Nested union/Any box */
        if (payload == 0) {
            return 0;
        }
        const PyrsUnionBox *inner = (const PyrsUnionBox *)(uintptr_t)payload;
        return any_truth_tag(inner->print_tag, inner->payload);
    }
    /* Nested list: 4 + 8 * elem_tag (includes list[Any] = 4+8*8 = 68) */
    if (tag >= 4 && ((tag - 4) % 8) == 0) {
        const long long *hdr = (const long long *)(uintptr_t)payload;
        return hdr != NULL && hdr[0] != 0;
    }
    /* Closure / generator / exception / class instance: truthy when non-null. */
    if (tag == TAG_CLOSURE || tag == TAG_GENERATOR || tag == 11 /* exception */) {
        return payload != 0;
    }
    if (tag >= TAG_CLASS_BASE && ((tag - TAG_CLASS_BASE) % 8) == 0) {
        return payload != 0;
    }
    return 1;
}

/* bool(any_value) / `if any_value:` — 1 truthy, 0 falsy. Null slot → falsy. */
int pyrs_any_truth(long long slot) {
    if (slot == 0) {
        return 0;
    }
    const PyrsUnionBox *u = (const PyrsUnionBox *)(uintptr_t)slot;
    return any_truth_tag(u->print_tag, u->payload);
}

/* element tags match codegen: 0=int 1=float 2=bool 3=str; nested list 4+8*t;
 * 5=tuple 6=dict 7=set */
void pyrs_print_list(const PyrsList *l, int tag) {
    check_ref(l);
    fputc('[', stdout);
    for (long long i = 0; i < l->len; i++) {
        if (i > 0) {
            fputs(", ", stdout);
        }
        print_slot(l->data[i], tag);
    }
    fputc(']', stdout);
}

void pyrs_print_sep(void) {
    fputc(' ', stdout);
}

void pyrs_print_end(void) {
    fputc('\n', stdout);
}

void pyrs_flush_if(int on) {
    if (on) {
        fflush(stdout);
    }
}

/* ---- shared ---- */

/* str and list both lead with their length */
long long pyrs_len(const void *obj) {
    check_ref(obj);
    return *(const long long *)obj;
}

/* ---- strings ---- */

PyrsStr *pyrs_str_concat(const PyrsStr *a, const PyrsStr *b) {
    check_ref(a);
    check_ref(b);
    PyrsStr *r = str_alloc(a->len + b->len);
    memcpy(r->data, a->data, (size_t)a->len);
    memcpy(r->data + a->len, b->data, (size_t)b->len);
    return r;
}

PyrsStr *pyrs_str_repeat(const PyrsStr *s, long long n) {
    check_ref(s);
    if (n < 0) {
        n = 0; /* Python: "ab" * -1 == "" */
    }
    PyrsStr *r = str_alloc(s->len * n);
    for (long long i = 0; i < n; i++) {
        memcpy(r->data + i * s->len, s->data, (size_t)s->len);
    }
    return r;
}

/* lexicographic: -1 / 0 / 1 */
int pyrs_str_cmp(const PyrsStr *a, const PyrsStr *b) {
    check_ref(a);
    check_ref(b);
    long long min = a->len < b->len ? a->len : b->len;
    int c = memcmp(a->data, b->data, (size_t)min);
    if (c != 0) {
        return c > 0 ? 1 : -1;
    }
    if (a->len == b->len) {
        return 0;
    }
    return a->len > b->len ? 1 : -1;
}

/* single-character strings are interned: indexing/iterating a string
 * allocates nothing */
static struct {
    long long len;
    char data[2];
} single_chars[256];

static PyrsStr *single_char(unsigned char c) {
    if (single_chars[c].len == 0) {
        single_chars[c].len = 1;
        single_chars[c].data[0] = (char)c;
        single_chars[c].data[1] = '\0';
    }
    return (PyrsStr *)&single_chars[c];
}

static struct {
    long long len;
    char data[1];
} empty_str_storage = {0, {'\0'}};
#define EMPTY_STR ((PyrsStr *)&empty_str_storage)

PyrsStr *pyrs_str_index(const PyrsStr *s, long long i) {
    check_ref(s);
    if (i < 0) {
        i += s->len;
    }
    if (i < 0 || i >= s->len) {
        pyrs_die("IndexError: string index out of range");
    }
    return single_char((unsigned char)s->data[i]);
}

/* a substring copy, reusing the interned empty/single-char strings */
static PyrsStr *str_sub(const PyrsStr *s, long long off, long long n) {
    if (n <= 0) {
        return EMPTY_STR;
    }
    if (n == 1) {
        return single_char((unsigned char)s->data[off]);
    }
    PyrsStr *r = str_alloc(n);
    memcpy(r->data, s->data + off, (size_t)n);
    return r;
}

/* CPython PySlice_AdjustIndices: resolve one bound against len for the
 * step's direction; LLONG_MIN encodes a missing bound */
static long long resolve_slice_bound(long long i, int is_start, long long len, long long step) {
    if (i == LLONG_MIN) {
        if (step > 0) {
            return is_start ? 0 : len;
        }
        return is_start ? len - 1 : -1;
    }
    if (i < 0) {
        i += len;
        if (i < 0) {
            return step > 0 ? 0 : -1;
        }
    } else if (i >= len) {
        return step > 0 ? len : len - 1;
    }
    return i;
}

static long long slice_count(long long start, long long stop, long long step) {
    if (step > 0) {
        return stop > start ? (stop - start + step - 1) / step : 0;
    }
    return stop < start ? (stop - start + step + 1) / step : 0;
}

PyrsStr *pyrs_str_slice(const PyrsStr *s, long long lo, long long hi, long long step) {
    check_ref(s);
    if (step == 0) {
        pyrs_die("ValueError: slice step cannot be zero");
    }
    long long start = resolve_slice_bound(lo, 1, s->len, step);
    long long stop = resolve_slice_bound(hi, 0, s->len, step);
    long long n = slice_count(start, stop, step);
    if (step == 1) {
        return str_sub(s, start, n);
    }
    if (n <= 0) {
        return EMPTY_STR;
    }
    if (n == 1) {
        return single_char((unsigned char)s->data[start]);
    }
    PyrsStr *r = str_alloc(n);
    for (long long i = 0; i < n; i++) {
        r->data[i] = s->data[start + i * step];
    }
    return r;
}

/* ---- str methods (ASCII case/whitespace rules) ---- */

PyrsStr *pyrs_str_upper(const PyrsStr *s) {
    check_ref(s);
    PyrsStr *r = str_alloc(s->len);
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        r->data[i] = (c >= 'a' && c <= 'z') ? (char)(c - 32) : c;
    }
    return r;
}

PyrsStr *pyrs_str_lower(const PyrsStr *s) {
    check_ref(s);
    PyrsStr *r = str_alloc(s->len);
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        r->data[i] = (c >= 'A' && c <= 'Z') ? (char)(c + 32) : c;
    }
    return r;
}

static char ascii_upper(char c) {
    return (c >= 'a' && c <= 'z') ? (char)(c - 32) : c;
}

static char ascii_lower(char c) {
    return (c >= 'A' && c <= 'Z') ? (char)(c + 32) : c;
}

static int ascii_is_letter(char c) {
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z');
}

PyrsStr *pyrs_str_capitalize(const PyrsStr *s) {
    check_ref(s);
    PyrsStr *r = str_alloc(s->len);
    if (s->len == 0) {
        return r;
    }
    r->data[0] = ascii_upper(s->data[0]);
    for (long long i = 1; i < s->len; i++) {
        r->data[i] = ascii_lower(s->data[i]);
    }
    return r;
}

PyrsStr *pyrs_str_title(const PyrsStr *s) {
    check_ref(s);
    PyrsStr *r = str_alloc(s->len);
    int prev_letter = 0;
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        if (ascii_is_letter(c)) {
            r->data[i] = prev_letter ? ascii_lower(c) : ascii_upper(c);
            prev_letter = 1;
        } else {
            r->data[i] = c;
            prev_letter = 0;
        }
    }
    return r;
}

PyrsStr *pyrs_str_swapcase(const PyrsStr *s) {
    check_ref(s);
    PyrsStr *r = str_alloc(s->len);
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        if (c >= 'a' && c <= 'z') {
            r->data[i] = (char)(c - 32);
        } else if (c >= 'A' && c <= 'Z') {
            r->data[i] = (char)(c + 32);
        } else {
            r->data[i] = c;
        }
    }
    return r;
}

/* mode: 0=ljust (pad right), 1=rjust (pad left), 2=center */
static PyrsStr *str_just(const PyrsStr *s, long long width, const PyrsStr *fill, int mode) {
    check_ref(s);
    check_ref(fill);
    if (fill->len != 1) {
        pyrs_die("TypeError: The fill character must be exactly one character long");
    }
    if (width <= s->len) {
        return str_sub(s, 0, s->len);
    }
    long long pad = width - s->len;
    long long left;
    if (mode == 0) {
        left = 0;
    } else if (mode == 1) {
        left = pad;
    } else {
        /* CPython 3.14: extra pad on the left when len is even. */
        left = (pad + 1 - (s->len & 1)) / 2;
    }
    long long right = pad - left;
    PyrsStr *r = str_alloc(width);
    char fc = fill->data[0];
    for (long long i = 0; i < left; i++) {
        r->data[i] = fc;
    }
    memcpy(r->data + left, s->data, (size_t)s->len);
    for (long long i = 0; i < right; i++) {
        r->data[left + s->len + i] = fc;
    }
    return r;
}

PyrsStr *pyrs_str_ljust(const PyrsStr *s, long long width, const PyrsStr *fill) {
    return str_just(s, width, fill, 0);
}

PyrsStr *pyrs_str_rjust(const PyrsStr *s, long long width, const PyrsStr *fill) {
    return str_just(s, width, fill, 1);
}

PyrsStr *pyrs_str_center(const PyrsStr *s, long long width, const PyrsStr *fill) {
    return str_just(s, width, fill, 2);
}

PyrsStr *pyrs_str_zfill(const PyrsStr *s, long long width) {
    check_ref(s);
    if (width <= s->len) {
        return str_sub(s, 0, s->len);
    }
    long long nzero = width - s->len;
    int sign = s->len > 0 && (s->data[0] == '+' || s->data[0] == '-');
    PyrsStr *r = str_alloc(width);
    long long o = 0;
    if (sign) {
        r->data[o++] = s->data[0];
    }
    for (long long i = 0; i < nzero; i++) {
        r->data[o++] = '0';
    }
    memcpy(r->data + o, s->data + (sign ? 1 : 0), (size_t)(s->len - (sign ? 1 : 0)));
    return r;
}

PyrsStr *pyrs_str_expandtabs(const PyrsStr *s, long long tabsize) {
    check_ref(s);
    long long n = 0;
    long long col = 0;
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        if (c == '\t') {
            if (tabsize <= 0) {
                continue;
            }
            long long pad = tabsize - (col % tabsize);
            if (pad <= 0) {
                pad = tabsize;
            }
            if (n > LLONG_MAX - pad) {
                pyrs_die("MemoryError: expandtabs result too large");
            }
            n += pad;
            col += pad;
        } else {
            if (n == LLONG_MAX) {
                pyrs_die("MemoryError: expandtabs result too large");
            }
            n++;
            if (c == '\n' || c == '\r') {
                col = 0;
            } else {
                col++;
            }
        }
    }
    PyrsStr *r = str_alloc(n);
    long long o = 0;
    col = 0;
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        if (c == '\t') {
            if (tabsize <= 0) {
                continue;
            }
            long long pad = tabsize - (col % tabsize);
            if (pad <= 0) {
                pad = tabsize;
            }
            for (long long k = 0; k < pad; k++) {
                r->data[o++] = ' ';
            }
            col += pad;
        } else {
            r->data[o++] = c;
            if (c == '\n' || c == '\r') {
                col = 0;
            } else {
                col++;
            }
        }
    }
    return r;
}

static int is_py_space(char c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' || c == '\f';
}

static PyrsStr *strip_impl(const PyrsStr *s, int left, int right) {
    check_ref(s);
    long long b = 0;
    long long e = s->len;
    if (left) {
        while (b < e && is_py_space(s->data[b])) {
            b++;
        }
    }
    if (right) {
        while (e > b && is_py_space(s->data[e - 1])) {
            e--;
        }
    }
    return str_sub(s, b, e - b);
}

PyrsStr *pyrs_str_strip(const PyrsStr *s) {
    return strip_impl(s, 1, 1);
}
PyrsStr *pyrs_str_lstrip(const PyrsStr *s) {
    return strip_impl(s, 1, 0);
}
PyrsStr *pyrs_str_rstrip(const PyrsStr *s) {
    return strip_impl(s, 0, 1);
}

static int byte_in_set(unsigned char c, const unsigned char set[32]) {
    return (set[c >> 3] >> (c & 7)) & 1;
}

static void fill_byte_set(const PyrsStr *chars, unsigned char set[32]) {
    memset(set, 0, 32);
    for (long long i = 0; i < chars->len; i++) {
        unsigned char c = (unsigned char)chars->data[i];
        set[c >> 3] |= (unsigned char)(1u << (c & 7));
    }
}

static PyrsStr *strip_chars_impl(const PyrsStr *s, const PyrsStr *chars, int left, int right) {
    check_ref(s);
    check_ref(chars);
    if (chars->len == 0 || s->len == 0) {
        return (PyrsStr *)s;
    }
    unsigned char set[32];
    fill_byte_set(chars, set);
    long long b = 0;
    long long e = s->len;
    if (left) {
        while (b < e && byte_in_set((unsigned char)s->data[b], set)) {
            b++;
        }
    }
    if (right) {
        while (e > b && byte_in_set((unsigned char)s->data[e - 1], set)) {
            e--;
        }
    }
    if (b == 0 && e == s->len) {
        return (PyrsStr *)s;
    }
    return str_sub(s, b, e - b);
}

PyrsStr *pyrs_str_strip_chars(const PyrsStr *s, const PyrsStr *chars) {
    return strip_chars_impl(s, chars, 1, 1);
}
PyrsStr *pyrs_str_lstrip_chars(const PyrsStr *s, const PyrsStr *chars) {
    return strip_chars_impl(s, chars, 1, 0);
}
PyrsStr *pyrs_str_rstrip_chars(const PyrsStr *s, const PyrsStr *chars) {
    return strip_chars_impl(s, chars, 0, 1);
}

int pyrs_str_startswith(const PyrsStr *s, const PyrsStr *pre) {
    check_ref(s);
    check_ref(pre);
    return pre->len <= s->len && memcmp(s->data, pre->data, (size_t)pre->len) == 0;
}

int pyrs_str_endswith(const PyrsStr *s, const PyrsStr *suf) {
    check_ref(s);
    check_ref(suf);
    return suf->len <= s->len &&
           memcmp(s->data + s->len - suf->len, suf->data, (size_t)suf->len) == 0;
}

PyrsStr *pyrs_str_removeprefix(const PyrsStr *s, const PyrsStr *pre) {
    check_ref(s);
    check_ref(pre);
    if (pre->len > 0 && pyrs_str_startswith(s, pre)) {
        long long n = s->len - pre->len;
        PyrsStr *r = str_alloc(n);
        memcpy(r->data, s->data + pre->len, (size_t)n);
        return r;
    }
    return (PyrsStr *)s;
}

PyrsStr *pyrs_str_removesuffix(const PyrsStr *s, const PyrsStr *suf) {
    check_ref(s);
    check_ref(suf);
    if (suf->len > 0 && pyrs_str_endswith(s, suf)) {
        long long n = s->len - suf->len;
        PyrsStr *r = str_alloc(n);
        memcpy(r->data, s->data, (size_t)n);
        return r;
    }
    return (PyrsStr *)s;
}

/* Slice-adjust start/end like CPython. `end == LLONG_MIN` means missing → len. */
static void str_adjust_bounds(long long len, long long *start, long long *end) {
    long long s = *start;
    long long e = *end;
    if (s < 0) {
        s += len;
        if (s < 0) {
            s = 0;
        }
    }
    /* Do not clamp start > len — CPython then has start > end and returns -1
     * (so "abc".find("", 4) is -1, not 3). */
    if (e == LLONG_MIN) {
        e = len;
    } else if (e < 0) {
        e += len;
        if (e < 0) {
            e = 0;
        }
    } else if (e > len) {
        e = len;
    }
    *start = s;
    *end = e;
}

static int str_affix_in_slice(const PyrsStr *s, const PyrsStr *aff, long long start,
                              long long end, int from_end) {
    check_ref(s);
    check_ref(aff);
    str_adjust_bounds(s->len, &start, &end);
    if (start > end) {
        return 0;
    }
    if (aff->len == 0) {
        return 1;
    }
    if (aff->len > end - start) {
        return 0;
    }
    if (from_end) {
        return memcmp(s->data + end - aff->len, aff->data, (size_t)aff->len) == 0;
    }
    return memcmp(s->data + start, aff->data, (size_t)aff->len) == 0;
}

int pyrs_str_startswith_slice(const PyrsStr *s, const PyrsStr *pre, long long start,
                              long long end) {
    return str_affix_in_slice(s, pre, start, end, 0);
}

int pyrs_str_endswith_slice(const PyrsStr *s, const PyrsStr *suf, long long start,
                            long long end) {
    return str_affix_in_slice(s, suf, start, end, 1);
}

static long long str_find_bounds(const PyrsStr *s, const PyrsStr *t, long long start,
                                 long long end, int from_right) {
    check_ref(s);
    check_ref(t);
    str_adjust_bounds(s->len, &start, &end);
    if (start > end) {
        return -1;
    }
    if (t->len == 0) {
        return from_right ? end : start;
    }
    if (t->len > end - start) {
        return -1;
    }
    if (from_right) {
        for (long long i = end - t->len; i >= start; i--) {
            if (memcmp(s->data + i, t->data, (size_t)t->len) == 0) {
                return i;
            }
        }
        return -1;
    }
    for (long long i = start; i + t->len <= end; i++) {
        if (memcmp(s->data + i, t->data, (size_t)t->len) == 0) {
            return i;
        }
    }
    return -1;
}

/* first index of t in s, or -1; the empty needle is found at 0 */
long long pyrs_str_find(const PyrsStr *s, const PyrsStr *t) {
    return str_find_bounds(s, t, 0, LLONG_MIN, 0);
}

/* last index of t in s, or -1; the empty needle is found at len(s) */
long long pyrs_str_rfind(const PyrsStr *s, const PyrsStr *t) {
    return str_find_bounds(s, t, 0, LLONG_MIN, 1);
}

long long pyrs_str_find_slice(const PyrsStr *s, const PyrsStr *t, long long start,
                              long long end) {
    return str_find_bounds(s, t, start, end, 0);
}

long long pyrs_str_rfind_slice(const PyrsStr *s, const PyrsStr *t, long long start,
                               long long end) {
    return str_find_bounds(s, t, start, end, 1);
}

/* like find, but trap when absent (CPython: ValueError: substring not found) */
long long pyrs_str_index_of(const PyrsStr *s, const PyrsStr *t, long long start,
                            long long end) {
    long long i = str_find_bounds(s, t, start, end, 0);
    if (i < 0) {
        pyrs_die("ValueError: substring not found");
    }
    return i;
}

/* like rfind, but trap when absent */
long long pyrs_str_rindex(const PyrsStr *s, const PyrsStr *t, long long start,
                          long long end) {
    long long i = str_find_bounds(s, t, start, end, 1);
    if (i < 0) {
        pyrs_die("ValueError: substring not found");
    }
    return i;
}

/* non-overlapping occurrences in [start, end); empty needle → slice_len+1 */
long long pyrs_str_count_slice(const PyrsStr *s, const PyrsStr *t, long long start,
                               long long end) {
    check_ref(s);
    check_ref(t);
    str_adjust_bounds(s->len, &start, &end);
    if (start > end) {
        return 0;
    }
    if (t->len == 0) {
        return end - start + 1;
    }
    if (t->len > end - start) {
        return 0;
    }
    long long n = 0;
    long long i = start;
    while (i + t->len <= end) {
        if (memcmp(s->data + i, t->data, (size_t)t->len) == 0) {
            n++;
            i += t->len;
        } else {
            i++;
        }
    }
    return n;
}

/* non-overlapping occurrences; Python counts len+1 for an empty needle */
long long pyrs_str_count(const PyrsStr *s, const PyrsStr *t) {
    return pyrs_str_count_slice(s, t, 0, LLONG_MIN);
}

/* count < 0 means replace all (CPython). */
PyrsStr *pyrs_str_replace(const PyrsStr *s, const PyrsStr *old, const PyrsStr *new_s,
                          long long count) {
    check_ref(s);
    check_ref(old);
    check_ref(new_s);
    if (count == 0) {
        return str_sub(s, 0, s->len);
    }
    /* Python: an empty old inserts new between every character (and at ends). */
    if (old->len == 0) {
        long long max_ins = s->len + 1;
        long long nins = (count < 0 || count >= max_ins) ? max_ins : count;
        if (nins == 0) {
            return str_sub(s, 0, s->len);
        }
        long long n = s->len + nins * new_s->len;
        PyrsStr *r = str_alloc(n);
        char *p = r->data;
        long long inserted = 0;
        for (long long i = 0; i < s->len; i++) {
            if (inserted < nins) {
                memcpy(p, new_s->data, (size_t)new_s->len);
                p += new_s->len;
                inserted++;
            }
            *p++ = s->data[i];
        }
        if (inserted < nins) {
            memcpy(p, new_s->data, (size_t)new_s->len);
        }
        return r;
    }
    long long avail = pyrs_str_count(s, old);
    long long nrep = count < 0 ? avail : (count < avail ? count : avail);
    if (nrep == 0) {
        return str_sub(s, 0, s->len);
    }
    long long n = s->len + nrep * (new_s->len - old->len);
    PyrsStr *r = str_alloc(n);
    char *p = r->data;
    long long i = 0;
    long long done = 0;
    while (i < s->len) {
        if (done < nrep && i + old->len <= s->len &&
            memcmp(s->data + i, old->data, (size_t)old->len) == 0) {
            memcpy(p, new_s->data, (size_t)new_s->len);
            p += new_s->len;
            i += old->len;
            done++;
        } else {
            *p++ = s->data[i++];
        }
    }
    return r;
}

static void list_reverse_slots(PyrsList *l) {
    for (long long i = 0, j = l->len - 1; i < j; i++, j--) {
        long long t = l->data[i];
        l->data[i] = l->data[j];
        l->data[j] = t;
    }
}

/* maxsplit < 0 means unlimited. */
PyrsList *pyrs_str_split_ws(const PyrsStr *s, long long maxsplit) {
    check_ref(s);
    PyrsList *r = pyrs_list_new(4);
    long long i = 0;
    long long n = 0;
    while (i < s->len) {
        while (i < s->len && is_py_space(s->data[i])) {
            i++;
        }
        if (i >= s->len) {
            break;
        }
        long long start = i;
        if (maxsplit >= 0 && n == maxsplit) {
            pyrs_list_push(r, (long long)str_sub(s, start, s->len - start));
            break;
        }
        while (i < s->len && !is_py_space(s->data[i])) {
            i++;
        }
        pyrs_list_push(r, (long long)str_sub(s, start, i - start));
        n++;
    }
    return r;
}

PyrsList *pyrs_str_rsplit_ws(const PyrsStr *s, long long maxsplit) {
    check_ref(s);
    PyrsList *r = pyrs_list_new(4);
    long long i = s->len;
    long long n = 0;
    while (i > 0) {
        while (i > 0 && is_py_space(s->data[i - 1])) {
            i--;
        }
        if (i == 0) {
            break;
        }
        long long end = i;
        if (maxsplit >= 0 && n == maxsplit) {
            pyrs_list_push(r, (long long)str_sub(s, 0, end));
            break;
        }
        while (i > 0 && !is_py_space(s->data[i - 1])) {
            i--;
        }
        pyrs_list_push(r, (long long)str_sub(s, i, end - i));
        n++;
    }
    list_reverse_slots(r);
    return r;
}

/* CPython splitlines: \n \r \r\n \v \f \x1c \x1d \x1e, plus UTF-8 U+0085 /
 * U+2028 / U+2029. `keepends` is CPython truthiness (already lowered). */
static long long linebreak_len(const char *p, long long i, long long n) {
    unsigned char c = (unsigned char)p[i];
    if (c == '\r') {
        if (i + 1 < n && p[i + 1] == '\n') {
            return 2;
        }
        return 1;
    }
    if (c == '\n' || c == '\v' || c == '\f' || c == 0x1c || c == 0x1d ||
        c == 0x1e) {
        return 1;
    }
    /* U+0085 NEL */
    if (c == 0xc2 && i + 1 < n && (unsigned char)p[i + 1] == 0x85) {
        return 2;
    }
    /* U+2028 LINE SEPARATOR / U+2029 PARAGRAPH SEPARATOR */
    if (c == 0xe2 && i + 2 < n && (unsigned char)p[i + 1] == 0x80 &&
        ((unsigned char)p[i + 2] == 0xa8 || (unsigned char)p[i + 2] == 0xa9)) {
        return 3;
    }
    return 0;
}

PyrsList *pyrs_str_splitlines(const PyrsStr *s, int keepends) {
    check_ref(s);
    PyrsList *r = pyrs_list_new(4);
    long long i = 0;
    while (i < s->len) {
        long long start = i;
        long long blen = 0;
        while (i < s->len) {
            blen = linebreak_len(s->data, i, s->len);
            if (blen > 0) {
                break;
            }
            i++;
        }
        if (i >= s->len) {
            pyrs_list_push(r, (long long)str_sub(s, start, s->len - start));
            break;
        }
        long long n = keepends ? (i + blen - start) : (i - start);
        pyrs_list_push(r, (long long)str_sub(s, start, n));
        i += blen;
    }
    return r;
}

PyrsList *pyrs_str_split(const PyrsStr *s, const PyrsStr *sep, long long maxsplit) {
    check_ref(s);
    check_ref(sep);
    if (sep->len == 0) {
        pyrs_die("ValueError: empty separator");
    }
    PyrsList *r = pyrs_list_new(4);
    long long start = 0;
    long long i = 0;
    long long n = 0;
    while (i + sep->len <= s->len) {
        if (maxsplit >= 0 && n == maxsplit) {
            break;
        }
        if (memcmp(s->data + i, sep->data, (size_t)sep->len) == 0) {
            pyrs_list_push(r, (long long)str_sub(s, start, i - start));
            i += sep->len;
            start = i;
            n++;
        } else {
            i++;
        }
    }
    pyrs_list_push(r, (long long)str_sub(s, start, s->len - start));
    return r;
}

PyrsList *pyrs_str_rsplit(const PyrsStr *s, const PyrsStr *sep, long long maxsplit) {
    check_ref(s);
    check_ref(sep);
    if (sep->len == 0) {
        pyrs_die("ValueError: empty separator");
    }
    PyrsList *r = pyrs_list_new(4);
    long long end = s->len;
    long long n = 0;
    long long i = end - sep->len;
    while (i >= 0) {
        if (maxsplit >= 0 && n == maxsplit) {
            break;
        }
        if (memcmp(s->data + i, sep->data, (size_t)sep->len) == 0) {
            pyrs_list_push(r, (long long)str_sub(s, i + sep->len, end - (i + sep->len)));
            end = i;
            i -= sep->len;
            n++;
        } else {
            i--;
        }
    }
    pyrs_list_push(r, (long long)str_sub(s, 0, end));
    list_reverse_slots(r);
    return r;
}

PyrsStr *pyrs_str_join(const PyrsStr *sep, const PyrsList *parts) {
    check_ref(sep);
    check_ref(parts);
    if (parts->len == 0) {
        return EMPTY_STR;
    }
    long long total = sep->len * (parts->len - 1);
    for (long long i = 0; i < parts->len; i++) {
        total += ((const PyrsStr *)parts->data[i])->len;
    }
    PyrsStr *r = str_alloc(total);
    char *p = r->data;
    for (long long i = 0; i < parts->len; i++) {
        if (i > 0) {
            memcpy(p, sep->data, (size_t)sep->len);
            p += sep->len;
        }
        const PyrsStr *part = (const PyrsStr *)parts->data[i];
        check_ref(part);
        memcpy(p, part->data, (size_t)part->len);
        p += part->len;
    }
    return r;
}

/* naive substring search; empty needle matches (like Python) */
int pyrs_str_contains(const PyrsStr *hay, const PyrsStr *needle) {
    check_ref(hay);
    check_ref(needle);
    if (needle->len == 0) {
        return 1;
    }
    if (needle->len > hay->len) {
        return 0;
    }
    for (long long i = 0; i + needle->len <= hay->len; i++) {
        if (memcmp(hay->data + i, needle->data, (size_t)needle->len) == 0) {
            return 1;
        }
    }
    return 0;
}

PyrsStr *pyrs_str_from_float(double v) {
    char buf[40];
    format_double(v, buf);
    size_t n = strlen(buf);
    PyrsStr *r = str_alloc((long long)n);
    memcpy(r->data, buf, n);
    return r;
}

/* forward decls for format helpers (int may promote to float formatting) */
PyrsStr *pyrs_format_float(double v, const PyrsStr *spec);
PyrsStr *pyrs_format_int(long long v, const PyrsStr *spec);

/* f-string `{x:.Nf}` / format(x, ".Nf") fixed-point (CPython %.*f) */
PyrsStr *pyrs_str_format_float(double v, long long precision) {
    if (precision < 0) {
        precision = 0;
    }
    if (precision > 1000) {
        precision = 1000;
    }
    int p = (int)precision;
    int n = snprintf(NULL, 0, "%.*f", p, v);
    if (n < 0) {
        pyrs_die("ValueError: float format failed");
    }
    PyrsStr *r = str_alloc((long long)n);
    snprintf(r->data, (size_t)n + 1, "%.*f", p, v);
    return r;
}

PyrsStr *pyrs_str_from_bool(int v) {
    const char *text = v ? "True" : "False";
    size_t n = strlen(text);
    PyrsStr *r = str_alloc((long long)n);
    memcpy(r->data, text, n);
    return r;
}

/* ---- format mini-language (PEP 3101 subset) ----
 * [[fill]align][sign][#][0][width][.precision][type]
 * Rejects grouping (,/_) and types n/c with clear messages. */

typedef struct {
    char fill;           /* default ' ' */
    char align;          /* '\0', '<', '>', '=', '^' */
    char sign;           /* '\0', '+', '-', ' ' */
    int alternate;       /* # */
    int zero;            /* 0 before width */
    int zflag;           /* z — coerce negative zero */
    long long width;     /* -1 = absent */
    long long precision; /* -1 = absent */
    char type;           /* '\0' or type letter */
} PyrsFormatSpec;

static void format_die_invalid(const char *spec, const char *ty_name) {
    char buf[256];
    snprintf(buf, sizeof buf,
             "ValueError: Invalid format specifier '%s' for object of type '%s'",
             spec, ty_name);
    pyrs_die(buf);
}

static void format_die_unknown(char code, const char *ty_name) {
    char buf[128];
    snprintf(buf, sizeof buf,
             "ValueError: Unknown format code '%c' for object of type '%s'", code,
             ty_name);
    pyrs_die(buf);
}

static void parse_format_spec(const PyrsStr *spec, PyrsFormatSpec *out,
                              const char *ty_name) {
    check_ref(spec);
    memset(out, 0, sizeof *out);
    out->fill = ' ';
    out->width = -1;
    out->precision = -1;

    const char *s = spec->data;
    long long len = spec->len;
    long long i = 0;

    if (len == 0) {
        return;
    }

    /* [[fill]align] */
    if (i + 1 < len) {
        char a = s[i + 1];
        if (a == '<' || a == '>' || a == '=' || a == '^') {
            out->fill = s[i];
            out->align = a;
            i += 2;
        }
    }
    if (out->align == '\0' && i < len) {
        char a = s[i];
        if (a == '<' || a == '>' || a == '=' || a == '^') {
            out->align = a;
            i += 1;
        }
    }

    /* [sign] */
    if (i < len && (s[i] == '+' || s[i] == '-' || s[i] == ' ')) {
        out->sign = s[i];
        i += 1;
    }

    /* [z] negative-zero coercion (floats) */
    if (i < len && s[i] == 'z') {
        out->zflag = 1;
        i += 1;
    }

    /* [#] */
    if (i < len && s[i] == '#') {
        out->alternate = 1;
        i += 1;
    }

    /* [0] zero-pad flag */
    if (i < len && s[i] == '0') {
        out->zero = 1;
        i += 1;
    }

    /* [width] */
    if (i < len && s[i] >= '0' && s[i] <= '9') {
        long long w = 0;
        while (i < len && s[i] >= '0' && s[i] <= '9') {
            int digit = s[i] - '0';
            if (w > (LLONG_MAX - digit) / 10) {
                format_die_invalid(s, ty_name);
            }
            w = w * 10 + digit;
            i += 1;
        }
        out->width = w;
    }

    /* grouping , or _ — not supported */
    if (i < len && (s[i] == ',' || s[i] == '_')) {
        char g = s[i];
        char buf[128];
        snprintf(buf, sizeof buf,
                 "ValueError: grouping option '%c' in format specifiers is not "
                 "supported yet",
                 g);
        pyrs_die(buf);
    }

    /* [.precision] */
    if (i < len && s[i] == '.') {
        i += 1;
        if (i >= len || s[i] < '0' || s[i] > '9') {
            format_die_invalid(s, ty_name);
        }
        long long p = 0;
        while (i < len && s[i] >= '0' && s[i] <= '9') {
            int digit = s[i] - '0';
            if (p > (LLONG_MAX - digit) / 10) {
                format_die_invalid(s, ty_name);
            }
            p = p * 10 + digit;
            i += 1;
        }
        out->precision = p;
    }

    /* [type] */
    if (i < len) {
        out->type = s[i];
        i += 1;
    }

    if (i != len) {
        format_die_invalid(s, ty_name);
    }

    /* zero flag implies fill='0' and align='=' when align not set */
    if (out->zero) {
        if (out->align == '\0') {
            out->align = '=';
        }
        if (out->fill == ' ' && out->align == '=') {
            out->fill = '0';
        }
    }
}

/* Build a new string by padding `body` (no sign) with optional sign/prefix. */
static PyrsStr *format_pad(const char *sign_str, const char *prefix,
                           const char *body, long long body_len,
                           const PyrsFormatSpec *fs) {
    long long sign_len = (long long)strlen(sign_str);
    long long pref_len = (long long)strlen(prefix);
    long long content = sign_len + pref_len + body_len;
    long long width = fs->width < 0 ? content : fs->width;
    if (width < content) {
        width = content;
    }
    long long pad = width - content;
    char align = fs->align;
    if (align == '\0') {
        align = '>'; /* default for numbers; callers for str override */
    }
    char fill = fs->fill ? fs->fill : ' ';

    PyrsStr *r = str_alloc(width);
    char *p = r->data;
    long long left = 0, right = 0, mid = 0;
    if (align == '<') {
        left = 0;
        right = pad;
    } else if (align == '^') {
        left = pad / 2;
        right = pad - left;
    } else if (align == '=') {
        /* sign + prefix, then pad, then body */
        mid = pad;
        left = 0;
        right = 0;
    } else { /* '>' */
        left = pad;
        right = 0;
    }

    if (align == '=') {
        memcpy(p, sign_str, (size_t)sign_len);
        p += sign_len;
        memcpy(p, prefix, (size_t)pref_len);
        p += pref_len;
        memset(p, fill, (size_t)mid);
        p += mid;
        memcpy(p, body, (size_t)body_len);
    } else {
        memset(p, fill, (size_t)left);
        p += left;
        memcpy(p, sign_str, (size_t)sign_len);
        p += sign_len;
        memcpy(p, prefix, (size_t)pref_len);
        p += pref_len;
        memcpy(p, body, (size_t)body_len);
        p += body_len;
        memset(p, fill, (size_t)right);
    }
    return r;
}

static void int_to_base(unsigned long long v, int base, int upper, char *out,
                        long long *out_len) {
    if (v == 0) {
        out[0] = '0';
        *out_len = 1;
        return;
    }
    char tmp[128];
    int n = 0;
    while (v > 0) {
        int d = (int)(v % (unsigned)base);
        if (d < 10) {
            tmp[n++] = (char)('0' + d);
        } else {
            tmp[n++] = (char)((upper ? 'A' : 'a') + (d - 10));
        }
        v /= (unsigned)base;
    }
    for (int i = 0; i < n; i++) {
        out[i] = tmp[n - 1 - i];
    }
    *out_len = n;
}

static const char *int_sign_str(long long v, char sign_opt) {
    if (v < 0) {
        return "-";
    }
    if (sign_opt == '+') {
        return "+";
    }
    if (sign_opt == ' ') {
        return " ";
    }
    return "";
}

PyrsStr *pyrs_format_int(long long v, const PyrsStr *spec) {
    check_ref(spec);
    if (spec->len == 0) {
        return pyrs_str_from_int(v);
    }

    PyrsFormatSpec fs;
    parse_format_spec(spec, &fs, "int");

    char type = fs.type ? fs.type : 'd';

    /* Float presentation types on int: promote (precision is allowed). */
    if (type == 'e' || type == 'E' || type == 'f' || type == 'F' || type == 'g' ||
        type == 'G' || type == '%') {
        return pyrs_format_float(pyrs_int_to_float(v), spec);
    }

    if (fs.zflag) {
        pyrs_die(
            "ValueError: Negative zero coercion (z) not allowed in integer "
            "format specifier");
    }
    if (fs.precision >= 0) {
        pyrs_die("ValueError: Precision not allowed in integer format specifier");
    }

    if (type == 'n' || type == 'c') {
        char buf[96];
        snprintf(buf, sizeof buf,
                 "ValueError: format type '%c' is not supported yet", type);
        pyrs_die(buf);
    }
    if (type == 'i' || type == 'u') {
        /* Not valid in CPython — match. */
        format_die_unknown(type, "int");
    }
    if (type != 'd' && type != 'b' && type != 'o' && type != 'x' && type != 'X') {
        format_die_unknown(type, "int");
    }

    int base = 10;
    int upper = 0;
    const char *prefix = "";
    if (type == 'b') {
        base = 2;
        if (fs.alternate) {
            prefix = "0b";
        }
    } else if (type == 'o') {
        base = 8;
        if (fs.alternate) {
            prefix = "0o";
        }
    } else if (type == 'x') {
        base = 16;
        if (fs.alternate) {
            prefix = "0x";
        }
    } else if (type == 'X') {
        base = 16;
        upper = 1;
        if (fs.alternate) {
            prefix = "0X";
        }
    }

    long long raw_len = 0;
    char *raw = int_to_base_str(v, base, upper, &raw_len);
    /* strip leading '-' for body; sign handled separately */
    const char *body_src = raw;
    long long body_len = raw_len;
    int neg = 0;
    if (raw_len > 0 && raw[0] == '-') {
        neg = 1;
        body_src = raw + 1;
        body_len = raw_len - 1;
    }
    char *body = xmalloc((size_t)body_len + 1);
    memcpy(body, body_src, (size_t)body_len);
    body[body_len] = '\0';
    free(raw);

    if (fs.align == '\0') {
        fs.align = '>';
    }

    const char *sign;
    if (neg) {
        sign = "-";
    } else if (fs.sign == '+') {
        sign = "+";
    } else if (fs.sign == ' ') {
        sign = " ";
    } else {
        sign = "";
    }
    PyrsStr *out = format_pad(sign, prefix, body, body_len, &fs);
    free(body);
    return out;
}

static void float_sign_and_mag(double v, int zflag, char *sign_out,
                               double *mag_out) {
    if (signbit(v) && v == 0.0 && zflag) {
        /* coerce -0.0 → 0.0 */
        *sign_out = '\0';
        *mag_out = 0.0;
        return;
    }
    if (signbit(v)) {
        *sign_out = '-';
        *mag_out = -v;
    } else {
        *sign_out = '\0';
        *mag_out = v;
    }
}

static const char *float_sign_str(char sign_ch, char sign_opt) {
    if (sign_ch == '-') {
        return "-";
    }
    if (sign_opt == '+') {
        return "+";
    }
    if (sign_opt == ' ') {
        return " ";
    }
    return "";
}

/* Format a non-negative finite float body (no sign) per type/precision. */
static void format_float_body(double mag, char type, long long precision,
                              int alternate, char *buf, size_t bufsz) {
    int prec;
    if (type == 'f' || type == 'F' || type == 'e' || type == 'E' || type == '%') {
        prec = precision < 0 ? 6 : (precision > 1000 ? 1000 : (int)precision);
    } else if (type == 'g' || type == 'G' || type == '\0') {
        prec = precision < 0 ? 6 : (precision > 1000 ? 1000 : (int)precision);
        if (prec == 0) {
            prec = 1; /* CPython g with .0 → 1 significant digit */
        }
    } else {
        prec = 6;
    }

    if (type == '%') {
        mag *= 100.0;
        type = 'f';
    }

    if (type == 'f' || type == 'F') {
        snprintf(buf, bufsz, alternate ? "%#.*f" : "%.*f", prec, mag);
        if (type == 'F') {
            for (char *p = buf; *p; p++) {
                if (*p >= 'a' && *p <= 'z') {
                    *p = (char)(*p - 'a' + 'A');
                }
            }
        }
    } else if (type == 'e' || type == 'E') {
        snprintf(buf, bufsz, alternate ? "%#.*e" : "%.*e", prec, mag);
        if (type == 'E') {
            for (char *p = buf; *p; p++) {
                if (*p >= 'a' && *p <= 'z') {
                    *p = (char)(*p - 'a' + 'A');
                }
            }
        }
        /* CPython uses e+NN with at least 2 exponent digits — snprintf does. */
    } else if (type == 'g' || type == 'G') {
        /* CPython g: significant digits = prec; switch to exp like printf. */
        snprintf(buf, bufsz, alternate ? "%#.*g" : "%.*g", prec, mag);
        if (type == 'G') {
            for (char *p = buf; *p; p++) {
                if (*p >= 'a' && *p <= 'z') {
                    *p = (char)(*p - 'a' + 'A');
                }
            }
        }
    } else {
        /* Should not reach: empty type handled by caller for str path. */
        snprintf(buf, bufsz, "%.*g", prec, mag);
    }
}

PyrsStr *pyrs_format_float(double v, const PyrsStr *spec) {
    check_ref(spec);
    if (spec->len == 0) {
        return pyrs_str_from_float(v);
    }

    PyrsFormatSpec fs;
    parse_format_spec(spec, &fs, "float");

    char type = fs.type;
    /* Integer presentation types are invalid on float. */
    if (type == 'd' || type == 'b' || type == 'o' || type == 'x' || type == 'X' ||
        type == 'i' || type == 'u' || type == 'c' || type == 's') {
        format_die_unknown(type ? type : '?', "float");
    }
    if (type == 'n') {
        pyrs_die("ValueError: format type 'n' is not supported yet");
    }
    if (type != '\0' && type != 'e' && type != 'E' && type != 'f' && type != 'F' &&
        type != 'g' && type != 'G' && type != '%') {
        format_die_unknown(type, "float");
    }

    /* Width-only / empty-type with no precision: str() then pad (CPython). */
    if (type == '\0' && fs.precision < 0) {
        char raw[64];
        format_double(v, raw);
        if (fs.align == '\0') {
            fs.align = '>';
        }
        /* sign is already in raw for negatives; pad as a whole string */
        return format_pad("", "", raw, (long long)strlen(raw), &fs);
    }

    /* Empty type with precision → like 'g'. */
    if (type == '\0') {
        type = 'g';
    }

    /* nan / inf */
    if (isnan(v) || isinf(v)) {
        char body[8];
        const char *sign = "";
        if (isnan(v)) {
            strcpy(body, (type == 'F' || type == 'E' || type == 'G') ? "NAN" : "nan");
        } else {
            if (signbit(v)) {
                sign = "-";
            } else if (fs.sign == '+') {
                sign = "+";
            } else if (fs.sign == ' ') {
                sign = " ";
            }
            strcpy(body, (type == 'F' || type == 'E' || type == 'G') ? "INF" : "inf");
        }
        if (fs.align == '\0') {
            fs.align = '>';
        }
        /* fill with '0' and align='=' → CPython uses space for nan/inf pad? */
        if (fs.fill == '0' && fs.align == '=') {
            fs.fill = ' ';
            fs.align = '>';
        }
        return format_pad(sign, "", body, (long long)strlen(body), &fs);
    }

    char sign_ch = '\0';
    double mag = v;
    float_sign_and_mag(v, fs.zflag, &sign_ch, &mag);
    const char *sign = float_sign_str(sign_ch, fs.sign);

    char body[128];
    if (type == '%') {
        /* body includes trailing % */
        char num[120];
        format_float_body(mag, '%', fs.precision, fs.alternate, num, sizeof num);
        snprintf(body, sizeof body, "%s%%", num);
    } else {
        format_float_body(mag, type, fs.precision, fs.alternate, body,
                          sizeof body);
    }

    if (fs.align == '\0') {
        fs.align = '>';
    }
    return format_pad(sign, "", body, (long long)strlen(body), &fs);
}

PyrsStr *pyrs_format_bool(int v, const PyrsStr *spec) {
    check_ref(spec);
    /* Empty format → "True"/"False"; any non-empty spec uses int formatting. */
    if (spec->len == 0) {
        return pyrs_str_from_bool(v);
    }
    return pyrs_format_int(pyrs_int_from_i64(v ? 1LL : 0LL), spec);
}

/* CPython-style repr of a string into a newly allocated PyrsStr. */
PyrsStr *pyrs_str_repr(const PyrsStr *s) {
    check_ref(s);
    int has_single = 0;
    int has_double = 0;
    for (long long i = 0; i < s->len; i++) {
        if (s->data[i] == '\'') {
            has_single = 1;
        } else if (s->data[i] == '"') {
            has_double = 1;
        }
    }
    char quote = (has_single && !has_double) ? '"' : '\'';

    /* worst case: every byte → \xHH (4 chars) + quotes */
    long long cap = s->len * 4 + 2;
    char *buf = xmalloc((size_t)cap + 1);
    long long n = 0;
    buf[n++] = quote;
    for (long long i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        if (c == (unsigned char)quote || c == '\\') {
            buf[n++] = '\\';
            buf[n++] = (char)c;
        } else if (c == '\n') {
            buf[n++] = '\\';
            buf[n++] = 'n';
        } else if (c == '\r') {
            buf[n++] = '\\';
            buf[n++] = 'r';
        } else if (c == '\t') {
            buf[n++] = '\\';
            buf[n++] = 't';
        } else if (c < 0x20 || c == 0x7f) {
            n += sprintf(buf + n, "\\x%02x", c);
        } else {
            buf[n++] = (char)c;
        }
    }
    buf[n++] = quote;
    buf[n] = '\0';
    PyrsStr *r = str_alloc(n);
    memcpy(r->data, buf, (size_t)n);
    free(buf);
    return r;
}

/* Decode one UTF-8 codepoint starting at s->data[i]; returns number of bytes
 * consumed (1 on invalid/truncated sequences, treating the byte as latin-1). */
static int utf8_next(const PyrsStr *s, long long i, unsigned int *cp) {
    unsigned char c = (unsigned char)s->data[i];
    if (c < 0x80) {
        *cp = c;
        return 1;
    }
    if ((c & 0xe0) == 0xc0 && i + 1 < s->len) {
        unsigned char c1 = (unsigned char)s->data[i + 1];
        if ((c1 & 0xc0) == 0x80) {
            *cp = ((unsigned int)(c & 0x1f) << 6) | (c1 & 0x3f);
            if (*cp >= 0x80) {
                return 2;
            }
        }
    } else if ((c & 0xf0) == 0xe0 && i + 2 < s->len) {
        unsigned char c1 = (unsigned char)s->data[i + 1];
        unsigned char c2 = (unsigned char)s->data[i + 2];
        if ((c1 & 0xc0) == 0x80 && (c2 & 0xc0) == 0x80) {
            *cp = ((unsigned int)(c & 0x0f) << 12) | ((unsigned int)(c1 & 0x3f) << 6) |
                  (c2 & 0x3f);
            if (*cp >= 0x800) {
                return 3;
            }
        }
    } else if ((c & 0xf8) == 0xf0 && i + 3 < s->len) {
        unsigned char c1 = (unsigned char)s->data[i + 1];
        unsigned char c2 = (unsigned char)s->data[i + 2];
        unsigned char c3 = (unsigned char)s->data[i + 3];
        if ((c1 & 0xc0) == 0x80 && (c2 & 0xc0) == 0x80 && (c3 & 0xc0) == 0x80) {
            *cp = ((unsigned int)(c & 0x07) << 18) | ((unsigned int)(c1 & 0x3f) << 12) |
                  ((unsigned int)(c2 & 0x3f) << 6) | (c3 & 0x3f);
            if (*cp >= 0x10000 && *cp <= 0x10ffff) {
                return 4;
            }
        }
    }
    *cp = c;
    return 1;
}

long long pyrs_str_ord(const PyrsStr *s) {
    check_ref(s);
    if (s->len == 0) {
        pyrs_die("TypeError: ord() expected a character, but string of length 0 found");
    }
    long long i = 0;
    long long nchars = 0;
    unsigned int last = 0;
    while (i < s->len) {
        unsigned int cp = 0;
        i += utf8_next(s, i, &cp);
        nchars++;
        last = cp;
    }
    if (nchars != 1) {
        char buf[96];
        snprintf(buf, sizeof buf,
                 "TypeError: ord() expected a character, but string of length %lld found",
                 nchars);
        pyrs_die(buf);
    }
    return pyrs_int_from_i64((long long)last);
}

PyrsStr *pyrs_chr(long long n) {
    if (!pyrs_int_is_small(n)) {
        pyrs_die("ValueError: chr() arg not in range(0x110000)");
    }
    long long v = pyrs_int_small_val(n);
    if (v < 0 || v > 0x10ffff) {
        pyrs_die("ValueError: chr() arg not in range(0x110000)");
    }
    unsigned int cp = (unsigned int)v;
    if (cp < 0x80) {
        return single_char((unsigned char)cp);
    }
    char buf[4];
    int nbytes;
    if (cp < 0x800) {
        buf[0] = (char)(0xc0 | (cp >> 6));
        buf[1] = (char)(0x80 | (cp & 0x3f));
        nbytes = 2;
    } else if (cp < 0x10000) {
        buf[0] = (char)(0xe0 | (cp >> 12));
        buf[1] = (char)(0x80 | ((cp >> 6) & 0x3f));
        buf[2] = (char)(0x80 | (cp & 0x3f));
        nbytes = 3;
    } else {
        buf[0] = (char)(0xf0 | (cp >> 18));
        buf[1] = (char)(0x80 | ((cp >> 12) & 0x3f));
        buf[2] = (char)(0x80 | ((cp >> 6) & 0x3f));
        buf[3] = (char)(0x80 | (cp & 0x3f));
        nbytes = 4;
    }
    PyrsStr *r = str_alloc(nbytes);
    memcpy(r->data, buf, (size_t)nbytes);
    return r;
}

/* ascii(): like repr but non-ASCII codepoints escaped (\xHH / \uXXXX / \UXXXXXXXX).
 * Source strings are UTF-8 bytes; we decode so café → 'caf\xe9' like CPython. */
PyrsStr *pyrs_str_ascii(const PyrsStr *s) {
    check_ref(s);
    int has_single = 0;
    int has_double = 0;
    for (long long i = 0; i < s->len; i++) {
        if (s->data[i] == '\'') {
            has_single = 1;
        } else if (s->data[i] == '"') {
            has_double = 1;
        }
    }
    char quote = (has_single && !has_double) ? '"' : '\'';

    /* worst case: every codepoint → \UXXXXXXXX (10 chars) + quotes */
    long long cap = s->len * 10 + 2;
    char *buf = xmalloc((size_t)cap + 1);
    long long n = 0;
    buf[n++] = quote;
    for (long long i = 0; i < s->len;) {
        unsigned int cp = 0;
        int adv = utf8_next(s, i, &cp);
        i += adv;
        if (cp == (unsigned int)quote || cp == '\\') {
            buf[n++] = '\\';
            buf[n++] = (char)cp;
        } else if (cp == '\n') {
            buf[n++] = '\\';
            buf[n++] = 'n';
        } else if (cp == '\r') {
            buf[n++] = '\\';
            buf[n++] = 'r';
        } else if (cp == '\t') {
            buf[n++] = '\\';
            buf[n++] = 't';
        } else if (cp < 0x20 || cp == 0x7f) {
            n += sprintf(buf + n, "\\x%02x", cp);
        } else if (cp < 0x80) {
            buf[n++] = (char)cp;
        } else if (cp < 0x100) {
            n += sprintf(buf + n, "\\x%02x", cp);
        } else if (cp < 0x10000) {
            n += sprintf(buf + n, "\\u%04x", cp);
        } else {
            n += sprintf(buf + n, "\\U%08x", cp);
        }
    }
    buf[n++] = quote;
    buf[n] = '\0';
    PyrsStr *r = str_alloc(n);
    memcpy(r->data, buf, (size_t)n);
    free(buf);
    return r;
}

PyrsStr *pyrs_format_str(const PyrsStr *s, const PyrsStr *spec) {
    check_ref(s);
    check_ref(spec);
    if (spec->len == 0) {
        /* Strings are immutable, so sharing the managed object is safe. */
        return (PyrsStr *)s;
    }

    PyrsFormatSpec fs;
    parse_format_spec(spec, &fs, "str");

    if (fs.sign != '\0' || fs.alternate || fs.zero) {
        /* CPython: '=' align / sign / # / 0 not allowed for strings in some
         * cases. Mirror common errors. */
        if (fs.sign != '\0') {
            pyrs_die("ValueError: Sign not allowed in string format specifier");
        }
        if (fs.alternate) {
            pyrs_die(
                "ValueError: Alternate form (#) not allowed in string format "
                "specifier");
        }
        if (fs.zero && fs.align == '=') {
            /* zero flag alone becomes fill=0 align== which is invalid for str */
            pyrs_die(
                "ValueError: '=' alignment not allowed in string format "
                "specifier");
        }
    }
    if (fs.align == '=') {
        pyrs_die(
            "ValueError: '=' alignment not allowed in string format specifier");
    }

    char type = fs.type ? fs.type : 's';
    if (type != 's') {
        format_die_unknown(type, "str");
    }

    const char *body = s->data;
    long long body_len = s->len;
    if (fs.precision >= 0 && fs.precision < body_len) {
        body_len = fs.precision;
    }

    if (fs.align == '\0') {
        fs.align = '<'; /* default for strings */
    }
    return format_pad("", "", body, body_len, &fs);
}

int pyrs_str_isdigit(const PyrsStr *s) {
    check_ref(s);
    if (s->len == 0) {
        return 0;
    }
    for (long long i = 0; i < s->len; i++) {
        if (s->data[i] < '0' || s->data[i] > '9') {
            return 0;
        }
    }

    return 1;
}

int pyrs_str_isalpha(const PyrsStr *s) {
    check_ref(s);
    if (s->len == 0) {
        return 0;
    }
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        if (!((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z'))) {
            return 0;
        }
    }
    return 1;
}

int pyrs_str_isspace(const PyrsStr *s) {
    check_ref(s);
    if (s->len == 0) {
        return 0;
    }
    for (long long i = 0; i < s->len; i++) {
        if (!is_py_space(s->data[i])) {
            return 0;
        }
    }
    return 1;
}

/* ASCII: at least one cased letter; all cased letters upper (or lower). */
int pyrs_str_isupper(const PyrsStr *s) {
    check_ref(s);
    int saw_cased = 0;
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        if (c >= 'a' && c <= 'z') {
            return 0;
        }
        if (c >= 'A' && c <= 'Z') {
            saw_cased = 1;
        }
    }
    return saw_cased;
}

int pyrs_str_islower(const PyrsStr *s) {
    check_ref(s);
    int saw_cased = 0;
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        if (c >= 'A' && c <= 'Z') {
            return 0;
        }
        if (c >= 'a' && c <= 'z') {
            saw_cased = 1;
        }
    }
    return saw_cased;
}

int pyrs_str_isalnum(const PyrsStr *s) {
    check_ref(s);
    if (s->len == 0) {
        return 0;
    }
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        int letter = (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
        int digit = c >= '0' && c <= '9';
        if (!letter && !digit) {
            return 0;
        }
    }
    return 1;
}

int pyrs_str_istitle(const PyrsStr *s) {
    check_ref(s);
    int saw_cased = 0;
    int prev_cased = 0;
    for (long long i = 0; i < s->len; i++) {
        char c = s->data[i];
        int upper = c >= 'A' && c <= 'Z';
        int lower = c >= 'a' && c <= 'z';
        if (upper || lower) {
            if (prev_cased) {
                if (upper) {
                    return 0;
                }
            } else if (lower) {
                return 0;
            }
            saw_cased = 1;
            prev_cased = 1;
        } else {
            prev_cased = 0;
        }
    }
    return saw_cased;
}

int pyrs_str_isascii(const PyrsStr *s) {
    check_ref(s);
    for (long long i = 0; i < s->len; i++) {
        if ((unsigned char)s->data[i] >= 128) {
            return 0;
        }
    }
    return 1;
}

/* ---- lists ---- */

PyrsList *pyrs_list_new(long long cap) {
    if (cap < 4) {
        cap = 4;
    }
    PyrsList *l = pyrs_gc_alloc(sizeof(PyrsList), PYRS_GC_LIST);
    l->len = 0;
    l->cap = cap;
    l->data = xmalloc((size_t)cap * sizeof(long long));
    pyrs_gc_external_allocated(l, (size_t)cap * sizeof(long long));
    return l;
}

void pyrs_list_push(PyrsList *l, long long slot) {
    check_ref(l);
    if (l->len == l->cap) {
        long long cap = l->cap * 2;
        long long *data = xmalloc((size_t)cap * sizeof(long long));
        pyrs_gc_external_allocated(l, (size_t)cap * sizeof(long long));
        memcpy(data, l->data, (size_t)l->len * sizeof(long long));
        pyrs_gc_external_freed(l,
                               (size_t)l->cap * sizeof(long long));
        free(l->data);
        l->data = data;
        l->cap = cap;
    }
    l->data[l->len++] = slot;
}

/* In-place extend: append all slots from src onto dst (same element encoding). */
void pyrs_list_extend(PyrsList *dst, const PyrsList *src) {
    check_ref(dst);
    check_ref(src);
    for (long long i = 0; i < src->len; i++) {
        pyrs_list_push(dst, src->data[i]);
    }
}

/* new list: a then b (shallow copy of slots) */
PyrsList *pyrs_list_concat(const PyrsList *a, const PyrsList *b) {
    check_ref(a);
    check_ref(b);
    long long n = a->len + b->len;
    PyrsList *r = pyrs_list_new(n);
    if (a->len > 0) {
        memcpy(r->data, a->data, (size_t)a->len * sizeof(long long));
    }
    if (b->len > 0) {
        memcpy(r->data + a->len, b->data, (size_t)b->len * sizeof(long long));
    }
    r->len = n;
    return r;
}

/* new list: a repeated n times; n <= 0 yields empty (like CPython) */
PyrsList *pyrs_list_repeat(const PyrsList *a, long long n) {
    check_ref(a);
    if (n <= 0 || a->len == 0) {
        return pyrs_list_new(0);
    }
    if (n > 0 && a->len > 0 && n > (LLONG_MAX / a->len)) {
        pyrs_die("MemoryError: list repeat too large");
    }
    long long total = a->len * n;
    PyrsList *r = pyrs_list_new(total);
    for (long long i = 0; i < n; i++) {
        memcpy(r->data + i * a->len, a->data, (size_t)a->len * sizeof(long long));
    }
    r->len = total;
    return r;
}

long long pyrs_list_get(const PyrsList *l, long long i) {
    check_ref(l);
    if (i < 0) {
        i += l->len;
    }
    if (i < 0 || i >= l->len) {
        pyrs_die("IndexError: list index out of range");
    }
    return l->data[i];
}

void pyrs_list_set(PyrsList *l, long long i, long long slot) {
    check_ref(l);
    if (i < 0) {
        i += l->len;
    }
    if (i < 0 || i >= l->len) {
        pyrs_die("IndexError: list assignment index out of range");
    }
    l->data[i] = slot;
}

PyrsList *pyrs_list_slice(const PyrsList *l, long long lo, long long hi, long long step) {
    check_ref(l);
    if (step == 0) {
        pyrs_die("ValueError: slice step cannot be zero");
    }
    long long start = resolve_slice_bound(lo, 1, l->len, step);
    long long stop = resolve_slice_bound(hi, 0, l->len, step);
    long long n = slice_count(start, stop, step);
    PyrsList *r = pyrs_list_new(n);
    if (n > 0) {
        if (step == 1) {
            memcpy(r->data, l->data + start, (size_t)n * sizeof(long long));
        } else {
            for (long long i = 0; i < n; i++) {
                r->data[i] = l->data[start + i * step];
            }
        }
        r->len = n;
    }
    return r;
}

static void list_ensure_cap(PyrsList *l, long long need) {
    if (need <= l->cap) {
        return;
    }
    long long cap = l->cap < 4 ? 4 : l->cap;
    while (cap < need) {
        cap *= 2;
    }
    long long *data = xmalloc((size_t)cap * sizeof(long long));
    pyrs_gc_external_allocated(l, (size_t)cap * sizeof(long long));
    if (l->len > 0) {
        memcpy(data, l->data, (size_t)l->len * sizeof(long long));
    }
    pyrs_gc_external_freed(l, (size_t)l->cap * sizeof(long long));
    free(l->data);
    l->data = data;
    l->cap = cap;
}

/* CPython list slice assignment. `src` is the replacement sequence. */
void pyrs_list_set_slice(PyrsList *dst, long long lo, long long hi, long long step,
                         const PyrsList *src) {
    check_ref(dst);
    check_ref(src);
    if (step == 0) {
        pyrs_die("ValueError: slice step cannot be zero");
    }
    PyrsList *owned = NULL;
    if (dst == src) {
        owned = pyrs_list_copy(src);
        src = owned;
    }
    long long start = resolve_slice_bound(lo, 1, dst->len, step);
    long long stop = resolve_slice_bound(hi, 0, dst->len, step);
    long long nrepl = slice_count(start, stop, step);
    long long nsrc = src->len;
    if (step != 1) {
        if (nsrc != nrepl) {
            char buf[128];
            snprintf(buf, sizeof buf,
                     "ValueError: attempt to assign sequence of size %lld to "
                     "extended slice of size %lld",
                     nsrc, nrepl);
            pyrs_die(buf);
        }
        for (long long i = 0; i < nsrc; i++) {
            dst->data[start + i * step] = src->data[i];
        }
        return;
    }
    long long tail = start + nrepl;
    long long new_len = dst->len - nrepl + nsrc;
    list_ensure_cap(dst, new_len);
    if (nsrc != nrepl && tail < dst->len) {
        memmove(&dst->data[start + nsrc], &dst->data[tail],
                (size_t)(dst->len - tail) * sizeof(long long));
    }
    if (nsrc > 0) {
        memcpy(&dst->data[start], src->data, (size_t)nsrc * sizeof(long long));
    }
    dst->len = new_len;
}

/* element tags match codegen: 0=int 1=float 2=bool 3=str;
 * list-of-X is 4 + 8 * tag(X) (recursive); 5=tuple 6=dict 7=set. */
static int slot_eq(long long a, long long b, int tag);
int pyrs_list_eq(const PyrsList *a, const PyrsList *b, int tag);
int pyrs_tuple_eq(const PyrsTuple *a, const PyrsTuple *b);
int pyrs_dict_eq(const PyrsDict *a, const PyrsDict *b);
int pyrs_set_eq(const PyrsSet *a, const PyrsSet *b);

static int slot_eq(long long a, long long b, int tag) {
    if (tag == TAG_TUPLE) {
        return pyrs_tuple_eq((const PyrsTuple *)(uintptr_t)a, (const PyrsTuple *)(uintptr_t)b);
    }
    if (tag == TAG_DICT) {
        return pyrs_dict_eq((const PyrsDict *)(uintptr_t)a, (const PyrsDict *)(uintptr_t)b);
    }
    if (tag == TAG_SET) {
        return pyrs_set_eq((const PyrsSet *)(uintptr_t)a, (const PyrsSet *)(uintptr_t)b);
    }
    /* list[Any] / union boxes: recursive eq on boxed print_tag + payload.
     * CPython: True == 1, so bool and int boxes compare numerically. */
    if (tag == TAG_UNION) {
        if (a == 0 && b == 0) {
            return 1;
        }
        if (a == 0 || b == 0) {
            return 0;
        }
        const PyrsUnionBox *ua = (const PyrsUnionBox *)(uintptr_t)a;
        const PyrsUnionBox *ub = (const PyrsUnionBox *)(uintptr_t)b;
        if (ua->print_tag < 0 && ub->print_tag < 0) {
            return 1; /* both None */
        }
        if (ua->print_tag == ub->print_tag) {
            if (ua->print_tag < 0) {
                return 1;
            }
            return slot_eq(ua->payload, ub->payload, ua->print_tag);
        }
        /* Cross-tag: bool ↔ int (True == 1). Bool payload is 0/1; int is tagged. */
        if (ua->print_tag == TAG_BOOL && ub->print_tag == TAG_INT) {
            long long bi = ua->payload ? 3 : 1; /* tagged small 1 or 0 */
            return pyrs_int_eq(bi, ub->payload);
        }
        if (ua->print_tag == TAG_INT && ub->print_tag == TAG_BOOL) {
            long long bi = ub->payload ? 3 : 1;
            return pyrs_int_eq(ua->payload, bi);
        }
        return 0;
    }
    if (tag >= 4 && ((tag - 4) % 8) == 0) {
        /* nested list: slots are list pointers; inner tag = (tag-4)/8 */
        int inner = (tag - 4) / 8;
        return pyrs_list_eq((const PyrsList *)(uintptr_t)a, (const PyrsList *)(uintptr_t)b, inner);
    }
    switch (tag) {
    case 0:
        return pyrs_int_eq(a, b);
    case 2:
        return a == b;
    case 1: {
        /* numeric equality: 0.0 == -0.0, nan != nan */
        double x, y;
        memcpy(&x, &a, sizeof x);
        memcpy(&y, &b, sizeof y);
        return x == y;
    }
    case 3:
        return pyrs_str_cmp((const PyrsStr *)a, (const PyrsStr *)b) == 0;
    default:
        /* Class instances (13+8*id) and other pointer-like tags: identity. */
        if (tag >= TAG_CLASS_BASE && ((tag - TAG_CLASS_BASE) % 8) == 0) {
            return a == b;
        }
        return 0;
    }
}

/* element-wise equality; used by == and by nested slot_eq */
int pyrs_list_eq(const PyrsList *a, const PyrsList *b, int tag) {
    check_ref(a);
    check_ref(b);
    if (a->len != b->len) {
        return 0;
    }
    for (long long i = 0; i < a->len; i++) {
        if (!slot_eq(a->data[i], b->data[i], tag)) {
            return 0;
        }
    }
    return 1;
}

/* List tags: 4 + 8 * elem_tag (codegen elem_tag). */
static int is_list_tag(int tag) {
    return tag >= 4 && ((tag - 4) % 8) == 0;
}

/* Lexicographic list order; tag is the element print-tag. Forward for
 * slot_ord_cmp recursion into nested lists. */
int pyrs_list_cmp(const PyrsList *a, const PyrsList *b, int tag);

int pyrs_list_contains(const PyrsList *l, long long slot, int tag) {
    check_ref(l);
    for (long long i = 0; i < l->len; i++) {
        if (slot_eq(l->data[i], slot, tag)) {
            return 1;
        }
    }
    return 0;
}

void pyrs_list_insert(PyrsList *l, long long i, long long slot) {
    check_ref(l);
    /* CPython: clamp index into [0, len] after negative adjustment */
    if (i < 0) {
        i += l->len;
        if (i < 0) {
            i = 0;
        }
    }
    if (i > l->len) {
        i = l->len;
    }
    if (l->len == l->cap) {
        long long cap = l->cap < 4 ? 4 : l->cap * 2;
        long long *data = xmalloc((size_t)cap * sizeof(long long));
        pyrs_gc_external_allocated(l, (size_t)cap * sizeof(long long));
        memcpy(data, l->data, (size_t)l->len * sizeof(long long));
        pyrs_gc_external_freed(l,
                               (size_t)l->cap * sizeof(long long));
        free(l->data);
        l->data = data;
        l->cap = cap;
    }
    memmove(&l->data[i + 1], &l->data[i],
            (size_t)(l->len - i) * sizeof(long long));
    l->data[i] = slot;
    l->len++;
}

void pyrs_list_remove(PyrsList *l, long long slot, int tag) {
    check_ref(l);
    for (long long i = 0; i < l->len; i++) {
        if (slot_eq(l->data[i], slot, tag)) {
            memmove(&l->data[i], &l->data[i + 1],
                    (size_t)(l->len - i - 1) * sizeof(long long));
            l->len--;
            return;
        }
    }
    pyrs_die("ValueError: list.remove(x): x not in list");
}

long long pyrs_list_index(const PyrsList *l, long long slot, int tag) {
    check_ref(l);
    for (long long i = 0; i < l->len; i++) {
        if (slot_eq(l->data[i], slot, tag)) {
            return i;
        }
    }
    pyrs_die("ValueError: list.index(x): x not in list");
}

long long pyrs_list_count(const PyrsList *l, long long slot, int tag) {
    check_ref(l);
    long long n = 0;
    for (long long i = 0; i < l->len; i++) {
        if (slot_eq(l->data[i], slot, tag)) {
            n++;
        }
    }
    return n;
}

void pyrs_list_clear(PyrsList *l) {
    check_ref(l);
    l->len = 0;
}

void pyrs_list_reverse(PyrsList *l) {
    check_ref(l);
    long long i = 0;
    long long j = l->len - 1;
    while (i < j) {
        long long tmp = l->data[i];
        l->data[i] = l->data[j];
        l->data[j] = tmp;
        i++;
        j--;
    }
}

/* qsort needs a tag; single-threaded compiler runtime is fine */
static int sort_elem_tag;
/* Forward: defined with tuple helpers below (used by list_sort for tag 5). */
int pyrs_tuple_cmp(const PyrsTuple *a, const PyrsTuple *b);

static int cmp_slots_qsort(const void *pa, const void *pb) {
    long long a = *(const long long *)pa;
    long long b = *(const long long *)pb;
    int tag = sort_elem_tag;
    switch (tag) {
    case 0: /* int (tagged / heap) */
        return pyrs_int_cmp(a, b);
    case 2: /* bool as 0/1 */
        return (a > b) - (a < b);
    case 1: { /* float: total order, NaN last */
        double x, y;
        memcpy(&x, &a, sizeof x);
        memcpy(&y, &b, sizeof y);
        int nx = isnan(x);
        int ny = isnan(y);
        if (nx && ny) {
            return 0;
        }
        if (nx) {
            return 1;
        }
        if (ny) {
            return -1;
        }
        return (x > y) - (x < y);
    }
    case 3:
        return pyrs_str_cmp((const PyrsStr *)a, (const PyrsStr *)b);
    case TAG_TUPLE:
        return pyrs_tuple_cmp((const PyrsTuple *)(uintptr_t)a, (const PyrsTuple *)(uintptr_t)b);
    default:
        if (is_list_tag(tag)) {
            int inner = (tag - 4) / 8;
            return pyrs_list_cmp((const PyrsList *)(uintptr_t)a, (const PyrsList *)(uintptr_t)b,
                                inner);
        }
        return 0;
    }
}

void pyrs_list_sort(PyrsList *l, int tag) {
    check_ref(l);
    if (l->len < 2) {
        return;
    }
    sort_elem_tag = tag;
    qsort(l->data, (size_t)l->len, sizeof(long long), cmp_slots_qsort);
}

void pyrs_list_del(PyrsList *l, long long i) {
    check_ref(l);
    if (i < 0) {
        i += l->len;
    }
    if (i < 0 || i >= l->len) {
        pyrs_die("IndexError: list assignment index out of range");
    }
    memmove(&l->data[i], &l->data[i + 1],
            (size_t)(l->len - i - 1) * sizeof(long long));
    l->len--;
}

long long pyrs_list_pop(PyrsList *l, long long i) {
    check_ref(l);
    if (l->len == 0) {
        pyrs_die("IndexError: pop from empty list");
    }
    if (i < 0) {
        i += l->len;
    }
    if (i < 0 || i >= l->len) {
        pyrs_die("IndexError: pop index out of range");
    }
    long long v = l->data[i];
    memmove(&l->data[i], &l->data[i + 1],
            (size_t)(l->len - i - 1) * sizeof(long long));
    l->len--;
    return v;
}

/* ---- float floored division & modulo (CPython float_divmod) ---- */

/* the remainder takes the divisor's sign; a zero remainder is signed like
 * the divisor (4.0 % -2.0 == -0.0) */
double pyrs_fmod_floored(double vx, double wx) {
    double mod = fmod(vx, wx);
    if (mod != 0.0) {
        if ((wx < 0) != (mod < 0)) {
            mod += wx;
        }
    } else {
        mod = copysign(0.0, wx);
    }
    return mod;
}

/* exact floored quotient, correct where floor(vx/wx) is not
 * (e.g. -1.0 // inf == -1.0, not -0.0) */
double pyrs_ffloordiv(double vx, double wx) {
    double mod = fmod(vx, wx);
    double div = (vx - mod) / wx;
    if (mod != 0.0 && (wx < 0) != (mod < 0)) {
        div -= 1.0;
    }
    double floordiv;
    if (div != 0.0) {
        floordiv = floor(div);
        if (div - floordiv > 0.5) {
            floordiv += 1.0;
        }
    } else {
        floordiv = copysign(0.0, vx / wx);
    }
    return floordiv;
}

/* ---- files ---- */

typedef struct {
    FILE *fp;
    const PyrsStr *name;
    int readable;
    int writable;
    int closed;
} PyrsFile;

/* uncaught-exception message matching what CPython's traceback ends with */
static _Noreturn void die_os_error(int err, const PyrsStr *path) {
    const char *exc;
    switch (err) {
    case ENOENT:
        exc = "FileNotFoundError";
        break;
    case EACCES:
        exc = "PermissionError";
        break;
    case EISDIR:
        exc = "IsADirectoryError";
        break;
    default:
        exc = "OSError";
        break;
    }
    char buf[512];
    snprintf(buf, sizeof buf, "%s: [Errno %d] %s: '%.*s'", exc, err,
             strerror(err), (int)path->len, path->data);
    pyrs_die(buf);
}

PyrsFile *pyrs_open(const PyrsStr *path, const PyrsStr *mode) {
    check_ref(path);
    check_ref(mode);

    int readable = 0;
    int writable = 0;
    const char *cmode;
    if (mode->len == 1 && mode->data[0] == 'r') {
        cmode = "r";
        readable = 1;
    } else if (mode->len == 1 && mode->data[0] == 'w') {
        cmode = "w";
        writable = 1;
    } else if (mode->len == 1 && mode->data[0] == 'a') {
        cmode = "a";
        writable = 1;
    } else {
        char buf[128];
        snprintf(buf, sizeof buf, "ValueError: invalid mode: '%.*s'",
                 (int)mode->len, mode->data);
        pyrs_die(buf);
    }

    FILE *fp = fopen(path->data, cmode);
    if (fp == NULL) {
        die_os_error(errno, path);
    }
    /* Linux fopen("dir", "r") succeeds; Python raises at open() */
    if (readable) {
        struct stat st;
        if (fstat(fileno(fp), &st) == 0 && S_ISDIR(st.st_mode)) {
            fclose(fp);
            die_os_error(EISDIR, path);
        }
    }

    PyrsFile *f = pyrs_gc_alloc(sizeof(PyrsFile), PYRS_GC_FILE);
    f->fp = fp;
    f->name = path;
    f->readable = readable;
    f->writable = writable;
    f->closed = 0;
    return f;
}

static void file_check_open(const PyrsFile *f) {
    check_ref(f);
    if (f->closed) {
        pyrs_die("ValueError: I/O operation on closed file.");
    }
}

static void file_check_readable(const PyrsFile *f) {
    file_check_open(f);
    if (!f->readable) {
        pyrs_die("io.UnsupportedOperation: not readable");
    }
}

/* everything remaining in the file */
PyrsStr *pyrs_file_read(PyrsFile *f) {
    file_check_readable(f);
    PyrsGcRoot file_root;
    pyrs_gc_root_push(&file_root, &f, sizeof(f));
    size_t cap = 1 << 16;
    size_t len = 0;
    char *buf = xmalloc(cap);
    for (;;) {
        size_t n = fread(buf + len, 1, cap - len, f->fp);
        len += n;
        if (len < cap) {
            break;
        }
        size_t newcap = cap * 2;
        char *bigger = xmalloc(newcap);
        memcpy(bigger, buf, len);
        free(buf);
        buf = bigger;
        cap = newcap;
    }
    PyrsStr *r = str_alloc((long long)len);
    memcpy(r->data, buf, len);
    free(buf);
    pyrs_gc_root_pop(&file_root);
    return r;
}

/* one line, keeping the trailing newline; "" at EOF (like Python) */
PyrsStr *pyrs_file_readline(PyrsFile *f) {
    file_check_readable(f);
    char *line = NULL;
    size_t cap = 0;
    ssize_t n = getline(&line, &cap, f->fp);
    if (n < 0) {
        free(line);
        return EMPTY_STR;
    }
    PyrsStr *r = str_alloc(n);
    memcpy(r->data, line, (size_t)n);
    free(line);
    return r;
}

PyrsList *pyrs_file_readlines(PyrsFile *f) {
    file_check_readable(f);
    PyrsGcRoot file_root;
    pyrs_gc_root_push(&file_root, &f, sizeof(f));
    PyrsList *out = pyrs_list_new(8);
    PyrsGcRoot out_root;
    pyrs_gc_root_push(&out_root, &out, sizeof(out));
    for (;;) {
        PyrsStr *line = pyrs_file_readline(f);
        if (line->len == 0) {
            break;
        }
        pyrs_list_push(out, (long long)line);
    }
    pyrs_gc_root_pop(&out_root);
    pyrs_gc_root_pop(&file_root);
    return out;
}

/* returns the number of characters written, like Python; flushed so data
 * is visible immediately even when deterministic close is omitted */
long long pyrs_file_write(PyrsFile *f, const PyrsStr *s) {
    file_check_open(f);
    if (!f->writable) {
        pyrs_die("io.UnsupportedOperation: not writable");
    }
    check_ref(s);
    fwrite(s->data, 1, (size_t)s->len, f->fp);
    fflush(f->fp);
    return s->len;
}

/* idempotent, like Python */
void pyrs_file_close(PyrsFile *f) {
    check_ref(f);
    if (!f->closed) {
        fclose(f->fp);
        f->closed = 1;
    }
}

/* ---- stdin & command-line arguments ---- */

static int g_argc = 0;
static char **g_argv = NULL;
static PyrsList *g_argv_cached = NULL;
static int g_argv_root_registered = 0;

void pyrs_set_args(int argc, char **argv) {
    g_argc = argc;
    g_argv = argv;
}

static PyrsStr *str_from_cstr(const char *c) {
    size_t n = strlen(c);
    if (n == 0) {
        return EMPTY_STR;
    }
    PyrsStr *r = str_alloc((long long)n);
    memcpy(r->data, c, n);
    return r;
}

/* sys.argv: built once so repeated accesses alias, like Python */
PyrsList *pyrs_argv(void) {
    if (!g_argv_root_registered) {
        /* Register before the first list allocation: stress mode may collect
         * while the cached list is being populated. */
        pyrs_gc_add_root_range(&g_argv_cached, sizeof(g_argv_cached));
        g_argv_root_registered = 1;
    }
    if (g_argv_cached == NULL) {
        g_argv_cached = pyrs_list_new(g_argc > 0 ? g_argc : 1);
        for (int i = 0; i < g_argc; i++) {
            pyrs_list_push(g_argv_cached,
                           (long long)str_from_cstr(g_argv[i]));
        }
    }
    return g_argv_cached;
}

/* input([prompt]): print the prompt (no newline), read a line, strip the
 * trailing newline; EOF raises like Python */
PyrsStr *pyrs_input(const PyrsStr *prompt) {
    if (prompt != NULL) {
        pyrs_print_str(prompt);
        fflush(stdout);
    }
    char *line = NULL;
    size_t cap = 0;
    ssize_t n = getline(&line, &cap, stdin);
    if (n < 0) {
        free(line);
        pyrs_die("EOFError: EOF when reading a line");
    }
    if (n > 0 && line[n - 1] == '\n') {
        n--;
    }
    PyrsStr *r = str_alloc(n);
    memcpy(r->data, line, (size_t)n);
    free(line);
    return r;
}

/* ---- integer power: see pyrs_int_pow / pyrs_ipow in bigint_impl.c ---- */

/* ---- tuples ---- */

/* Self-describing: each element carries its print/eq tag. Layout:
 *   { i64 len; i64 *data; int *tags; }
 * First field is len so pyrs_len works. */
struct PyrsTuple {
    long long len;
    long long *data;
    int *tags;
};

static int str_affix_tuple(const PyrsStr *s, const PyrsTuple *t, long long start, long long end,
                           int from_end, const char *name) {
    check_ref(s);
    check_ref(t);
    for (long long i = 0; i < t->len; i++) {
        if (t->tags[i] != TAG_STR) {
            char buf[96];
            snprintf(buf, sizeof buf,
                     "TypeError: tuple for %s must only contain str, not other", name);
            pyrs_die(buf);
        }
        const PyrsStr *aff = (const PyrsStr *)(uintptr_t)t->data[i];
        if (str_affix_in_slice(s, aff, start, end, from_end)) {
            return 1;
        }
    }
    return 0;
}

int pyrs_str_startswith_tuple(const PyrsStr *s, const PyrsTuple *t, long long start,
                              long long end) {
    return str_affix_tuple(s, t, start, end, 0, "startswith");
}

int pyrs_str_endswith_tuple(const PyrsStr *s, const PyrsTuple *t, long long start,
                            long long end) {
    return str_affix_tuple(s, t, start, end, 1, "endswith");
}

PyrsTuple *pyrs_tuple_new(long long n) {
    if (n < 0) {
        n = 0;
    }
    PyrsTuple *t = pyrs_gc_alloc(sizeof(PyrsTuple), PYRS_GC_TUPLE);
    t->len = n;
    t->data = n > 0 ? xmalloc((size_t)n * sizeof(long long)) : NULL;
    if (n > 0) {
        pyrs_gc_external_allocated(t, (size_t)n * sizeof(long long));
        memset(t->data, 0, (size_t)n * sizeof(long long));
    }
    t->tags = n > 0 ? xmalloc((size_t)n * sizeof(int)) : NULL;
    if (n > 0) {
        pyrs_gc_external_allocated(t, (size_t)n * sizeof(int));
        memset(t->tags, 0, (size_t)n * sizeof(int));
    }
    return t;
}

void pyrs_tuple_set(PyrsTuple *t, long long i, long long slot, int tag) {
    check_ref(t);
    if (i < 0 || i >= t->len) {
        pyrs_die("IndexError: tuple assignment index out of range");
    }
    t->data[i] = slot;
    t->tags[i] = tag;
}

static PyrsStr *str_slice_copy(const PyrsStr *s, long long lo, long long hi) {
    long long n = hi - lo;
    PyrsStr *r = str_alloc(n);
    memcpy(r->data, s->data + lo, (size_t)n);
    return r;
}

static PyrsTuple *str_parts3(PyrsStr *a, PyrsStr *b, PyrsStr *c) {
    PyrsTuple *t = pyrs_tuple_new(3);
    pyrs_tuple_set(t, 0, (long long)(uintptr_t)a, TAG_STR);
    pyrs_tuple_set(t, 1, (long long)(uintptr_t)b, TAG_STR);
    pyrs_tuple_set(t, 2, (long long)(uintptr_t)c, TAG_STR);
    return t;
}

PyrsTuple *pyrs_str_partition(const PyrsStr *s, const PyrsStr *sep) {
    check_ref(s);
    check_ref(sep);
    if (sep->len == 0) {
        pyrs_die("ValueError: empty separator");
    }
    long long idx = pyrs_str_find(s, sep);
    if (idx < 0) {
        PyrsStr *empty = str_alloc(0);
        return str_parts3((PyrsStr *)s, empty, empty);
    }
    return str_parts3(str_slice_copy(s, 0, idx), (PyrsStr *)sep,
                      str_slice_copy(s, idx + sep->len, s->len));
}

PyrsTuple *pyrs_str_rpartition(const PyrsStr *s, const PyrsStr *sep) {
    check_ref(s);
    check_ref(sep);
    if (sep->len == 0) {
        pyrs_die("ValueError: empty separator");
    }
    long long idx = pyrs_str_rfind(s, sep);
    if (idx < 0) {
        PyrsStr *empty = str_alloc(0);
        return str_parts3(empty, empty, (PyrsStr *)s);
    }
    return str_parts3(str_slice_copy(s, 0, idx), (PyrsStr *)sep,
                      str_slice_copy(s, idx + sep->len, s->len));
}

long long pyrs_tuple_get(const PyrsTuple *t, long long i) {
    check_ref(t);
    if (i < 0) {
        i += t->len;
    }
    if (i < 0 || i >= t->len) {
        pyrs_die("IndexError: tuple index out of range");
    }
    return t->data[i];
}

void pyrs_print_tuple(const PyrsTuple *t) {
    check_ref(t);
    fputc('(', stdout);
    for (long long i = 0; i < t->len; i++) {
        if (i > 0) {
            fputs(", ", stdout);
        }
        print_slot(t->data[i], t->tags[i]);
    }
    if (t->len == 1) {
        fputc(',', stdout);
    }
    fputc(')', stdout);
}

int pyrs_tuple_eq(const PyrsTuple *a, const PyrsTuple *b) {
    check_ref(a);
    check_ref(b);
    if (a->len != b->len) {
        return 0;
    }
    for (long long i = 0; i < a->len; i++) {
        if (a->tags[i] != b->tags[i]) {
            return 0;
        }
        if (!slot_eq(a->data[i], b->data[i], a->tags[i])) {
            return 0;
        }
    }
    return 1;
}

/* Lexicographic ordering: negative if a < b, 0 if equal, positive if a > b.
 * Recurses into nested tuples; dies on incomparable element tags. */
static int slot_ord_cmp(long long a, long long b, int tag);

static int slot_ord_cmp(long long a, long long b, int tag) {
    switch (tag) {
    case TAG_INT:
        return pyrs_int_cmp(a, b);
    case TAG_FLOAT: {
        double x, y;
        memcpy(&x, &a, sizeof x);
        memcpy(&y, &b, sizeof y);
        /* Match float binary compares: unordered NaN yields 0 here so neither
         * side is strictly less (min/max keep the left/first operand). */
        if (isnan(x) || isnan(y)) {
            if (isnan(x) && isnan(y)) {
                return 0;
            }
            return 0;
        }
        return (x > y) - (x < y);
    }
    case TAG_BOOL:
        return (a > b) - (a < b);
    case TAG_STR:
        return pyrs_str_cmp((const PyrsStr *)(uintptr_t)a, (const PyrsStr *)(uintptr_t)b);
    case TAG_TUPLE:
        return pyrs_tuple_cmp((const PyrsTuple *)(uintptr_t)a, (const PyrsTuple *)(uintptr_t)b);
    default:
        if (is_list_tag(tag)) {
            int inner = (tag - 4) / 8;
            return pyrs_list_cmp((const PyrsList *)(uintptr_t)a, (const PyrsList *)(uintptr_t)b,
                                inner);
        }
        pyrs_die("TypeError: '<' not supported between these types");
    }
}

/* Lexicographic: negative if a < b, 0 if equal, positive if a > b. */
int pyrs_list_cmp(const PyrsList *a, const PyrsList *b, int tag) {
    check_ref(a);
    check_ref(b);
    long long n = a->len < b->len ? a->len : b->len;
    for (long long i = 0; i < n; i++) {
        int c = slot_ord_cmp(a->data[i], b->data[i], tag);
        if (c != 0) {
            return c;
        }
    }
    if (a->len < b->len) {
        return -1;
    }
    if (a->len > b->len) {
        return 1;
    }
    return 0;
}

int pyrs_tuple_cmp(const PyrsTuple *a, const PyrsTuple *b) {
    check_ref(a);
    check_ref(b);
    long long n = a->len < b->len ? a->len : b->len;
    for (long long i = 0; i < n; i++) {
        if (a->tags[i] != b->tags[i]) {
            pyrs_die("TypeError: '<' not supported between instances of different types");
        }
        int c = slot_ord_cmp(a->data[i], b->data[i], a->tags[i]);
        if (c != 0) {
            return c;
        }
    }
    if (a->len < b->len) {
        return -1;
    }
    if (a->len > b->len) {
        return 1;
    }
    return 0;
}

/* Membership: only compare elements whose tag matches the needle tag. */
int pyrs_tuple_contains(const PyrsTuple *t, long long slot, int tag) {
    check_ref(t);
    for (long long i = 0; i < t->len; i++) {
        if (t->tags[i] == tag && slot_eq(t->data[i], slot, tag)) {
            return 1;
        }
    }
    return 0;
}

void pyrs_unpack_check(long long got, long long expected) {
    if (got < expected) {
        char buf[128];
        snprintf(buf, sizeof buf,
                 "ValueError: not enough values to unpack (expected %lld, got %lld)",
                 expected, got);
        pyrs_die(buf);
    }
    if (got > expected) {
        char buf[128];
        snprintf(buf, sizeof buf,
                 "ValueError: too many values to unpack (expected %lld, got %lld)",
                 expected, got);
        pyrs_die(buf);
    }
}

void pyrs_unpack_check_min(long long got, long long minimum) {
    if (got < minimum) {
        char buf[160];
        snprintf(buf, sizeof buf,
                 "ValueError: not enough values to unpack (expected at least %lld, got %lld)",
                 minimum, got);
        pyrs_die(buf);
    }
}

/* ---- dicts (open addressing + insertion order; keys: int/str) ---- */

typedef struct {
    long long key;
    long long val;
    int key_tag;
    int val_tag;
    unsigned char state; /* 0 empty, 1 full, 2 tomb */
} DictSlot;

struct PyrsDict {
    long long len; /* item count — first field for pyrs_len */
    long long cap;
    DictSlot *table;
    long long *order; /* table indices in insertion order */
    long long order_len;
    long long order_cap;
};


static void die_keyerror_int(long long key) {
    long long n;
    char *s = int_to_dec(key, &n);
    char buf[512];
    snprintf(buf, sizeof buf, "KeyError: %.496s", s);
    free(s);
    pyrs_die(buf);
}

static unsigned long long hash_key(long long key, int tag) {
    if (tag == TAG_INT) {
        return pyrs_int_hash(key);
    }
    if (tag == TAG_STR) {
        const PyrsStr *s = (const PyrsStr *)(uintptr_t)key;
        unsigned long long h = 14695981039346656037ULL;
        for (long long i = 0; i < s->len; i++) {
            h ^= (unsigned char)s->data[i];
            h *= 1099511628211ULL;
        }
        return h;
    }
    pyrs_die("TypeError: unhashable dict/set key tag");
    return 0;
}

static int key_eq(long long a, int at, long long b, int bt) {
    if (at != bt) {
        return 0;
    }
    return slot_eq(a, b, at);
}

PyrsDict *pyrs_dict_new(void) {
    PyrsDict *d = pyrs_gc_alloc(sizeof(PyrsDict), PYRS_GC_DICT);
    d->len = 0;
    d->cap = 8;
    d->table = xmalloc((size_t)d->cap * sizeof(DictSlot));
    pyrs_gc_external_allocated(d, (size_t)d->cap * sizeof(DictSlot));
    memset(d->table, 0, (size_t)d->cap * sizeof(DictSlot));
    d->order_cap = 8;
    d->order_len = 0;
    d->order = xmalloc((size_t)d->order_cap * sizeof(long long));
    pyrs_gc_external_allocated(d,
                               (size_t)d->order_cap * sizeof(long long));
    return d;
}

static void dict_grow(PyrsDict *d);

static long long dict_lookup(const PyrsDict *d, long long key, int key_tag, int *found) {
    unsigned long long h = hash_key(key, key_tag);
    long long mask = d->cap - 1;
    long long i = (long long)(h & (unsigned long long)mask);
    long long tomb = -1;
    for (long long n = 0; n < d->cap; n++) {
        DictSlot *s = &d->table[i];
        if (s->state == 0) {
            *found = 0;
            return tomb >= 0 ? tomb : i;
        }
        if (s->state == 2) {
            if (tomb < 0) {
                tomb = i;
            }
        } else if (key_eq(s->key, s->key_tag, key, key_tag)) {
            *found = 1;
            return i;
        }
        i = (i + 1) & mask;
    }
    *found = 0;
    return tomb >= 0 ? tomb : 0;
}

static void dict_grow(PyrsDict *d) {
    long long old_cap = d->cap;
    DictSlot *old = d->table;
    long long *old_order = d->order;
    long long old_order_len = d->order_len;
    long long old_order_cap = d->order_cap;

    /* Keep the published object internally consistent if either native
     * allocation has to invoke an OOM collection. */
    long long new_cap = old_cap * 2;
    DictSlot *new_table =
        xmalloc((size_t)new_cap * sizeof(DictSlot));
    memset(new_table, 0, (size_t)new_cap * sizeof(DictSlot));
    long long *new_order =
        xmalloc((size_t)new_cap * sizeof(long long));

    d->cap = new_cap;
    d->table = new_table;
    d->order_cap = new_cap;
    d->order = new_order;
    pyrs_gc_external_allocated(d, (size_t)new_cap * sizeof(DictSlot));
    pyrs_gc_external_allocated(d,
                               (size_t)new_cap * sizeof(long long));
    d->order_len = 0;
    d->len = 0;

    for (long long k = 0; k < old_order_len; k++) {
        DictSlot *s = &old[old_order[k]];
        if (s->state == 1) {
            int found;
            long long idx = dict_lookup(d, s->key, s->key_tag, &found);
            d->table[idx] = *s;
            d->table[idx].state = 1;
            d->order[d->order_len++] = idx;
            d->len++;
        }
    }
    pyrs_gc_external_freed(d, (size_t)old_cap * sizeof(DictSlot));
    pyrs_gc_external_freed(d,
                           (size_t)old_order_cap * sizeof(long long));
    free(old);
    free(old_order);
}

void pyrs_dict_set(PyrsDict *d, long long key, int key_tag, long long val, int val_tag) {
    check_ref(d);
    if (d->len * 2 >= d->cap) {
        dict_grow(d);
    }
    int found;
    long long idx = dict_lookup(d, key, key_tag, &found);
    if (found) {
        d->table[idx].val = val;
        d->table[idx].val_tag = val_tag;
        return;
    }
    d->table[idx].key = key;
    d->table[idx].val = val;
    d->table[idx].key_tag = key_tag;
    d->table[idx].val_tag = val_tag;
    d->table[idx].state = 1;
    if (d->order_len == d->order_cap) {
        long long nc = d->order_cap * 2;
        long long *no = xmalloc((size_t)nc * sizeof(long long));
        pyrs_gc_external_allocated(d, (size_t)nc * sizeof(long long));
        memcpy(no, d->order, (size_t)d->order_len * sizeof(long long));
        pyrs_gc_external_freed(
            d, (size_t)d->order_cap * sizeof(long long));
        free(d->order);
        d->order = no;
        d->order_cap = nc;
    }
    d->order[d->order_len++] = idx;
    d->len++;
}

long long pyrs_dict_get(const PyrsDict *d, long long key, int key_tag) {
    check_ref(d);
    int found;
    long long idx = dict_lookup(d, key, key_tag, &found);
    if (!found) {
        /* KeyError message like CPython */
        if (key_tag == TAG_STR) {
            /* build KeyError: '...' using repr-ish single quotes for simple keys */
            const PyrsStr *s = (const PyrsStr *)(uintptr_t)key;
            char buf[256];
            if (s->len < 200) {
                snprintf(buf, sizeof buf, "KeyError: '%.*s'", (int)s->len, s->data);
            } else {
                snprintf(buf, sizeof buf, "KeyError");
            }
            pyrs_die(buf);
        } else {
            die_keyerror_int(key);
        }
    }
    return d->table[idx].val;
}

/* Insert `def` if missing; return the stored value (existing or `def`). */
long long pyrs_dict_setdefault(PyrsDict *d, long long key, int key_tag,
                               long long def, int val_tag) {
    check_ref(d);
    int found;
    long long idx = dict_lookup(d, key, key_tag, &found);
    if (found) {
        return d->table[idx].val;
    }
    pyrs_dict_set(d, key, key_tag, def, val_tag);
    return def;
}

/* returns 1 and writes *out if found; else 0 */
int pyrs_dict_get_default(const PyrsDict *d, long long key, int key_tag, long long *out) {
    check_ref(d);
    int found;
    long long idx = dict_lookup(d, key, key_tag, &found);
    if (!found) {
        return 0;
    }
    *out = d->table[idx].val;
    return 1;
}

void pyrs_dict_del(PyrsDict *d, long long key, int key_tag) {
    check_ref(d);
    int found;
    long long idx = dict_lookup(d, key, key_tag, &found);
    if (!found) {
        if (key_tag == TAG_STR) {
            const PyrsStr *s = (const PyrsStr *)(uintptr_t)key;
            char buf[256];
            snprintf(buf, sizeof buf, "KeyError: '%.*s'", (int)s->len, s->data);
            pyrs_die(buf);
        } else {
            die_keyerror_int(key);
        }
    }
    d->table[idx].state = 2;
    d->len--;
    /* remove from order */
    for (long long i = 0; i < d->order_len; i++) {
        if (d->order[i] == idx) {
            memmove(&d->order[i], &d->order[i + 1],
                    (size_t)(d->order_len - i - 1) * sizeof(long long));
            d->order_len--;
            break;
        }
    }
}

int pyrs_dict_contains(const PyrsDict *d, long long key, int key_tag) {
    check_ref(d);
    int found;
    dict_lookup(d, key, key_tag, &found);
    return found;
}

void pyrs_dict_clear(PyrsDict *d) {
    check_ref(d);
    memset(d->table, 0, (size_t)d->cap * sizeof(DictSlot));
    d->len = 0;
    d->order_len = 0;
}

/* Merge all keys from `other` into `d` (overwrite on collision). Same K/V tags. */
void pyrs_dict_update(PyrsDict *d, const PyrsDict *other) {
    check_ref(d);
    check_ref(other);
    for (long long i = 0; i < other->order_len; i++) {
        DictSlot *e = &other->table[other->order[i]];
        if (e->state == 1) {
            pyrs_dict_set(d, e->key, e->key_tag, e->val, e->val_tag);
        }
    }
}

long long pyrs_dict_pop(PyrsDict *d, long long key, int key_tag, int has_default,
                        long long default_slot, long long *out) {
    check_ref(d);
    int found;
    long long idx = dict_lookup(d, key, key_tag, &found);
    if (!found) {
        if (has_default) {
            *out = default_slot;
            return 1;
        }
        if (key_tag == TAG_STR) {
            const PyrsStr *s = (const PyrsStr *)(uintptr_t)key;
            char buf[256];
            snprintf(buf, sizeof buf, "KeyError: '%.*s'", (int)s->len, s->data);
            pyrs_die(buf);
        } else {
            die_keyerror_int(key);
        }
    }
    *out = d->table[idx].val;
    d->table[idx].state = 2;
    d->len--;
    for (long long i = 0; i < d->order_len; i++) {
        if (d->order[i] == idx) {
            memmove(&d->order[i], &d->order[i + 1],
                    (size_t)(d->order_len - i - 1) * sizeof(long long));
            d->order_len--;
            break;
        }
    }
    return 1;
}

PyrsList *pyrs_dict_keys(const PyrsDict *d) {
    check_ref(d);
    PyrsList *r = pyrs_list_new(d->len);
    for (long long i = 0; i < d->order_len; i++) {
        DictSlot *s = &d->table[d->order[i]];
        if (s->state == 1) {
            pyrs_list_push(r, s->key);
        }
    }
    return r;
}

PyrsList *pyrs_dict_values(const PyrsDict *d) {
    check_ref(d);
    PyrsList *r = pyrs_list_new(d->len);
    for (long long i = 0; i < d->order_len; i++) {
        DictSlot *s = &d->table[d->order[i]];
        if (s->state == 1) {
            pyrs_list_push(r, s->val);
        }
    }
    return r;
}

/* LIFO last-inserted pair; empty → KeyError like CPython. */
PyrsTuple *pyrs_dict_popitem(PyrsDict *d) {
    check_ref(d);
    for (long long i = d->order_len - 1; i >= 0; i--) {
        DictSlot *s = &d->table[d->order[i]];
        if (s->state != 1) {
            continue;
        }
        PyrsTuple *t = pyrs_tuple_new(2);
        pyrs_tuple_set(t, 0, s->key, s->key_tag);
        pyrs_tuple_set(t, 1, s->val, s->val_tag);
        s->state = 2;
        d->len--;
        memmove(&d->order[i], &d->order[i + 1],
                (size_t)(d->order_len - i - 1) * sizeof(long long));
        d->order_len--;
        return t;
    }
    pyrs_die("KeyError: 'popitem(): dictionary is empty'");
    return NULL;
}

/* items: list of 2-tuples */
PyrsList *pyrs_dict_items(const PyrsDict *d) {
    check_ref(d);
    PyrsList *r = pyrs_list_new(d->len);
    for (long long i = 0; i < d->order_len; i++) {
        DictSlot *s = &d->table[d->order[i]];
        if (s->state != 1) {
            continue;
        }
        PyrsTuple *t = pyrs_tuple_new(2);
        pyrs_tuple_set(t, 0, s->key, s->key_tag);
        pyrs_tuple_set(t, 1, s->val, s->val_tag);
        pyrs_list_push(r, (long long)(uintptr_t)t);
    }
    return r;
}

/* iteration support: get key at insertion-order position i; returns 0 if done */
int pyrs_dict_iter_key(const PyrsDict *d, long long i, long long *out_key) {
    check_ref(d);
    if (i < 0 || i >= d->order_len) {
        return 0;
    }
    DictSlot *s = &d->table[d->order[i]];
    if (s->state != 1) {
        return 0;
    }
    *out_key = s->key;
    return 1;
}

void pyrs_print_dict(const PyrsDict *d) {
    check_ref(d);
    fputc('{', stdout);
    int first = 1;
    for (long long i = 0; i < d->order_len; i++) {
        DictSlot *s = &d->table[d->order[i]];
        if (s->state != 1) {
            continue;
        }
        if (!first) {
            fputs(", ", stdout);
        }
        first = 0;
        print_slot(s->key, s->key_tag);
        fputs(": ", stdout);
        print_slot(s->val, s->val_tag);
    }
    fputc('}', stdout);
}

/* structural equality (order-independent; values compared with slot_eq) */
int pyrs_dict_eq(const PyrsDict *a, const PyrsDict *b) {
    check_ref(a);
    check_ref(b);
    if (a->len != b->len) {
        return 0;
    }
    for (long long i = 0; i < a->order_len; i++) {
        DictSlot *s = &a->table[a->order[i]];
        if (s->state != 1) {
            continue;
        }
        int found;
        long long idx = dict_lookup(b, s->key, s->key_tag, &found);
        if (!found) {
            return 0;
        }
        DictSlot *t = &b->table[idx];
        if (s->val_tag != t->val_tag || !slot_eq(s->val, t->val, s->val_tag)) {
            return 0;
        }
    }
    return 1;
}

/* ---- sets (same hash table shape as dict, values ignored) ---- */

typedef struct {
    long long key;
    int key_tag;
    unsigned char state;
} SetSlot;

struct PyrsSet {
    long long len;
    long long cap;
    SetSlot *table;
    long long *order;
    long long order_len;
    long long order_cap;
};

PyrsSet *pyrs_set_new(void) {
    PyrsSet *s = pyrs_gc_alloc(sizeof(PyrsSet), PYRS_GC_SET);
    s->len = 0;
    s->cap = 8;
    s->table = xmalloc((size_t)s->cap * sizeof(SetSlot));
    pyrs_gc_external_allocated(s, (size_t)s->cap * sizeof(SetSlot));
    memset(s->table, 0, (size_t)s->cap * sizeof(SetSlot));
    s->order_cap = 8;
    s->order_len = 0;
    s->order = xmalloc((size_t)s->order_cap * sizeof(long long));
    pyrs_gc_external_allocated(s,
                               (size_t)s->order_cap * sizeof(long long));
    return s;
}

static long long set_lookup(const PyrsSet *s, long long key, int key_tag, int *found) {
    unsigned long long h = hash_key(key, key_tag);
    long long mask = s->cap - 1;
    long long i = (long long)(h & (unsigned long long)mask);
    long long tomb = -1;
    for (long long n = 0; n < s->cap; n++) {
        SetSlot *e = &s->table[i];
        if (e->state == 0) {
            *found = 0;
            return tomb >= 0 ? tomb : i;
        }
        if (e->state == 2) {
            if (tomb < 0) {
                tomb = i;
            }
        } else if (key_eq(e->key, e->key_tag, key, key_tag)) {
            *found = 1;
            return i;
        }
        i = (i + 1) & mask;
    }
    *found = 0;
    return tomb >= 0 ? tomb : 0;
}

static void set_grow(PyrsSet *s) {
    SetSlot *old = s->table;
    long long *old_order = s->order;
    long long old_order_len = s->order_len;
    long long old_cap = s->cap;
    long long old_order_cap = s->order_cap;
    /* Publish the resized shape only after both allocations succeed; xmalloc
     * may run a collection on its first OOM attempt. */
    long long new_cap = old_cap * 2;
    SetSlot *new_table = xmalloc((size_t)new_cap * sizeof(SetSlot));
    memset(new_table, 0, (size_t)new_cap * sizeof(SetSlot));
    long long *new_order =
        xmalloc((size_t)new_cap * sizeof(long long));

    s->cap = new_cap;
    s->table = new_table;
    s->order_cap = new_cap;
    s->order = new_order;
    pyrs_gc_external_allocated(s, (size_t)new_cap * sizeof(SetSlot));
    pyrs_gc_external_allocated(s,
                               (size_t)new_cap * sizeof(long long));
    s->order_len = 0;
    s->len = 0;
    for (long long k = 0; k < old_order_len; k++) {
        SetSlot *e = &old[old_order[k]];
        if (e->state == 1) {
            int found;
            long long idx = set_lookup(s, e->key, e->key_tag, &found);
            s->table[idx] = *e;
            s->table[idx].state = 1;
            s->order[s->order_len++] = idx;
            s->len++;
        }
    }
    pyrs_gc_external_freed(s, (size_t)old_cap * sizeof(SetSlot));
    pyrs_gc_external_freed(s,
                           (size_t)old_order_cap * sizeof(long long));
    free(old);
    free(old_order);
}

void pyrs_set_add(PyrsSet *s, long long key, int key_tag) {
    check_ref(s);
    if (s->len * 2 >= s->cap) {
        set_grow(s);
    }
    int found;
    long long idx = set_lookup(s, key, key_tag, &found);
    if (found) {
        return;
    }
    s->table[idx].key = key;
    s->table[idx].key_tag = key_tag;
    s->table[idx].state = 1;
    if (s->order_len == s->order_cap) {
        long long nc = s->order_cap * 2;
        long long *no = xmalloc((size_t)nc * sizeof(long long));
        pyrs_gc_external_allocated(s, (size_t)nc * sizeof(long long));
        memcpy(no, s->order, (size_t)s->order_len * sizeof(long long));
        pyrs_gc_external_freed(
            s, (size_t)s->order_cap * sizeof(long long));
        free(s->order);
        s->order = no;
        s->order_cap = nc;
    }
    s->order[s->order_len++] = idx;
    s->len++;
}

void pyrs_set_remove(PyrsSet *s, long long key, int key_tag) {
    check_ref(s);
    int found;
    long long idx = set_lookup(s, key, key_tag, &found);
    if (!found) {
        if (key_tag == TAG_STR) {
            const PyrsStr *str = (const PyrsStr *)(uintptr_t)key;
            char buf[256];
            snprintf(buf, sizeof buf, "KeyError: '%.*s'", (int)str->len, str->data);
            pyrs_die(buf);
        } else {
            die_keyerror_int(key);
        }
    }
    s->table[idx].state = 2;
    s->len--;
    for (long long i = 0; i < s->order_len; i++) {
        if (s->order[i] == idx) {
            memmove(&s->order[i], &s->order[i + 1],
                    (size_t)(s->order_len - i - 1) * sizeof(long long));
            s->order_len--;
            break;
        }
    }
}

void pyrs_set_discard(PyrsSet *s, long long key, int key_tag) {
    check_ref(s);
    int found;
    long long idx = set_lookup(s, key, key_tag, &found);
    if (!found) {
        return;
    }
    s->table[idx].state = 2;
    s->len--;
    for (long long i = 0; i < s->order_len; i++) {
        if (s->order[i] == idx) {
            memmove(&s->order[i], &s->order[i + 1],
                    (size_t)(s->order_len - i - 1) * sizeof(long long));
            s->order_len--;
            break;
        }
    }
}

int pyrs_set_contains(const PyrsSet *s, long long key, int key_tag) {
    check_ref(s);
    int found;
    set_lookup(s, key, key_tag, &found);
    return found;
}

void pyrs_set_clear(PyrsSet *s) {
    check_ref(s);
    memset(s->table, 0, (size_t)s->cap * sizeof(SetSlot));
    s->len = 0;
    s->order_len = 0;
}

int pyrs_set_iter_elem(const PyrsSet *s, long long i, long long *out) {
    check_ref(s);
    if (i < 0 || i >= s->order_len) {
        return 0;
    }
    SetSlot *e = &s->table[s->order[i]];
    if (e->state != 1) {
        return 0;
    }
    *out = e->key;
    return 1;
}

void pyrs_print_set(const PyrsSet *s) {
    check_ref(s);
    if (s->len == 0) {
        fputs("set()", stdout);
        return;
    }
    fputc('{', stdout);
    int first = 1;
    for (long long i = 0; i < s->order_len; i++) {
        SetSlot *e = &s->table[s->order[i]];
        if (e->state != 1) {
            continue;
        }
        if (!first) {
            fputs(", ", stdout);
        }
        first = 0;
        print_slot(e->key, e->key_tag);
    }
    fputc('}', stdout);
}

int pyrs_set_eq(const PyrsSet *a, const PyrsSet *b) {
    check_ref(a);
    check_ref(b);
    if (a->len != b->len) {
        return 0;
    }
    for (long long i = 0; i < a->order_len; i++) {
        SetSlot *e = &a->table[a->order[i]];
        if (e->state != 1) {
            continue;
        }
        int found;
        set_lookup(b, e->key, e->key_tag, &found);
        if (!found) {
            return 0;
        }
    }
    return 1;
}

int pyrs_set_issubset(const PyrsSet *a, const PyrsSet *b, int proper) {
    check_ref(a);
    check_ref(b);
    if (a->len > b->len) {
        return 0;
    }
    if (proper && a->len == b->len) {
        return 0;
    }
    for (long long i = 0; i < a->order_len; i++) {
        SetSlot *e = &a->table[a->order[i]];
        if (e->state != 1) {
            continue;
        }
        int found;
        set_lookup(b, e->key, e->key_tag, &found);
        if (!found) {
            return 0;
        }
    }
    return 1;
}

int pyrs_set_isdisjoint(const PyrsSet *a, const PyrsSet *b) {
    check_ref(a);
    check_ref(b);
    const PyrsSet *small = a->len <= b->len ? a : b;
    const PyrsSet *big = small == a ? b : a;
    for (long long i = 0; i < small->order_len; i++) {
        SetSlot *e = &small->table[small->order[i]];
        if (e->state != 1) {
            continue;
        }
        int found;
        set_lookup(big, e->key, e->key_tag, &found);
        if (found) {
            return 0;
        }
    }
    return 1;
}

PyrsList *pyrs_set_elements(const PyrsSet *s) {
    check_ref(s);
    PyrsList *r = pyrs_list_new(s->len);
    for (long long i = 0; i < s->order_len; i++) {
        SetSlot *e = &s->table[s->order[i]];
        if (e->state == 1) {
            pyrs_list_push(r, e->key);
        }
    }
    return r;
}

/* In-place union: add every element of `other` into `s`. */
void pyrs_set_update(PyrsSet *s, const PyrsSet *other) {
    check_ref(s);
    check_ref(other);
    for (long long i = 0; i < other->order_len; i++) {
        SetSlot *e = &other->table[other->order[i]];
        if (e->state == 1) {
            pyrs_set_add(s, e->key, e->key_tag);
        }
    }
}

/* New set = s | other. */
PyrsSet *pyrs_set_union(const PyrsSet *a, const PyrsSet *b) {
    check_ref(a);
    check_ref(b);
    PyrsSet *r = pyrs_set_new();
    pyrs_set_update(r, a);
    pyrs_set_update(r, b);
    return r;
}

/* New set = a & b. */
PyrsSet *pyrs_set_intersect(const PyrsSet *a, const PyrsSet *b) {
    check_ref(a);
    check_ref(b);
    PyrsSet *r = pyrs_set_new();
    for (long long i = 0; i < a->order_len; i++) {
        SetSlot *e = &a->table[a->order[i]];
        if (e->state == 1 && pyrs_set_contains(b, e->key, e->key_tag)) {
            pyrs_set_add(r, e->key, e->key_tag);
        }
    }
    return r;
}

/* New set = a - b. */
PyrsSet *pyrs_set_diff(const PyrsSet *a, const PyrsSet *b) {
    check_ref(a);
    check_ref(b);
    PyrsSet *r = pyrs_set_new();
    for (long long i = 0; i < a->order_len; i++) {
        SetSlot *e = &a->table[a->order[i]];
        if (e->state == 1 && !pyrs_set_contains(b, e->key, e->key_tag)) {
            pyrs_set_add(r, e->key, e->key_tag);
        }
    }
    return r;
}

/* New set = a ^ b. */
PyrsSet *pyrs_set_symdiff(const PyrsSet *a, const PyrsSet *b) {
    check_ref(a);
    check_ref(b);
    PyrsSet *r = pyrs_set_new();
    for (long long i = 0; i < a->order_len; i++) {
        SetSlot *e = &a->table[a->order[i]];
        if (e->state == 1 && !pyrs_set_contains(b, e->key, e->key_tag)) {
            pyrs_set_add(r, e->key, e->key_tag);
        }
    }
    for (long long i = 0; i < b->order_len; i++) {
        SetSlot *e = &b->table[b->order[i]];
        if (e->state == 1 && !pyrs_set_contains(a, e->key, e->key_tag)) {
            pyrs_set_add(r, e->key, e->key_tag);
        }
    }
    return r;
}

/* Shallow list copy (new list, same slots). */
PyrsList *pyrs_list_copy(const PyrsList *src) {
    check_ref(src);
    PyrsList *r = pyrs_list_new(src->len);
    if (src->len > 0) {
        memcpy(r->data, src->data, (size_t)src->len * sizeof(long long));
        r->len = src->len;
    }
    return r;
}

/* list(str) → list of 1-char PyrsStr. */
PyrsList *pyrs_list_from_str(const PyrsStr *s) {
    check_ref(s);
    PyrsList *r = pyrs_list_new(s->len);
    for (long long i = 0; i < s->len; i++) {
        pyrs_list_push(r, (long long)(uintptr_t)str_sub(s, i, 1));
    }
    return r;
}

/* set(list) — list slots already tagged as int/str keys. */
PyrsSet *pyrs_set_from_list(const PyrsList *xs, int key_tag) {
    check_ref(xs);
    PyrsSet *r = pyrs_set_new();
    for (long long i = 0; i < xs->len; i++) {
        pyrs_set_add(r, xs->data[i], key_tag);
    }
    return r;
}

/* set(str) → set of 1-char strings. */
PyrsSet *pyrs_set_from_str(const PyrsStr *s) {
    check_ref(s);
    PyrsSet *r = pyrs_set_new();
    for (long long i = 0; i < s->len; i++) {
        pyrs_set_add(r, (long long)(uintptr_t)str_sub(s, i, 1), TAG_STR);
    }
    return r;
}

/* Shallow dict copy. */
PyrsDict *pyrs_dict_copy(const PyrsDict *d) {
    check_ref(d);
    PyrsDict *r = pyrs_dict_new();
    pyrs_dict_update(r, d);
    return r;
}

/* dict(list of 2-tuples) — each element is a PyrsTuple* of length 2. */
PyrsDict *pyrs_dict_from_pairs(const PyrsList *pairs, int key_tag, int val_tag) {
    check_ref(pairs);
    PyrsDict *d = pyrs_dict_new();
    for (long long i = 0; i < pairs->len; i++) {
        PyrsTuple *t = (PyrsTuple *)(uintptr_t)pairs->data[i];
        check_ref(t);
        if (t->len != 2) {
            pyrs_die("ValueError: dictionary update sequence element has length other than 2");
        }
        pyrs_dict_set(d, t->data[0], key_tag, t->data[1], val_tag);
    }
    return d;
}

/* ---- os ---- */


PyrsStr *pyrs_os_getcwd(void) {
    char buf[PATH_MAX];
    if (getcwd(buf, sizeof(buf)) == NULL) {
        pyrs_die("OSError: getcwd failed");
    }
    return str_from_cstr(buf);
}

/* ---- json (subset) ---- */

static void json_skip_ws(const char **p) {
    while (**p == ' ' || **p == '\t' || **p == '\n' || **p == '\r') {
        (*p)++;
    }
}

static int json_match(const char **p, const char *lit) {
    size_t n = strlen(lit);
    if (strncmp(*p, lit, n) != 0) {
        return 0;
    }
    *p += n;
    return 1;
}

static void json_expect_end(const char *p) {
    json_skip_ws(&p);
    if (*p != '\0') {
        pyrs_die("ValueError: Extra data");
    }
}

static PyrsStr *json_parse_string(const char **p) {
    if (**p != '"') {
        pyrs_die("ValueError: Expecting value");
    }
    (*p)++;
    /* first pass: compute length with escapes */
    const char *s = *p;
    long long n = 0;
    while (*s && *s != '"') {
        if (*s == '\\') {
            s++;
            if (!*s) {
                pyrs_die("ValueError: Unterminated string");
            }
            s++;
            n++;
        } else {
            s++;
            n++;
        }
    }
    if (*s != '"') {
        pyrs_die("ValueError: Unterminated string");
    }
    PyrsStr *r = str_alloc(n);
    char *out = r->data;
    while (**p && **p != '"') {
        if (**p == '\\') {
            (*p)++;
            char c = **p;
            if (!c) {
                pyrs_die("ValueError: Unterminated string");
            }
            switch (c) {
            case '"':
            case '\\':
            case '/':
                *out++ = c;
                break;
            case 'b':
                *out++ = '\b';
                break;
            case 'f':
                *out++ = '\f';
                break;
            case 'n':
                *out++ = '\n';
                break;
            case 'r':
                *out++ = '\r';
                break;
            case 't':
                *out++ = '\t';
                break;
            case 'u':
                /* minimal: only \u00XX latin-1 */
                if (!(*p)[1] || !(*p)[2] || !(*p)[3] || !(*p)[4]) {
                    pyrs_die("ValueError: Invalid \\u escape");
                }
                {
                    unsigned v = 0;
                    for (int i = 1; i <= 4; i++) {
                        char h = (*p)[i];
                        v <<= 4;
                        if (h >= '0' && h <= '9')
                            v |= (unsigned)(h - '0');
                        else if (h >= 'a' && h <= 'f')
                            v |= (unsigned)(h - 'a' + 10);
                        else if (h >= 'A' && h <= 'F')
                            v |= (unsigned)(h - 'A' + 10);
                        else
                            pyrs_die("ValueError: Invalid \\u escape");
                    }
                    if (v > 0xff) {
                        pyrs_die("ValueError: \\u escape out of range (ASCII/latin-1 only)");
                    }
                    *out++ = (char)v;
                    *p += 4;
                }
                break;
            default:
                pyrs_die("ValueError: Invalid escape");
            }
            (*p)++;
        } else {
            *out++ = **p;
            (*p)++;
        }
    }
    (*p)++; /* closing quote */
    return r;
}

static long long json_parse_int(const char **p) {
    const char *start = *p;
    if (*start == '+' || *start == '-') {
        start++;
    }
    if (*start < '0' || *start > '9') {
        pyrs_die("ValueError: Expecting value");
    }
    const char *end = *p;
    if (*end == '+' || *end == '-') {
        end++;
    }
    while (*end >= '0' && *end <= '9') {
        end++;
    }
    if (end == *p || (*p[0] == '+' || *p[0] == '-') && end == *p + 1) {
        pyrs_die("ValueError: Expecting value");
    }
    long long len = (long long)(end - *p);
    long long v = pyrs_int_from_str(*p, len);
    *p = end;
    return v;
}

static double json_parse_float(const char **p) {
    char *end = NULL;
    errno = 0;
    double v = strtod(*p, &end);
    if (end == *p || errno == ERANGE) {
        pyrs_die("ValueError: Expecting value");
    }
    *p = end;
    return v;
}

static int json_parse_bool(const char **p) {
    if (json_match(p, "true")) {
        return 1;
    }
    if (json_match(p, "false")) {
        return 0;
    }
    pyrs_die("ValueError: Expecting value");
    return 0;
}

long long pyrs_json_loads_int(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    long long v = json_parse_int(&p);
    json_expect_end(p);
    return v;
}

double pyrs_json_loads_float(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    double v = json_parse_float(&p);
    json_expect_end(p);
    return v;
}

int pyrs_json_loads_bool(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    int v = json_parse_bool(&p);
    json_expect_end(p);
    return v;
}

PyrsStr *pyrs_json_loads_str(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    PyrsStr *v = json_parse_string(&p);
    json_expect_end(p);
    return v;
}

static PyrsList *json_parse_list_of(const char **p, int elem_tag) {
    if (**p != '[') {
        pyrs_die("ValueError: Expecting value");
    }
    (*p)++;
    json_skip_ws(p);
    PyrsList *list = pyrs_list_new(4);
    if (**p == ']') {
        (*p)++;
        return list;
    }
    for (;;) {
        json_skip_ws(p);
        long long slot;
        if (elem_tag == TAG_INT) {
            slot = json_parse_int(p);
        } else if (elem_tag == TAG_FLOAT) {
            double d = json_parse_float(p);
            memcpy(&slot, &d, sizeof(double));
        } else if (elem_tag == TAG_BOOL) {
            slot = json_parse_bool(p) ? 1 : 0;
        } else if (elem_tag == TAG_STR) {
            slot = (long long)(uintptr_t)json_parse_string(p);
        } else {
            pyrs_die("ValueError: unsupported list element");
            slot = 0;
        }
        pyrs_list_push(list, slot);
        json_skip_ws(p);
        if (**p == ',') {
            (*p)++;
            continue;
        }
        if (**p == ']') {
            (*p)++;
            break;
        }
        pyrs_die("ValueError: Expecting ',' delimiter");
    }
    return list;
}

PyrsList *pyrs_json_loads_list_int(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    PyrsList *v = json_parse_list_of(&p, TAG_INT);
    json_expect_end(p);
    return v;
}

PyrsList *pyrs_json_loads_list_float(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    PyrsList *v = json_parse_list_of(&p, TAG_FLOAT);
    json_expect_end(p);
    return v;
}

PyrsList *pyrs_json_loads_list_str(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    PyrsList *v = json_parse_list_of(&p, TAG_STR);
    json_expect_end(p);
    return v;
}

PyrsList *pyrs_json_loads_list_bool(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    PyrsList *v = json_parse_list_of(&p, TAG_BOOL);
    json_expect_end(p);
    return v;
}

static PyrsDict *json_parse_dict_str_val(const char **p, int val_tag) {
    if (**p != '{') {
        pyrs_die("ValueError: Expecting value");
    }
    (*p)++;
    json_skip_ws(p);
    PyrsDict *d = pyrs_dict_new();
    if (**p == '}') {
        (*p)++;
        return d;
    }
    for (;;) {
        json_skip_ws(p);
        PyrsStr *key = json_parse_string(p);
        json_skip_ws(p);
        if (**p != ':') {
            pyrs_die("ValueError: Expecting ':' delimiter");
        }
        (*p)++;
        json_skip_ws(p);
        long long val;
        if (val_tag == TAG_INT) {
            val = json_parse_int(p);
        } else if (val_tag == TAG_FLOAT) {
            double dv = json_parse_float(p);
            memcpy(&val, &dv, sizeof(double));
        } else if (val_tag == TAG_BOOL) {
            val = json_parse_bool(p) ? 1 : 0;
        } else if (val_tag == TAG_STR) {
            val = (long long)(uintptr_t)json_parse_string(p);
        } else {
            pyrs_die("ValueError: unsupported dict value");
            val = 0;
        }
        pyrs_dict_set(d, (long long)(uintptr_t)key, TAG_STR, val, val_tag);
        json_skip_ws(p);
        if (**p == ',') {
            (*p)++;
            continue;
        }
        if (**p == '}') {
            (*p)++;
            break;
        }
        pyrs_die("ValueError: Expecting ',' delimiter");
    }
    return d;
}

PyrsDict *pyrs_json_loads_dict_str_int(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    PyrsDict *v = json_parse_dict_str_val(&p, TAG_INT);
    json_expect_end(p);
    return v;
}

PyrsDict *pyrs_json_loads_dict_str_float(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    PyrsDict *v = json_parse_dict_str_val(&p, TAG_FLOAT);
    json_expect_end(p);
    return v;
}

PyrsDict *pyrs_json_loads_dict_str_str(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    PyrsDict *v = json_parse_dict_str_val(&p, TAG_STR);
    json_expect_end(p);
    return v;
}

PyrsDict *pyrs_json_loads_dict_str_bool(const PyrsStr *s) {
    check_ref(s);
    const char *p = s->data;
    json_skip_ws(&p);
    PyrsDict *v = json_parse_dict_str_val(&p, TAG_BOOL);
    json_expect_end(p);
    return v;
}

/* growable byte buffer for dumps */
typedef struct {
    char *data;
    size_t len;
    size_t cap;
} JsonBuf;

static void jbuf_init(JsonBuf *b) {
    b->cap = 64;
    b->len = 0;
    b->data = (char *)xmalloc(b->cap);
    b->data[0] = '\0';
}

static void jbuf_dispose(void *context) {
    JsonBuf *b = (JsonBuf *)context;
    free(b->data);
    b->data = NULL;
    b->len = 0;
    b->cap = 0;
}

static void jbuf_ensure(JsonBuf *b, size_t extra) {
    if (b->len + extra + 1 > b->cap) {
        size_t nc = b->cap * 2;
        while (nc < b->len + extra + 1) {
            nc *= 2;
        }
        char *nd = (char *)xmalloc(nc);
        memcpy(nd, b->data, b->len + 1);
        free(b->data);
        b->data = nd;
        b->cap = nc;
    }
}

static void jbuf_putc(JsonBuf *b, char c) {
    jbuf_ensure(b, 1);
    b->data[b->len++] = c;
    b->data[b->len] = '\0';
}

static void jbuf_puts(JsonBuf *b, const char *s) {
    size_t n = strlen(s);
    jbuf_ensure(b, n);
    memcpy(b->data + b->len, s, n);
    b->len += n;
    b->data[b->len] = '\0';
}

static void jbuf_put_str_escaped(JsonBuf *b, const PyrsStr *s) {
    jbuf_putc(b, '"');
    for (long long i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        switch (c) {
        case '"':
            jbuf_puts(b, "\\\"");
            break;
        case '\\':
            jbuf_puts(b, "\\\\");
            break;
        case '\b':
            jbuf_puts(b, "\\b");
            break;
        case '\f':
            jbuf_puts(b, "\\f");
            break;
        case '\n':
            jbuf_puts(b, "\\n");
            break;
        case '\r':
            jbuf_puts(b, "\\r");
            break;
        case '\t':
            jbuf_puts(b, "\\t");
            break;
        default:
            if (c < 0x20) {
                char tmp[8];
                snprintf(tmp, sizeof(tmp), "\\u%04x", c);
                jbuf_puts(b, tmp);
            } else {
                jbuf_putc(b, (char)c);
            }
        }
    }
    jbuf_putc(b, '"');
}

static void jbuf_put_int(JsonBuf *b, long long v) {
    long long n;
    char *s = int_to_dec(v, &n);
    jbuf_puts(b, s);
    free(s);
}

static void jbuf_put_float(JsonBuf *b, double v) {
    /* Match CPython json: use repr-like shortest that round-trips; whole floats keep .0 */
    char tmp[64];
    if (isnan(v) || isinf(v)) {
        pyrs_die("ValueError: Out of range float values are not JSON compliant");
    }
    /* Use the same idea as print: enough digits, then strip trailing zeros carefully */
    snprintf(tmp, sizeof(tmp), "%.17g", v);
    /* Ensure a decimal point or exponent for whole numbers (json allows "1" for 1.0) */
    jbuf_puts(b, tmp);
}

static void json_dumps_into(JsonBuf *b, long long slot, int tag) {
    if (tag == TAG_INT) {
        jbuf_put_int(b, slot);
    } else if (tag == TAG_FLOAT) {
        double d;
        memcpy(&d, &slot, sizeof(double));
        jbuf_put_float(b, d);
    } else if (tag == TAG_BOOL) {
        jbuf_puts(b, slot ? "true" : "false");
    } else if (tag == TAG_STR) {
        jbuf_put_str_escaped(b, (const PyrsStr *)(uintptr_t)slot);
    } else if (tag == TAG_DICT) {
        const PyrsDict *d = (const PyrsDict *)(uintptr_t)slot;
        check_ref(d);
        jbuf_putc(b, '{');
        int first = 1;
        PyrsList *items = pyrs_dict_items(d);
        for (long long i = 0; i < items->len; i++) {
            PyrsTuple *t = (PyrsTuple *)(uintptr_t)items->data[i];
            if (!first) {
                jbuf_puts(b, ", ");
            }
            first = 0;
            long long kslot = t->data[0];
            long long vslot = t->data[1];
            int vtag = t->tags[1];
            jbuf_put_str_escaped(b, (const PyrsStr *)(uintptr_t)kslot);
            jbuf_puts(b, ": ");
            json_dumps_into(b, vslot, vtag);
        }
        jbuf_putc(b, '}');
    } else if (tag >= 4 && ((tag - 4) % 8) == 0) {
        /* list: tag = 4 + 8 * elem_tag */
        const PyrsList *l = (const PyrsList *)(uintptr_t)slot;
        check_ref(l);
        int elem_tag = (tag - 4) / 8;
        jbuf_putc(b, '[');
        for (long long i = 0; i < l->len; i++) {
            if (i > 0) {
                jbuf_puts(b, ", ");
            }
            json_dumps_into(b, l->data[i], elem_tag);
        }
        jbuf_putc(b, ']');
    } else {
        pyrs_die("TypeError: Object of this type is not JSON serializable");
    }
}

PyrsStr *pyrs_json_dumps(long long slot, int tag) {
    JsonBuf b;
    PyrsCleanup cleanup;
    jbuf_init(&b);
    pyrs_cleanup_push(&cleanup, jbuf_dispose, &b);
    json_dumps_into(&b, slot, tag);
    PyrsStr *r = str_from_cstr(b.data);
    pyrs_cleanup_pop(&cleanup);
    jbuf_dispose(&b);
    return r;
}

/* ---- cells (nonlocal / mutable free vars) ---- */
typedef struct {
    long long slot;
    int bound; /* 0 = unbound (NameError on load); 1 = assigned */
} PyrsCell;

PyrsCell *pyrs_cell_new(long long slot) {
    PyrsCell *c = (PyrsCell *)pyrs_gc_alloc(sizeof(PyrsCell), PYRS_GC_CELL);
    c->slot = slot;
    c->bound = 1;
    return c;
}

/* Unbound cell for late free-var capture (CPython empty cell until assign). */
PyrsCell *pyrs_cell_new_unbound(void) {
    PyrsCell *c = (PyrsCell *)pyrs_gc_alloc(sizeof(PyrsCell), PYRS_GC_CELL);
    c->slot = 0;
    c->bound = 0;
    return c;
}

long long pyrs_cell_load(PyrsCell *c) {
    check_ref(c);
    if (!c->bound) {
        /* Free-var cells: CPython NameError (not UnboundLocalError). */
        pyrs_die("NameError: cannot access free variable where it is not "
                 "associated with a value in enclosing scope");
    }
    return c->slot;
}

void pyrs_cell_store(PyrsCell *c, long long slot) {
    check_ref(c);
    c->slot = slot;
    c->bound = 1;
}

/* ---- closures ---- */
typedef struct {
    void *code;
    long long ncap;
    long long caps[];
} PyrsClosure;

PyrsClosure *pyrs_closure_new(void *code, long long ncap) {
    size_t sz = sizeof(PyrsClosure) + (size_t)ncap * sizeof(long long);
    PyrsClosure *c = (PyrsClosure *)pyrs_gc_alloc(sz, PYRS_GC_CLOSURE);
    c->code = code;
    c->ncap = ncap;
    for (long long i = 0; i < ncap; i++) {
        c->caps[i] = 0;
    }
    return c;
}

void pyrs_closure_set(PyrsClosure *c, long long i, long long slot) {
    check_ref(c);
    if (i < 0 || i >= c->ncap) {
        pyrs_die("RuntimeError: closure capture index out of range");
    }
    c->caps[i] = slot;
}

void *pyrs_closure_code(PyrsClosure *c) {
    check_ref(c);
    return c->code;
}

long long pyrs_closure_get(PyrsClosure *c, long long i) {
    check_ref(c);
    if (i < 0 || i >= c->ncap) {
        pyrs_die("RuntimeError: closure capture index out of range");
    }
    return c->caps[i];
}

/* ---- generators ---- */
#define PYRS_GEN_MAX_TRY 16
typedef struct {
    void *code;          /* resume function: i32 (PyrsGen*) */
    long long state;     /* program counter */
    long long done;      /* non-zero when exhausted */
    long long yield_slot;/* last yielded value as slot */
    long long return_slot; /* StopIteration.value / `return expr` payload */
    long long return_set;  /* 1 if return_slot is a real return value (not bare end) */
    long long closing;   /* non-zero while close() injects GeneratorExit */
    long long send_slot; /* value delivered to suspended yield expression */
    long long send_is_none; /* 1 when send was None / next() */
    long long throw_type; /* non-zero: inject this PYRS_EXC_* at resume */
    void *throw_msg;     /* pyrs str* message for throw (may be NULL) */
    long long try_phases[PYRS_GEN_MAX_TRY]; /* phase per try pool slot across yield */
    long long try_exits[PYRS_GEN_MAX_TRY];  /* TRY_EXIT_* per pool slot (yield in finally) */
    long long nlocals;
    long long locals[];  /* frame */
} PyrsGen;

PyrsGen *pyrs_gen_new(void *code, long long nlocals) {
    size_t sz = sizeof(PyrsGen) + (size_t)nlocals * sizeof(long long);
    PyrsGen *g = (PyrsGen *)pyrs_gc_alloc(sz, PYRS_GC_GENERATOR);
    g->code = code;
    g->state = 0;
    g->done = 0;
    g->yield_slot = 0;
    g->return_slot = 0;
    g->return_set = 0;
    g->closing = 0;
    g->send_slot = 0;
    g->send_is_none = 1;
    g->throw_type = 0;
    g->throw_msg = NULL;
    for (int i = 0; i < PYRS_GEN_MAX_TRY; i++) {
        g->try_phases[i] = 0;
        g->try_exits[i] = 0;
    }
    g->nlocals = nlocals;
    for (long long i = 0; i < nlocals; i++) {
        g->locals[i] = 0;
    }
    return g;
}

void pyrs_gen_save_try_phase(PyrsGen *g, long long i, long long phase) {
    check_ref(g);
    if (i >= 0 && i < PYRS_GEN_MAX_TRY) {
        g->try_phases[i] = phase;
    }
}

long long pyrs_gen_load_try_phase(PyrsGen *g, long long i) {
    check_ref(g);
    if (i >= 0 && i < PYRS_GEN_MAX_TRY) {
        return g->try_phases[i];
    }
    return 0;
}

void pyrs_gen_save_try_exit(PyrsGen *g, long long i, long long exit_kind) {
    check_ref(g);
    if (i >= 0 && i < PYRS_GEN_MAX_TRY) {
        g->try_exits[i] = exit_kind;
    }
}

long long pyrs_gen_load_try_exit(PyrsGen *g, long long i) {
    check_ref(g);
    if (i >= 0 && i < PYRS_GEN_MAX_TRY) {
        return g->try_exits[i];
    }
    return 0;
}

int pyrs_gen_closing(PyrsGen *g) {
    check_ref(g);
    return g->closing ? 1 : 0;
}

/* Prepare send value for the next resume. `is_none` non-zero → yield expr is None. */
void pyrs_gen_set_send(PyrsGen *g, long long slot, long long is_none) {
    check_ref(g);
    /* Exhausted generators do not accept send; caller should short-circuit. */
    if (g->done) {
        return;
    }
    if (g->state == 0 && !is_none) {
        pyrs_die("TypeError: can't send non-None value to a just-started generator");
    }
    g->send_slot = slot;
    g->send_is_none = is_none ? 1 : 0;
}

long long pyrs_gen_send_slot(PyrsGen *g) {
    check_ref(g);
    return g->send_slot;
}

int pyrs_gen_send_is_none(PyrsGen *g) {
    check_ref(g);
    return g->send_is_none ? 1 : 0;
}

/* Arm throw injection for the next resume (type is PYRS_EXC_*; msg is pyrs str*).
 * Not-yet-started or already-finished generators raise immediately at the
 * throw() call site (CPython does not run the body). Mark done so later
 * send/next do not re-enter the body. */
void pyrs_gen_set_throw(PyrsGen *g, long long type, void *msg) {
    check_ref(g);
    if (g->done || g->state == 0) {
        g->done = 1;
        g->throw_type = 0;
        g->throw_msg = NULL;
        int t = (int)type;
        const char *m = NULL;
        if (msg != NULL) {
            /* pyrs str: i64 len then bytes */
            m = (const char *)msg + 8;
        }
        pyrs_raise(t, m);
        return;
    }
    g->throw_type = type;
    g->throw_msg = msg;
}

int pyrs_gen_throwing(PyrsGen *g) {
    check_ref(g);
    return g->throw_type != 0 ? 1 : 0;
}

long long pyrs_gen_throw_type(PyrsGen *g) {
    check_ref(g);
    return g->throw_type;
}

void *pyrs_gen_throw_msg(PyrsGen *g) {
    check_ref(g);
    return g->throw_msg;
}

void pyrs_gen_clear_throw(PyrsGen *g) {
    check_ref(g);
    g->throw_type = 0;
    g->throw_msg = NULL;
}

/* Inject GeneratorExit and resume until the generator finishes. CPython
 * close() swallows an uncaught GeneratorExit after finally runs. Yielding
 * again after swallowing GeneratorExit is RuntimeError.
 *
 * Nested close (yield-from finally while outer GE is pending) must not
 * clear the outer exception. */
void pyrs_gen_close(PyrsGen *g) {
    check_ref(g);
    if (g->done) {
        return;
    }
    int saved_type = g_exc_type;
    char saved_msg[sizeof g_exc_msg];
    memcpy(saved_msg, g_exc_msg, sizeof g_exc_msg);

    g->closing = 1;
    typedef int (*ResumeFn)(void *);
    ResumeFn resume = (ResumeFn)g->code;
    for (int i = 0; i < 10000 && !g->done; i++) {
        int r = resume(g);
        if (r != 0) {
            break;
        }
        /* Yielded again while closing — CPython:
         * RuntimeError: generator ignored GeneratorExit */
        g->closing = 0;
        g->done = 1;
        /* Restore any outer exception before dying. */
        if (saved_type != 0) {
            g_exc_type = saved_type;
            memcpy(g_exc_msg, saved_msg, sizeof g_exc_msg);
        }
        pyrs_die("RuntimeError: generator ignored GeneratorExit");
    }
    g->done = 1;
    g->closing = 0;
    if (saved_type != 0) {
        /* Preserve outer pending exception (e.g. outer GeneratorExit). */
        g_exc_type = saved_type;
        memcpy(g_exc_msg, saved_msg, sizeof g_exc_msg);
    } else if (g_exc_type == PYRS_EXC_GENEXIT) {
        /* Swallow GeneratorExit produced by this close only. */
        g_exc_type = 0;
        g_exc_msg[0] = '\0';
    }
}

void pyrs_gen_set_return(PyrsGen *g, long long slot) {
    check_ref(g);
    g->return_slot = slot;
    g->return_set = 1;
}

long long pyrs_gen_return_value(PyrsGen *g) {
    check_ref(g);
    return g->return_slot;
}

/* 1 if generator executed `return <expr>` (StopIteration.value set).
 * Bare `return` / fall-off leave this 0 so yield-from yields None. */
int pyrs_gen_has_return(PyrsGen *g) {
    check_ref(g);
    return g->return_set ? 1 : 0;
}

long long pyrs_gen_get_local(PyrsGen *g, long long i) {
    check_ref(g);
    if (i < 0 || i >= g->nlocals) {
        pyrs_die("RuntimeError: generator local index out of range");
    }
    return g->locals[i];
}

void pyrs_gen_set_local(PyrsGen *g, long long i, long long slot) {
    check_ref(g);
    if (i < 0 || i >= g->nlocals) {
        pyrs_die("RuntimeError: generator local index out of range");
    }
    g->locals[i] = slot;
}

long long pyrs_gen_state(PyrsGen *g) {
    check_ref(g);
    return g->state;
}

void pyrs_gen_set_state(PyrsGen *g, long long state) {
    check_ref(g);
    g->state = state;
}

void pyrs_gen_set_yield(PyrsGen *g, long long slot) {
    check_ref(g);
    g->yield_slot = slot;
}

long long pyrs_gen_yield_value(PyrsGen *g) {
    check_ref(g);
    return g->yield_slot;
}

int pyrs_gen_done(PyrsGen *g) {
    check_ref(g);
    return g->done ? 1 : 0;
}

void pyrs_gen_set_done(PyrsGen *g) {
    check_ref(g);
    g->done = 1;
}

int pyrs_gen_is_genexit(void) {
    return g_exc_type == PYRS_EXC_GENEXIT ? 1 : 0;
}
/* ---- user class objects (closed-world layouts) ---- */

/* Allocate nbytes (including i64 type_id header) and write type_id at offset 0.
 * Remaining bytes are zeroed so fields start as 0/null. */
void *pyrs_object_new(long long type_id, long long nbytes) {
    if (nbytes < (long long)sizeof(long long)) {
        nbytes = (long long)sizeof(long long);
    }
    void *p = pyrs_gc_alloc((size_t)nbytes, PYRS_GC_CLASS);
    memset(p, 0, (size_t)nbytes);
    *(long long *)p = type_id;
    return p;
}

/* isinstance(obj, Class): walk parent chain. parents[i] is parent of class i,
 * or -1 for no parent. n is table length. */
int pyrs_isinstance_class(void *obj, long long target, long long *parents, long long n) {
    if (obj == NULL || parents == NULL || n <= 0) {
        return 0;
    }
    long long tid = *(long long *)obj;
    for (int depth = 0; depth < 64; depth++) {
        if (tid == target) {
            return 1;
        }
        if (tid < 0 || tid >= n) {
            return 0;
        }
        tid = parents[tid];
        if (tid < 0) {
            return 0;
        }
    }
    return 0;
}

/* Optional: print via prebuilt str (codegen usually uses interned "<Name object>"). */
void pyrs_print_object(void *obj) {
    pyrs_print_class_instance(obj);
}

/* Build a PyrsStr `"<Name object>"` from runtime type_id (for str(obj)). */
PyrsStr *pyrs_str_from_object(void *obj) {
    char buf[256];
    if (obj != NULL && g_class_names != NULL) {
        long long tid = *(long long *)obj;
        if (tid >= 0 && tid < g_class_n && g_class_names[tid] != NULL) {
            snprintf(buf, sizeof buf, "<%s object>", g_class_names[tid]);
        } else {
            snprintf(buf, sizeof buf, "<object>");
        }
    } else {
        snprintf(buf, sizeof buf, "<object>");
    }
    size_t n = strlen(buf);
    PyrsStr *s = pyrs_gc_alloc(sizeof(long long) + n + 1, PYRS_GC_STRING);
    s->len = (long long)n;
    memcpy(s->data, buf, n + 1);
    return s;
}

/* ---- collector object-model hooks ----
 *
 * These are deliberately kept beside the concrete runtime layouts instead of
 * duplicating those layouts in gc.c. Heap traversal is layout-directed;
 * erased i64 payload slots are still validated conservatively because not
 * every object layout retains source-level type metadata.
 */

typedef struct {
    void *receiver;
} PyrsBoundMethodBox;

void *pyrs_bound_method_new(void *receiver) {
    PyrsBoundMethodBox *box =
        pyrs_gc_alloc(sizeof(PyrsBoundMethodBox), PYRS_GC_BOUND_METHOD);
    box->receiver = receiver;
    return box;
}

static void gc_visit_slot(long long slot, PyrsGcVisitFn visit,
                          void *context) {
    visit((uintptr_t)(unsigned long long)slot, context);
}

void pyrs_gc_trace_object(int kind, void *object, size_t size,
                          PyrsGcVisitFn visit, void *context) {
    if (object == NULL || visit == NULL) {
        return;
    }
    switch (kind) {
    case PYRS_GC_STRING:
    case PYRS_GC_BIGINT:
        return;
    case PYRS_GC_EXCEPTION: {
        PyrsExc *e = object;
        visit((uintptr_t)e->msg, context);
        return;
    }
    case PYRS_GC_LIST: {
        PyrsList *list = object;
        long long n = list->len;
        if (n < 0 || n > list->cap || list->data == NULL) {
            return;
        }
        for (long long i = 0; i < n; i++) {
            gc_visit_slot(list->data[i], visit, context);
        }
        return;
    }
    case PYRS_GC_FILE: {
        PyrsFile *file = object;
        visit((uintptr_t)file->name, context);
        return;
    }
    case PYRS_GC_TUPLE: {
        PyrsTuple *tuple = object;
        if (tuple->len < 0 || tuple->data == NULL) {
            return;
        }
        for (long long i = 0; i < tuple->len; i++) {
            gc_visit_slot(tuple->data[i], visit, context);
        }
        return;
    }
    case PYRS_GC_DICT: {
        PyrsDict *dict = object;
        if (dict->cap < 0 || dict->table == NULL) {
            return;
        }
        for (long long i = 0; i < dict->cap; i++) {
            DictSlot *slot = &dict->table[i];
            if (slot->state == 1) {
                gc_visit_slot(slot->key, visit, context);
                gc_visit_slot(slot->val, visit, context);
            }
        }
        return;
    }
    case PYRS_GC_SET: {
        PyrsSet *set = object;
        if (set->cap < 0 || set->table == NULL) {
            return;
        }
        for (long long i = 0; i < set->cap; i++) {
            SetSlot *slot = &set->table[i];
            if (slot->state == 1) {
                gc_visit_slot(slot->key, visit, context);
            }
        }
        return;
    }
    case PYRS_GC_CELL: {
        PyrsCell *cell = object;
        if (cell->bound) {
            gc_visit_slot(cell->slot, visit, context);
        }
        return;
    }
    case PYRS_GC_CLOSURE: {
        PyrsClosure *closure = object;
        if (closure->ncap < 0) {
            return;
        }
        for (long long i = 0; i < closure->ncap; i++) {
            gc_visit_slot(closure->caps[i], visit, context);
        }
        return;
    }
    case PYRS_GC_GENERATOR: {
        PyrsGen *generator = object;
        gc_visit_slot(generator->yield_slot, visit, context);
        gc_visit_slot(generator->return_slot, visit, context);
        gc_visit_slot(generator->send_slot, visit, context);
        visit((uintptr_t)generator->throw_msg, context);
        if (generator->nlocals < 0) {
            return;
        }
        for (long long i = 0; i < generator->nlocals; i++) {
            gc_visit_slot(generator->locals[i], visit, context);
        }
        return;
    }
    case PYRS_GC_CLASS: {
        /* Closed-world class fields are naturally aligned within the LLVM
         * struct.  Scan their words conservatively; the leading type_id is
         * scalar, and candidate validation rejects non-heap bit patterns. */
        unsigned char *bytes = object;
        for (size_t offset = sizeof(long long);
             offset + sizeof(uintptr_t) <= size;
             offset += sizeof(uintptr_t)) {
            uintptr_t candidate = 0;
            memcpy(&candidate, bytes + offset, sizeof(candidate));
            visit(candidate, context);
        }
        return;
    }
    case PYRS_GC_UNION_BOX: {
        PyrsUnionBox *box = object;
        gc_visit_slot(box->payload, visit, context);
        return;
    }
    case PYRS_GC_BOUND_METHOD: {
        PyrsBoundMethodBox *box = object;
        visit((uintptr_t)box->receiver, context);
        return;
    }
    default:
        return;
    }
}

static void gc_visit_array_range(void *start, long long count,
                                 size_t element_size, PyrsGcRangeFn visit,
                                 void *context) {
    if (start == NULL || count <= 0 ||
        (unsigned long long)count > SIZE_MAX / element_size) {
        return;
    }
    visit(start, (size_t)count * element_size, context);
}

void pyrs_gc_visit_owned_ranges(int kind, void *object, size_t size,
                                PyrsGcRangeFn visit, void *context) {
    (void)size;
    if (object == NULL || visit == NULL) {
        return;
    }
    switch (kind) {
    case PYRS_GC_BIGINT: {
        PyrsInt *integer = object;
        gc_visit_array_range(integer->limbs, integer->nlimbs,
                             sizeof(*integer->limbs), visit, context);
        return;
    }
    case PYRS_GC_LIST: {
        PyrsList *list = object;
        gc_visit_array_range(list->data, list->cap, sizeof(*list->data),
                             visit, context);
        return;
    }
    case PYRS_GC_TUPLE: {
        PyrsTuple *tuple = object;
        gc_visit_array_range(tuple->data, tuple->len, sizeof(*tuple->data),
                             visit, context);
        gc_visit_array_range(tuple->tags, tuple->len, sizeof(*tuple->tags),
                             visit, context);
        return;
    }
    case PYRS_GC_DICT: {
        PyrsDict *dict = object;
        gc_visit_array_range(dict->table, dict->cap, sizeof(*dict->table),
                             visit, context);
        gc_visit_array_range(dict->order, dict->order_cap,
                             sizeof(*dict->order), visit, context);
        return;
    }
    case PYRS_GC_SET: {
        PyrsSet *set = object;
        gc_visit_array_range(set->table, set->cap, sizeof(*set->table), visit,
                             context);
        gc_visit_array_range(set->order, set->order_cap,
                             sizeof(*set->order), visit, context);
        return;
    }
    default:
        return;
    }
}

void pyrs_gc_destroy_object(int kind, void *object, size_t size) {
    (void)size;
    if (object == NULL) {
        return;
    }
    switch (kind) {
    case PYRS_GC_BIGINT:
        free(((PyrsInt *)object)->limbs);
        return;
    case PYRS_GC_LIST:
        free(((PyrsList *)object)->data);
        return;
    case PYRS_GC_FILE: {
        PyrsFile *file = object;
        if (!file->closed && file->fp != NULL) {
            fclose(file->fp);
            file->closed = 1;
        }
        return;
    }
    case PYRS_GC_TUPLE:
        free(((PyrsTuple *)object)->data);
        free(((PyrsTuple *)object)->tags);
        return;
    case PYRS_GC_DICT:
        free(((PyrsDict *)object)->table);
        free(((PyrsDict *)object)->order);
        return;
    case PYRS_GC_SET:
        free(((PyrsSet *)object)->table);
        free(((PyrsSet *)object)->order);
        return;
    default:
        return;
    }
}
