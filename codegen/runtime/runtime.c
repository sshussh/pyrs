/* PyRs runtime: tiny C support library linked into every compiled program.
 *
 * Printing matches CPython:
 * - floats use the shortest representation that round-trips, and whole
 *   floats keep their ".0" (1.0 prints as "1.0", not "1")
 * - bools print True/False; lists/tuples/dicts/sets print like CPython
 * - runtime errors (ZeroDivisionError, IndexError, ...) print to stderr
 *   and exit(1), unless a try-frame is active (then longjmp to handler)
 *
 * Heap objects (str/list/tuple/dict/set) are never freed — fine for
 * short-lived compiled programs, documented as a known limitation.
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

/* exception type tags — keep in sync with ir::ExcType; OTHER is catchable
 * only by bare `except:` (not by `except RuntimeError`). */
#define PYRS_EXC_VALUE 1
#define PYRS_EXC_KEY 2
#define PYRS_EXC_INDEX 3
#define PYRS_EXC_ZERODIV 4
#define PYRS_EXC_TYPE 5
#define PYRS_EXC_RUNTIME 6
#define PYRS_EXC_GENEXIT 7
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
/* list tags: 4 + 8 * elem_tag */

/* layout shared with codegen: leading i64 length, then bytes (+ NUL) */
typedef struct {
    long long len;
    char data[];
} PyrsStr;

/* stable header; data grows by reallocation */
typedef struct {
    long long len;
    long long cap;
    long long *data;
} PyrsList;

/* used by the str splitters before the list section defines them */
PyrsList *pyrs_list_new(long long cap);
void pyrs_list_push(PyrsList *l, long long slot);

/* ---- exceptions (setjmp/longjmp try frames) ----
 * Single-threaded process-global state (PyRs programs are not multi-threaded). */

typedef struct PyrsExcFrame {
    jmp_buf buf;
    struct PyrsExcFrame *prev;
} PyrsExcFrame;

static PyrsExcFrame *g_exc_frames = NULL;
static int g_exc_type = 0;
static char g_exc_msg[512];

static void *xmalloc(size_t n);

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
    default:
        return "Exception";
    }
}

static int classify_exc_msg(const char *msg) {
    if (strncmp(msg, "ValueError", 10) == 0) {
        return PYRS_EXC_VALUE;
    }
    if (strncmp(msg, "KeyError", 8) == 0) {
        return PYRS_EXC_KEY;
    }
    if (strncmp(msg, "IndexError", 10) == 0) {
        return PYRS_EXC_INDEX;
    }
    if (strncmp(msg, "ZeroDivisionError", 17) == 0) {
        return PYRS_EXC_ZERODIV;
    }
    if (strncmp(msg, "TypeError", 9) == 0) {
        return PYRS_EXC_TYPE;
    }
    if (strncmp(msg, "RuntimeError", 12) == 0) {
        return PYRS_EXC_RUNTIME;
    }
    /* UnboundLocalError, FileNotFoundError, EOFError, MemoryError, … —
     * only bare `except:` matches (not RuntimeError). */
    return PYRS_EXC_OTHER;
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
        longjmp(g_exc_frames->buf, 1);
    }
    die_uncaught(g_exc_msg);
}

_Noreturn void pyrs_die(const char *msg) {
    int ty = classify_exc_msg(msg);
    g_exc_type = ty;
    snprintf(g_exc_msg, sizeof g_exc_msg, "%s", msg);
    if (g_exc_frames != NULL) {
        longjmp(g_exc_frames->buf, 1);
    }
    die_uncaught(msg);
}

static void *xmalloc(size_t n) {
    void *p = malloc(n);
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
    g_exc_frames = f;
    return f;
}

/* Note: do not wrap setjmp in a C function — longjmp must restore to the
 * LLVM call site of setjmp (jmp_buf is the first field of PyrsExcFrame). */

