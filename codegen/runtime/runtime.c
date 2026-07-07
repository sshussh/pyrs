/* pyrs runtime: tiny C support library linked into every compiled program.
 *
 * Printing matches CPython:
 * - floats use the shortest representation that round-trips, and whole
 *   floats keep their ".0" (1.0 prints as "1.0", not "1")
 * - bools print True/False; lists print like [1, 2, 3] / ['a', 'b']
 * - runtime errors (ZeroDivisionError, IndexError, ...) print to stderr
 *   and exit(1)
 *
 * Strings are immutable, length-prefixed blobs; lists are growable arrays
 * of 8-byte value slots. Both are heap-allocated and never freed — fine
 * for short-lived compiled programs, documented as a known limitation.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

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

_Noreturn void pyrs_die(const char *msg) {
    fflush(stdout);
    fputs(msg, stderr);
    fputc('\n', stderr);
    exit(1);
}

static void *xmalloc(size_t n) {
    void *p = malloc(n);
    if (p == NULL) {
        pyrs_die("MemoryError: out of memory");
    }
    return p;
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

/* element tags match codegen: 0=int 1=float 2=bool 3=str */
void pyrs_print_list(const PyrsList *l, int tag) {
    check_ref(l);
    fputc('[', stdout);
    for (long long i = 0; i < l->len; i++) {
        if (i > 0) {
            fputs(", ", stdout);
        }
        long long slot = l->data[i];
        switch (tag) {
        case 0:
            printf("%lld", slot);
            break;
        case 1: {
            double d;
            memcpy(&d, &slot, sizeof d);
            pyrs_print_float(d);
            break;
        }
        case 2:
            fputs(slot ? "True" : "False", stdout);
            break;
        case 3:
            /* repr-style quotes; escape sequences are not re-escaped */
            fputc('\'', stdout);
            pyrs_print_str((const PyrsStr *)slot);
            fputc('\'', stdout);
            break;
        }
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

/* clamp a slice bound Python-style: negative counts from the end, then
 * clip to [0, len] */
static long long clamp_bound(long long i, long long len) {
    if (i < 0) {
        i += len;
        if (i < 0) {
            i = 0;
        }
    }
    if (i > len) {
        i = len;
    }
    return i;
}

PyrsStr *pyrs_str_slice(const PyrsStr *s, long long lo, long long hi) {
    check_ref(s);
    lo = clamp_bound(lo, s->len);
    hi = clamp_bound(hi, s->len);
    if (hi <= lo) {
        return EMPTY_STR;
    }
    long long n = hi - lo;
    if (n == 1) {
        return single_char((unsigned char)s->data[lo]);
    }
    PyrsStr *r = str_alloc(n);
    memcpy(r->data, s->data + lo, (size_t)n);
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

PyrsStr *pyrs_str_from_bool(int v) {
    const char *text = v ? "True" : "False";
    size_t n = strlen(text);
    PyrsStr *r = str_alloc((long long)n);
    memcpy(r->data, text, n);
    return r;
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

PyrsList *pyrs_list_slice(const PyrsList *l, long long lo, long long hi) {
    check_ref(l);
    lo = clamp_bound(lo, l->len);
    hi = clamp_bound(hi, l->len);
    long long n = hi > lo ? hi - lo : 0;
    PyrsList *r = pyrs_list_new(n);
    if (n > 0) {
        memcpy(r->data, l->data + lo, (size_t)n * sizeof(long long));
        r->len = n;
    }
    return r;
}

/* element tags match codegen: 0=int 1=float 2=bool 3=str */
int pyrs_list_contains(const PyrsList *l, long long slot, int tag) {
    check_ref(l);
    for (long long i = 0; i < l->len; i++) {
        switch (tag) {
        case 0:
        case 2:
            if (l->data[i] == slot) {
                return 1;
            }
            break;
        case 1: {
            /* numeric equality: 0.0 == -0.0, nan != nan */
            double a, b;
            memcpy(&a, &l->data[i], sizeof a);
            memcpy(&b, &slot, sizeof b);
            if (a == b) {
                return 1;
            }
            break;
        }
        case 3:
            if (pyrs_str_cmp((const PyrsStr *)l->data[i], (const PyrsStr *)slot) == 0) {
                return 1;
            }
            break;
        }
    }
    return 0;
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

/* ---- integer power ---- */

/* repeated squaring; unsigned internally so overflow wraps (like the rest
 * of pyrs int arithmetic) instead of being UB */
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
