/* PyRs runtime: tiny C support library linked into every compiled program.
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

#include <errno.h>
#include <limits.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

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
            print_str_repr((const PyrsStr *)slot);
            break;
        default:
            /* tag >= 4: the element is itself a list; decode its element
             * tag and recurse */
            pyrs_print_list((const PyrsList *)slot, (tag - 4) / 8);
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
 * list-of-X is 4 + 8 * tag(X) (recursive). */
static int slot_eq(long long a, long long b, int tag);
int pyrs_list_eq(const PyrsList *a, const PyrsList *b, int tag);

static int slot_eq(long long a, long long b, int tag) {
    if (tag >= 4) {
        /* nested list: slots are list pointers; inner tag = (tag-4)/8 */
        int inner = (tag - 4) / 8;
        return pyrs_list_eq((const PyrsList *)a, (const PyrsList *)b, inner);
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