void pyrs_try_pop(void) {
    if (g_exc_frames != NULL) {
        g_exc_frames = g_exc_frames->prev;
    }
}

int pyrs_exc_type(void) {
    return g_exc_type;
}

/* message body only (no "Type: " prefix), as a PyrsStr */
PyrsStr *pyrs_exc_message(void) {
    const char *body = exc_msg_body(g_exc_msg);
    size_t n = strlen(body);
    PyrsStr *s = xmalloc(sizeof(long long) + n + 1);
    s->len = (long long)n;
    memcpy(s->data, body, n + 1);
    return s;
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
        longjmp(g_exc_frames->buf, 1);
    }
    die_uncaught(g_exc_msg[0] ? g_exc_msg : "RuntimeError: unknown error");
}

/* zero-initialized (null) str/list locals read before assignment land here
 * instead of segfaulting */
static void check_ref(const void *p) {
    if (p == NULL) {
        pyrs_die("UnboundLocalError: value used before assignment");
    }
}

static PyrsStr *str_alloc(long long len) {
    PyrsStr *s = xmalloc(sizeof(long long) + (size_t)len + 1);
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

void pyrs_print_int(long long v) {
    printf("%lld", v);
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

static void print_slot(long long slot, int tag) {
    switch (tag) {
    case TAG_INT:
        printf("%lld", slot);
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
    default:
        /* tag encoding for nested list: 4 + 8 * inner_tag */
        if (tag >= 4 && ((tag - 4) % 8) == 0) {
            pyrs_print_list((const PyrsList *)(uintptr_t)slot, (tag - 4) / 8);
        } else {
            printf("<object>");
        }
        break;
    }
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

/* first index of t in s, or -1; the empty needle is found at 0 */
long long pyrs_str_find(const PyrsStr *s, const PyrsStr *t) {
    check_ref(s);
    check_ref(t);
    if (t->len > s->len) {
        return -1;
    }
    for (long long i = 0; i + t->len <= s->len; i++) {
        if (memcmp(s->data + i, t->data, (size_t)t->len) == 0) {
            return i;
        }
    }
    return -1;
}

/* last index of t in s, or -1; the empty needle is found at len(s) */
long long pyrs_str_rfind(const PyrsStr *s, const PyrsStr *t) {
    check_ref(s);
    check_ref(t);
    if (t->len > s->len) {
        return -1;
    }
    if (t->len == 0) {
        return s->len;
    }
    for (long long i = s->len - t->len; i >= 0; i--) {
        if (memcmp(s->data + i, t->data, (size_t)t->len) == 0) {
            return i;
        }
    }
    return -1;
}

/* like rfind, but trap when absent (CPython: ValueError: substring not found) */
long long pyrs_str_rindex(const PyrsStr *s, const PyrsStr *t) {
    long long i = pyrs_str_rfind(s, t);
    if (i < 0) {
        pyrs_die("ValueError: substring not found");
    }
    return i;
}

/* non-overlapping occurrences; Python counts len+1 for an empty needle */
long long pyrs_str_count(const PyrsStr *s, const PyrsStr *t) {
    check_ref(s);
    check_ref(t);
    if (t->len == 0) {
        return s->len + 1;
    }
    long long n = 0;
    long long i = 0;
    while (i + t->len <= s->len) {
        if (memcmp(s->data + i, t->data, (size_t)t->len) == 0) {
            n++;
            i += t->len;
        } else {
            i++;
        }
    }
    return n;
}

PyrsStr *pyrs_str_replace(const PyrsStr *s, const PyrsStr *old, const PyrsStr *new_s) {
    check_ref(s);
    check_ref(old);
    check_ref(new_s);
    /* Python: an empty old inserts new between every character */
    if (old->len == 0) {
        long long n = s->len + (s->len + 1) * new_s->len;
        PyrsStr *r = str_alloc(n);
        char *p = r->data;
        for (long long i = 0; i < s->len; i++) {
            memcpy(p, new_s->data, (size_t)new_s->len);
            p += new_s->len;
            *p++ = s->data[i];
        }
        memcpy(p, new_s->data, (size_t)new_s->len);
        return r;
    }
    long long count = pyrs_str_count(s, old);
    if (count == 0) {
        /* immutable, so sharing is safe — but return a copy of the header
         * shape anyway to keep ownership simple */
        return str_sub(s, 0, s->len);
    }
    long long n = s->len + count * (new_s->len - old->len);
    PyrsStr *r = str_alloc(n);
    char *p = r->data;
    long long i = 0;
    while (i < s->len) {
        if (i + old->len <= s->len && memcmp(s->data + i, old->data, (size_t)old->len) == 0) {
            memcpy(p, new_s->data, (size_t)new_s->len);
            p += new_s->len;
            i += old->len;
        } else {
            *p++ = s->data[i++];
        }
    }
    return r;
}

/* split on whitespace runs, skipping empty parts (Python's s.split()) */
PyrsList *pyrs_str_split_ws(const PyrsStr *s) {
    check_ref(s);
    PyrsList *r = pyrs_list_new(4);
    long long i = 0;
    while (i < s->len) {
        while (i < s->len && is_py_space(s->data[i])) {
            i++;
        }
        long long start = i;
        while (i < s->len && !is_py_space(s->data[i])) {
            i++;
        }
        if (i > start) {
            pyrs_list_push(r, (long long)str_sub(s, start, i - start));
        }
    }
    return r;
}

/* split on a nonempty separator, keeping empty parts (Python's
 * s.split(sep)) */
PyrsList *pyrs_str_split(const PyrsStr *s, const PyrsStr *sep) {
    check_ref(s);
    check_ref(sep);
    if (sep->len == 0) {
        pyrs_die("ValueError: empty separator");
    }
    PyrsList *r = pyrs_list_new(4);
    long long start = 0;
    long long i = 0;
    while (i + sep->len <= s->len) {
        if (memcmp(s->data + i, sep->data, (size_t)sep->len) == 0) {
            pyrs_list_push(r, (long long)str_sub(s, start, i - start));
            i += sep->len;
            start = i;
        } else {
            i++;
        }
    }
    pyrs_list_push(r, (long long)str_sub(s, start, s->len - start));
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

PyrsStr *pyrs_str_from_int(long long v) {
    char buf[24];
    int n = snprintf(buf, sizeof buf, "%lld", v);
    PyrsStr *r = str_alloc(n);
    memcpy(r->data, buf, (size_t)n);
    return r;
}

PyrsStr *pyrs_str_from_float(double v) {
    char buf[40];
    format_double(v, buf);
    size_t n = strlen(buf);
    PyrsStr *r = str_alloc((long long)n);
    memcpy(r->data, buf, n);
    return r;
}

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

/* ---- lists ---- */

PyrsList *pyrs_list_new(long long cap) {
    if (cap < 4) {
        cap = 4;
    }
    PyrsList *l = xmalloc(sizeof(PyrsList));
    l->len = 0;
    l->cap = cap;
    l->data = xmalloc((size_t)cap * sizeof(long long));
    return l;
}

void pyrs_list_push(PyrsList *l, long long slot) {
    check_ref(l);
    if (l->len == l->cap) {
        long long cap = l->cap * 2;
        long long *data = xmalloc((size_t)cap * sizeof(long long));
        memcpy(data, l->data, (size_t)l->len * sizeof(long long));
        /* the old block is intentionally leaked (no GC yet) */
        l->data = data;
        l->cap = cap;
    }
    l->data[l->len++] = slot;
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
    if (tag >= 4 && ((tag - 4) % 8) == 0) {
        /* nested list: slots are list pointers; inner tag = (tag-4)/8 */
        int inner = (tag - 4) / 8;
        return pyrs_list_eq((const PyrsList *)(uintptr_t)a, (const PyrsList *)(uintptr_t)b, inner);
    }
    switch (tag) {
    case 0:
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
        memcpy(data, l->data, (size_t)l->len * sizeof(long long));
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

void pyrs_list_clear(PyrsList *l) {
    check_ref(l);
    l->len = 0;
}

/* qsort needs a tag; single-threaded compiler runtime is fine */
static int sort_elem_tag;

static int cmp_slots_qsort(const void *pa, const void *pb) {
    long long a = *(const long long *)pa;
    long long b = *(const long long *)pb;
    int tag = sort_elem_tag;
    switch (tag) {
    case 0: /* int */
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
    default:
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

    PyrsFile *f = xmalloc(sizeof(PyrsFile));
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
        buf = bigger;
        cap = newcap;
    }
    PyrsStr *r = str_alloc((long long)len);
    memcpy(r->data, buf, len);
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
    PyrsList *out = pyrs_list_new(8);
    for (;;) {
        PyrsStr *line = pyrs_file_readline(f);
        if (line->len == 0) {
            break;
        }
        pyrs_list_push(out, (long long)line);
    }
    return out;
}

/* returns the number of characters written, like Python; flushed so data
 * survives even if close() is never called (nothing is freed anyway) */
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
    static PyrsList *cached = NULL;
    if (cached == NULL) {
        cached = pyrs_list_new(g_argc > 0 ? g_argc : 1);
        for (int i = 0; i < g_argc; i++) {
            pyrs_list_push(cached, (long long)str_from_cstr(g_argv[i]));
        }
    }
    return cached;
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

/* ---- integer power ---- */

/* repeated squaring; unsigned internally so overflow wraps (like the rest
 * of PyRs int arithmetic) instead of being UB */
long long pyrs_ipow(long long base, long long exp) {
    if (exp < 0) {
        pyrs_die(
            "ValueError: integer to a negative power is not supported; "
            "use a float base (e.g. 2.0 ** -1)");
    }
    unsigned long long result = 1;
    unsigned long long b = (unsigned long long)base;
    while (exp > 0) {
        if (exp & 1) {
            result *= b;
        }
        b *= b;
        exp >>= 1;
    }
    return (long long)result;
}

/* ---- tuples ---- */

/* Self-describing: each element carries its print/eq tag. Layout:
 *   { i64 len; i64 *data; int *tags; }
 * First field is len so pyrs_len works. */
struct PyrsTuple {
    long long len;
    long long *data;
    int *tags;
};

PyrsTuple *pyrs_tuple_new(long long n) {
    if (n < 0) {
        n = 0;
    }
    PyrsTuple *t = xmalloc(sizeof(PyrsTuple));
    t->len = n;
    t->data = n > 0 ? xmalloc((size_t)n * sizeof(long long)) : NULL;
    t->tags = n > 0 ? xmalloc((size_t)n * sizeof(int)) : NULL;
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

static unsigned long long hash_key(long long key, int tag) {
    if (tag == TAG_INT) {
        unsigned long long x = (unsigned long long)key;
        x ^= x >> 30;
        x *= 0xbf58476d1ce4e5b9ULL;
        x ^= x >> 27;
        x *= 0x94d049bb133111ebULL;
        x ^= x >> 31;
        return x;
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
    PyrsDict *d = xmalloc(sizeof(PyrsDict));
    d->len = 0;
    d->cap = 8;
    d->table = xmalloc((size_t)d->cap * sizeof(DictSlot));
    memset(d->table, 0, (size_t)d->cap * sizeof(DictSlot));
    d->order_cap = 8;
    d->order_len = 0;
    d->order = xmalloc((size_t)d->order_cap * sizeof(long long));
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

    d->cap *= 2;
    d->table = xmalloc((size_t)d->cap * sizeof(DictSlot));
    memset(d->table, 0, (size_t)d->cap * sizeof(DictSlot));
    d->order_cap = d->cap;
    d->order = xmalloc((size_t)d->order_cap * sizeof(long long));
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
    (void)old_cap;
    (void)old; /* leaked */
    (void)old_order;
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
        memcpy(no, d->order, (size_t)d->order_len * sizeof(long long));
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
            char buf[64];
            snprintf(buf, sizeof buf, "KeyError: %lld", key);
            pyrs_die(buf);
        }
    }
    return d->table[idx].val;
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
            char buf[64];
            snprintf(buf, sizeof buf, "KeyError: %lld", key);
            pyrs_die(buf);
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
            char buf[64];
            snprintf(buf, sizeof buf, "KeyError: %lld", key);
            pyrs_die(buf);
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
    PyrsSet *s = xmalloc(sizeof(PyrsSet));
    s->len = 0;
    s->cap = 8;
    s->table = xmalloc((size_t)s->cap * sizeof(SetSlot));
    memset(s->table, 0, (size_t)s->cap * sizeof(SetSlot));
    s->order_cap = 8;
    s->order_len = 0;
    s->order = xmalloc((size_t)s->order_cap * sizeof(long long));
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
    s->cap *= 2;
    s->table = xmalloc((size_t)s->cap * sizeof(SetSlot));
    memset(s->table, 0, (size_t)s->cap * sizeof(SetSlot));
    s->order_cap = s->cap;
    s->order = xmalloc((size_t)s->order_cap * sizeof(long long));
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
    (void)old;
    (void)old_order;
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
        memcpy(no, s->order, (size_t)s->order_len * sizeof(long long));
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
            char buf[64];
            snprintf(buf, sizeof buf, "KeyError: %lld", key);
            pyrs_die(buf);
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
    char *end = NULL;
    errno = 0;
    long long v = strtoll(*p, &end, 10);
    if (end == *p || errno == ERANGE) {
        pyrs_die("ValueError: Expecting value");
    }
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
    char tmp[32];
    snprintf(tmp, sizeof(tmp), "%lld", v);
    jbuf_puts(b, tmp);
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
    jbuf_init(&b);
    json_dumps_into(&b, slot, tag);
    PyrsStr *r = str_from_cstr(b.data);
    free(b.data);
    return r;
}

/* ---- cells (nonlocal / mutable free vars) ---- */
typedef struct {
    long long slot;
    int bound; /* 0 = unbound (NameError on load); 1 = assigned */
} PyrsCell;

PyrsCell *pyrs_cell_new(long long slot) {
    PyrsCell *c = (PyrsCell *)xmalloc(sizeof(PyrsCell));
    c->slot = slot;
    c->bound = 1;
    return c;
}

/* Unbound cell for late free-var capture (CPython empty cell until assign). */
PyrsCell *pyrs_cell_new_unbound(void) {
    PyrsCell *c = (PyrsCell *)xmalloc(sizeof(PyrsCell));
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
    PyrsClosure *c = (PyrsClosure *)xmalloc(sz);
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
    long long try_phases[PYRS_GEN_MAX_TRY]; /* phase per active try across yield */
    long long nlocals;
    long long locals[];  /* frame */
} PyrsGen;

PyrsGen *pyrs_gen_new(void *code, long long nlocals) {
    size_t sz = sizeof(PyrsGen) + (size_t)nlocals * sizeof(long long);
    PyrsGen *g = (PyrsGen *)xmalloc(sz);
    g->code = code;
    g->state = 0;
    g->done = 0;
    g->yield_slot = 0;
    g->return_slot = 0;
    g->return_set = 0;
    g->closing = 0;
    for (int i = 0; i < PYRS_GEN_MAX_TRY; i++) {
        g->try_phases[i] = 0;
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

int pyrs_gen_closing(PyrsGen *g) {
    check_ref(g);
    return g->closing ? 1 : 0;
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
