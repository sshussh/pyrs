/* Nonmoving mark-sweep collector for the current PyRs raw-pointer ABI. */

#include "gc.h"

#include <errno.h>
#include <inttypes.h>
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(__GNUC__) || defined(__clang__)
#define PYRS_NOINLINE __attribute__((noinline))
#define PYRS_NO_ASAN __attribute__((no_sanitize_address))
#else
#define PYRS_NOINLINE
#define PYRS_NO_ASAN
#endif

typedef union PyrsGcHeader PyrsGcHeader;

union PyrsGcHeader {
    struct {
        PyrsGcHeader *next;
        size_t size;
        size_t external_size;
        unsigned int kind;
        unsigned int marked;
    } fields;
    max_align_t alignment;
};

typedef struct {
    void *start;
    size_t size;
} PyrsGcRootRange;

typedef struct {
    uintptr_t start;
    uintptr_t end;
    PyrsGcHeader *owner;
} PyrsGcRange;

typedef struct {
    PyrsGcRange *items;
    size_t len;
    size_t cap;
    /* Envelope of every range, so a candidate that cannot point into any
     * object is rejected by two compares instead of a binary search. Almost
     * every word offered to the collector is not a pointer: tagged small ints
     * are odd and tiny, doubles bit-cast into list slots are enormous, and a
     * `list[int]` of N elements offers N of them on every collection. */
    uintptr_t lo;
    uintptr_t hi;
} PyrsGcRanges;

/* One filing of a range under one granule. `range` is the index into the
 * range array plus one, so a zeroed slot reads as empty. `tag` is the low bits
 * of the granule, enough to skip a slot displaced from another chain without
 * touching the range array. */
typedef struct {
    uint32_t tag;
    uint32_t range;
} PyrsGcIndexSlot;

typedef struct {
    PyrsGcIndexSlot *slots;
    size_t mask;
    /* Ranges too wide to file by granule, sorted by start and searched. */
    PyrsGcRange *large;
    size_t large_len;
} PyrsGcIndex;

typedef struct {
    PyrsGcRanges *ranges;
    PyrsGcIndex *index;
    PyrsGcHeader **work;
    size_t work_len;
    size_t work_cap;
} PyrsGcMarkContext;

static PyrsGcHeader *g_objects = NULL;
static PyrsGcRootRange *g_roots = NULL;
static size_t g_roots_len = 0;
static size_t g_roots_cap = 0;
static PyrsGcRoot *g_scoped_roots = NULL;
static uintptr_t g_stack_bound = 0;
static int g_stack_grows_down = 1;

static size_t g_live_bytes = 0;
static size_t g_live_objects = 0;
static size_t g_allocated_bytes = 0;
static size_t g_reclaimed_bytes = 0;
static size_t g_allocated_since_gc = 0;
static size_t g_collections = 0;
static size_t g_threshold = 1024U * 1024U;
static size_t g_base_threshold = 1024U * 1024U;

static int g_initialized = 0;
static int g_enabled = 1;
static int g_stress = 0;
static int g_stats = 0;
static int g_threshold_is_explicit = 0;
static int g_collecting = 0;

/* AddressSanitizer can move address-taken C locals to its fake stack.  Such an
 * address is not in the process's native stack mapping, so use the hardware
 * stack pointer where supported.  The no-ASan fallback likewise keeps its
 * marker on the real native stack. */
static PYRS_NOINLINE PYRS_NO_ASAN uintptr_t capture_stack_pointer(void) {
    uintptr_t stack_pointer = 0;
#if (defined(__GNUC__) || defined(__clang__)) && defined(__x86_64__)
    __asm__ volatile("movq %%rsp, %0" : "=r"(stack_pointer) : : "memory");
#elif (defined(__GNUC__) || defined(__clang__)) && defined(__aarch64__)
    __asm__ volatile("mov %0, sp" : "=r"(stack_pointer) : : "memory");
#else
    volatile uintptr_t stack_marker = 0;
    stack_pointer = (uintptr_t)&stack_marker;
#endif
    return stack_pointer;
}

static _Noreturn void gc_oom(void) {
    fflush(stdout);
    fputs("MemoryError: out of memory\n", stderr);
    exit(1);
}

static void *gc_raw_malloc(size_t size) {
    void *p = malloc(size == 0 ? 1 : size);
    if (p == NULL) {
        gc_oom();
    }
    return p;
}

static void *gc_raw_realloc(void *old, size_t size) {
    void *p = realloc(old, size == 0 ? 1 : size);
    if (p == NULL) {
        gc_oom();
    }
    return p;
}

static int env_truthy(const char *name) {
    const char *value = getenv(name);
    return value != NULL && value[0] != '\0' && strcmp(value, "0") != 0 &&
           strcmp(value, "false") != 0 && strcmp(value, "no") != 0;
}

static size_t env_size(const char *name, size_t fallback, int *was_set) {
    const char *value = getenv(name);
    if (value == NULL || value[0] == '\0') {
        *was_set = 0;
        return fallback;
    }
    errno = 0;
    char *end = NULL;
    unsigned long long parsed = strtoull(value, &end, 10);
    if (errno != 0 || end == value || *end != '\0') {
        *was_set = 0;
        return fallback;
    }
    *was_set = 1;
    if (parsed < 1024ULL) {
        parsed = 1024ULL;
    }
    if (parsed > (unsigned long long)SIZE_MAX) {
        parsed = (unsigned long long)SIZE_MAX;
    }
    return (size_t)parsed;
}

/* Linux exposes the actual mapped stack interval.  Resolve it from the real
 * hardware stack pointer, rather than an address-taken local that a sanitizer
 * may relocate. Other platforms retain the caller-supplied anchor fallback. */
static int discover_stack_mapping(uintptr_t address, uintptr_t *low,
                                  uintptr_t *high) {
#if defined(__linux__)
    FILE *maps = fopen("/proc/self/maps", "r");
    if (maps == NULL) {
        return 0;
    }
    char line[512];
    int found = 0;
    while (fgets(line, sizeof(line), maps) != NULL) {
        uintptr_t start = 0;
        uintptr_t end = 0;
        if (sscanf(line, "%" SCNxPTR "-%" SCNxPTR, &start, &end) == 2 &&
            address >= start && address < end) {
            *low = start;
            *high = end;
            found = 1;
            break;
        }
    }
    fclose(maps);
    return found;
#else
    (void)address;
    (void)low;
    (void)high;
    return 0;
#endif
}

static void gc_print_stats(void) {
    if (!g_stats) {
        return;
    }
    fprintf(stderr,
            "[pyrs-gc] collections=%zu allocated=%zu reclaimed=%zu live=%zu "
            "objects=%zu\n",
            g_collections, g_allocated_bytes, g_reclaimed_bytes, g_live_bytes,
            g_live_objects);
}

void pyrs_gc_init(void *stack_anchor) {
    uintptr_t stack_pointer = capture_stack_pointer();
    if (stack_anchor != NULL) {
        uintptr_t anchor = (uintptr_t)stack_anchor;
        g_stack_grows_down = stack_pointer < anchor;
        g_stack_bound = anchor;
    }
    uintptr_t low = 0;
    uintptr_t high = 0;
    if (discover_stack_mapping(stack_pointer, &low, &high)) {
        uintptr_t anchor = (uintptr_t)stack_anchor;
        if (stack_anchor != NULL && anchor >= low && anchor < high) {
            g_stack_grows_down = stack_pointer < anchor;
        }
        /* /proc mappings are half-open; scan_stack accepts an inclusive
         * endpoint, so keep its final word wholly inside the mapping. */
        g_stack_bound =
            g_stack_grows_down && high >= sizeof(uintptr_t)
                ? high - sizeof(uintptr_t)
                : low;
    }
    if (g_initialized) {
        return;
    }
    g_initialized = 1;

    const char *plan = getenv("PYRS_GC");
    if (plan != NULL && (strcmp(plan, "none") == 0 ||
                         strcmp(plan, "nogc") == 0 ||
                         strcmp(plan, "off") == 0)) {
        g_enabled = 0;
    }
    g_stress = env_truthy("PYRS_GC_STRESS");
    g_stats = env_truthy("PYRS_GC_STATS");
    g_base_threshold =
        env_size("PYRS_GC_THRESHOLD", 1024U * 1024U,
                 &g_threshold_is_explicit);
    g_threshold = g_base_threshold;
    if (atexit(gc_print_stats) != 0) {
        gc_oom();
    }
}

void pyrs_gc_add_root_range(void *start, size_t size) {
    if (start == NULL || size == 0) {
        return;
    }
    if (g_roots_len == g_roots_cap) {
        size_t next = g_roots_cap == 0 ? 16 : g_roots_cap * 2;
        g_roots = gc_raw_realloc(g_roots, next * sizeof(*g_roots));
        g_roots_cap = next;
    }
    g_roots[g_roots_len].start = start;
    g_roots[g_roots_len].size = size;
    g_roots_len++;
}

void pyrs_gc_add_global(void *start, size_t size) {
    pyrs_gc_add_root_range(start, size);
}

void pyrs_gc_root_push(PyrsGcRoot *root, void *start, size_t size) {
    if (root == NULL) {
        return;
    }
    root->start = start;
    root->size = size;
    root->prev = g_scoped_roots;
    g_scoped_roots = root;
}

void pyrs_gc_root_pop(PyrsGcRoot *root) {
    if (root == NULL) {
        return;
    }
    if (g_scoped_roots == root) {
        g_scoped_roots = root->prev;
    } else {
        PyrsGcRoot *cursor = g_scoped_roots;
        while (cursor != NULL && cursor->prev != root) {
            cursor = cursor->prev;
        }
        if (cursor != NULL) {
            cursor->prev = root->prev;
        }
    }
    root->prev = NULL;
    root->start = NULL;
    root->size = 0;
}

static void ranges_push(PyrsGcRanges *ranges, uintptr_t start, size_t size,
                        PyrsGcHeader *owner) {
    if (start == 0 || size == 0) {
        return;
    }
    uintptr_t end = start + size;
    if (end < start) {
        end = UINTPTR_MAX;
    }
    if (ranges->len == ranges->cap) {
        size_t next = ranges->cap == 0 ? 64 : ranges->cap * 2;
        ranges->items =
            gc_raw_realloc(ranges->items, next * sizeof(*ranges->items));
        ranges->cap = next;
    }
    ranges->items[ranges->len].start = start;
    ranges->items[ranges->len].end = end;
    ranges->items[ranges->len].owner = owner;
    if (ranges->len == 0 || start < ranges->lo) {
        ranges->lo = start;
    }
    if (ranges->len == 0 || end > ranges->hi) {
        ranges->hi = end;
    }
    ranges->len++;
}

typedef struct {
    PyrsGcRanges *ranges;
    PyrsGcHeader *owner;
} PyrsGcOwnedRangeContext;

static void add_owned_range(void *start, size_t size, void *opaque) {
    PyrsGcOwnedRangeContext *context = opaque;
    ranges_push(context->ranges, (uintptr_t)start, size, context->owner);
}

/* Address granule an object range is filed under.
 *
 * Marking is conservative, so every candidate word has to be answered with
 * "which object, if any, contains this address". Sorting all ranges and
 * binary searching them costs an O(n log n) sort before marking can start and
 * ~log2(n) cache-missing probes per real pointer; with half a million live
 * objects that was two thirds of the `objects` benchmark's runtime.
 *
 * Instead every range is filed under each 256-byte granule it covers, in an
 * open-addressed table built in one linear pass. A lookup hashes the
 * candidate's granule and scans until an empty slot, which is O(1) expected
 * and touches one or two cache lines.
 *
 * A range spanning more granules than PYRS_GC_INDEX_MAX_SPAN would file too
 * many entries, so those go to a small sorted tier instead. In practice that
 * is the handful of large `data` buffers a program's lists own. */
#define PYRS_GC_GRANULE_SHIFT 8
#define PYRS_GC_INDEX_MAX_SPAN 8

static uintptr_t granule_of(uintptr_t address) {
    return address >> PYRS_GC_GRANULE_SHIFT;
}

static int compare_ranges(const void *left, const void *right) {
    const PyrsGcRange *a = left;
    const PyrsGcRange *b = right;
    if (a->start < b->start) {
        return -1;
    }
    if (a->start > b->start) {
        return 1;
    }
    if (a->end < b->end) {
        return -1;
    }
    if (a->end > b->end) {
        return 1;
    }
    return 0;
}

/* Index of the last range whose start is <= candidate, in a sorted array. */
static size_t range_upper_bound(const PyrsGcRange *items, size_t len,
                                uintptr_t candidate) {
    size_t lo = 0;
    size_t hi = len;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        if (items[mid].start <= candidate) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    return lo;
}

static size_t index_hash(uintptr_t granule, size_t mask) {
    /* Fibonacci hashing: the granule's low bits are the ones that vary, so
     * they have to reach the top of the word before masking. */
    return (size_t)((granule * 0x9E3779B97F4A7C15ULL) >> 32) & mask;
}

/* File `range` under `granule`, growing nothing: the table is sized up front
 * for exactly the number of entries the ranges need. */
static void index_insert(PyrsGcIndex *index, uintptr_t granule, size_t range) {
    size_t slot = index_hash(granule, index->mask);
    while (index->slots[slot].range != 0) {
        slot = (slot + 1) & index->mask;
    }
    index->slots[slot].tag = (uint32_t)granule;
    index->slots[slot].range = (uint32_t)(range + 1);
}

static void index_build(PyrsGcIndex *index, PyrsGcRanges *ranges) {
    memset(index, 0, sizeof(*index));
    if (ranges->len == 0) {
        return;
    }
    /* One pass to size both tiers, so neither ever reallocates. */
    size_t entries = 0;
    size_t large = 0;
    for (size_t i = 0; i < ranges->len; i++) {
        uintptr_t first = granule_of(ranges->items[i].start);
        uintptr_t last = granule_of(ranges->items[i].end);
        uintptr_t span = last - first + 1;
        if (span > PYRS_GC_INDEX_MAX_SPAN) {
            large++;
        } else {
            entries += (size_t)span;
        }
    }

    if (large > 0) {
        index->large = gc_raw_malloc(large * sizeof(*index->large));
    }
    /* Load factor 1/2, so probe chains stay short. */
    size_t capacity = 16;
    while (capacity < entries * 2) {
        capacity *= 2;
    }
    index->slots = gc_raw_malloc(capacity * sizeof(*index->slots));
    memset(index->slots, 0, capacity * sizeof(*index->slots));
    index->mask = capacity - 1;

    for (size_t i = 0; i < ranges->len; i++) {
        uintptr_t first = granule_of(ranges->items[i].start);
        uintptr_t last = granule_of(ranges->items[i].end);
        if (last - first + 1 > PYRS_GC_INDEX_MAX_SPAN) {
            index->large[index->large_len++] = ranges->items[i];
            continue;
        }
        for (uintptr_t g = first; g <= last; g++) {
            index_insert(index, g, i);
        }
    }
    if (index->large_len > 1) {
        qsort(index->large, index->large_len, sizeof(*index->large),
              compare_ranges);
    }
}

static void index_free(PyrsGcIndex *index) {
    free(index->slots);
    free(index->large);
    memset(index, 0, sizeof(*index));
}

static void mark_owner(PyrsGcHeader *owner, PyrsGcMarkContext *context) {
    if (owner == NULL || owner->fields.marked) {
        return;
    }
    owner->fields.marked = 1;
    if (context->work_len >= context->work_cap) {
        gc_oom();
    }
    context->work[context->work_len++] = owner;
}

static void mark_candidate(uintptr_t candidate, void *opaque) {
    PyrsGcMarkContext *context = opaque;
    /* Reject outside the heap envelope before looking anything up. This is the
     * whole cost of tracing a large `list[int]` or `list[float]`, whose slots
     * hold no pointers at all but are still offered one by one. */
    if (candidate < context->ranges->lo || candidate > context->ranges->hi) {
        return;
    }
    PyrsGcIndex *index = context->index;

    /* Inclusive end matching deliberately keeps an owner alive from a valid
     * C one-past pointer. Marking every range that contains the candidate --
     * rather than only the nearest by start -- also covers the case where that
     * address is where an adjacent allocation begins: both owners are retained
     * rather than letting ordering decide. */
    if (index->slots != NULL) {
        uintptr_t granule = granule_of(candidate);
        uint32_t tag = (uint32_t)granule;
        size_t slot = index_hash(granule, index->mask);
        /* Entries for this granule were inserted from here onwards, so
         * scanning to the first empty slot sees all of them. */
        while (index->slots[slot].range != 0) {
            if (index->slots[slot].tag == tag) {
                const PyrsGcRange *range =
                    &context->ranges->items[index->slots[slot].range - 1];
                if (candidate >= range->start && candidate <= range->end) {
                    mark_owner(range->owner, context);
                }
            }
            slot = (slot + 1) & index->mask;
        }
    }

    if (index->large_len == 0) {
        return;
    }
    size_t upper = range_upper_bound(index->large, index->large_len, candidate);
    if (upper == 0) {
        return;
    }
    const PyrsGcRange *range = &index->large[upper - 1];
    if (candidate >= range->start && candidate <= range->end) {
        mark_owner(range->owner, context);
    }
    if (candidate == range->start && upper >= 2) {
        const PyrsGcRange *previous = &index->large[upper - 2];
        if (candidate == previous->end) {
            mark_owner(previous->owner, context);
        }
    }
}

/* Bulk candidate marking over a contiguous slot array.
 *
 * The envelope bounds are hoisted out of the loop and `mark_candidate` is a
 * direct call here, so a slot that cannot be a pointer costs two compares
 * rather than an indirect call into the runtime and back. */
static void mark_slots(const long long *slots, size_t count, void *opaque) {
    PyrsGcMarkContext *context = opaque;
    if (slots == NULL) {
        return;
    }
    uintptr_t lo = context->ranges->lo;
    uintptr_t hi = context->ranges->hi;
    for (size_t i = 0; i < count; i++) {
        uintptr_t candidate = (uintptr_t)(unsigned long long)slots[i];
        if (candidate < lo || candidate > hi) {
            continue;
        }
        mark_candidate(candidate, context);
    }
}

static void scan_words(const void *start, size_t size,
                       PyrsGcMarkContext *context) {
    if (start == NULL || size < sizeof(uintptr_t)) {
        return;
    }
    const unsigned char *bytes = start;
    for (size_t offset = 0; offset + sizeof(uintptr_t) <= size;
         offset += sizeof(uintptr_t)) {
        uintptr_t candidate = 0;
        memcpy(&candidate, bytes + offset, sizeof(candidate));
        mark_candidate(candidate, context);
    }
}

static PYRS_NO_ASAN void scan_stack(uintptr_t top, uintptr_t bottom,
                                    PyrsGcMarkContext *context) {
    if (top == 0 || bottom == 0) {
        return;
    }
    uintptr_t lo = top < bottom ? top : bottom;
    uintptr_t hi = top < bottom ? bottom : top;
    uintptr_t alignment = sizeof(uintptr_t) - 1;
    lo = (lo + alignment) & ~alignment;
    hi &= ~alignment;
    if (hi < lo) {
        return;
    }
    for (uintptr_t address = lo; address <= hi; address += sizeof(uintptr_t)) {
        uintptr_t candidate = 0;
        memcpy(&candidate, (const void *)address, sizeof(candidate));
        mark_candidate(candidate, context);
        if (hi - address < sizeof(uintptr_t)) {
            break;
        }
    }
}

static void gc_collect_inner(uintptr_t stack_top, const void *registers,
                             size_t registers_size) {
    if (!g_enabled || g_collecting || g_objects == NULL) {
        return;
    }
    g_collecting = 1;
    pyrs_str_cache_invalidate();

    PyrsGcRanges ranges = {0};
    for (PyrsGcHeader *header = g_objects; header != NULL;
         header = header->fields.next) {
        header->fields.marked = 0;
        void *payload = (void *)(header + 1);
        ranges_push(&ranges, (uintptr_t)payload, header->fields.size, header);
        PyrsGcOwnedRangeContext owned = {&ranges, header};
        pyrs_gc_visit_owned_ranges((int)header->fields.kind, payload,
                                   header->fields.size, add_owned_range, &owned);
    }
    PyrsGcIndex index;
    index_build(&index, &ranges);

    PyrsGcHeader **work = gc_raw_malloc(g_live_objects * sizeof(*work));
    PyrsGcMarkContext context = {&ranges, &index, work, 0, g_live_objects};

    scan_words(registers, registers_size, &context);
    scan_stack(stack_top, g_stack_bound, &context);
    for (size_t i = 0; i < g_roots_len; i++) {
        scan_words(g_roots[i].start, g_roots[i].size, &context);
    }
    for (PyrsGcRoot *root = g_scoped_roots; root != NULL; root = root->prev) {
        scan_words(root->start, root->size, &context);
    }

    while (context.work_len > 0) {
        PyrsGcHeader *header = context.work[--context.work_len];
        pyrs_gc_trace_object((int)header->fields.kind, (void *)(header + 1),
                             header->fields.size, mark_candidate, mark_slots,
                             &context);
    }

    free(work);
    index_free(&index);
    free(ranges.items);

    PyrsGcHeader **link = &g_objects;
    while (*link != NULL) {
        PyrsGcHeader *header = *link;
        if (header->fields.marked) {
            header->fields.marked = 0;
            link = &header->fields.next;
            continue;
        }
        *link = header->fields.next;
        void *payload = (void *)(header + 1);
        pyrs_gc_destroy_object((int)header->fields.kind, payload,
                               header->fields.size);
        size_t reclaimed = header->fields.size + header->fields.external_size;
        if (reclaimed < header->fields.size) {
            reclaimed = SIZE_MAX;
        }
        g_reclaimed_bytes += reclaimed;
        g_live_bytes -= reclaimed;
        g_live_objects--;
        free(header);
    }

    g_collections++;
    g_allocated_since_gc = 0;
    if (g_threshold_is_explicit) {
        g_threshold = g_base_threshold;
    } else {
        size_t live_target =
            g_live_bytes > SIZE_MAX / 2 ? SIZE_MAX : g_live_bytes * 2;
        g_threshold = live_target > g_base_threshold ? live_target
                                                     : g_base_threshold;
    }
    g_collecting = 0;
}

typedef struct {
    uintptr_t callee_saved[16];
    jmp_buf fallback;
} PyrsGcRegisterRoots;

/* Values live across an allocation call must reside in callee-saved registers
 * or in memory. Capture those registers into plain, directly searchable
 * words; setjmp remains a portable best-effort supplement for other targets. */
static PYRS_NOINLINE void capture_register_roots(PyrsGcRegisterRoots *roots) {
    memset(roots, 0, sizeof(*roots));
#if (defined(__GNUC__) || defined(__clang__)) && defined(__x86_64__)
    __asm__ volatile("movq %%rbx, 0(%0)\n\t"
                     "movq %%rbp, 8(%0)\n\t"
                     "movq %%r12, 16(%0)\n\t"
                     "movq %%r13, 24(%0)\n\t"
                     "movq %%r14, 32(%0)\n\t"
                     "movq %%r15, 40(%0)\n\t"
                     :
                     : "a"(roots->callee_saved)
                     : "memory");
#elif (defined(__GNUC__) || defined(__clang__)) && defined(__aarch64__)
    register uintptr_t *base __asm__("x0") = roots->callee_saved;
    __asm__ volatile("stp x19, x20, [%0, #0]\n\t"
                     "stp x21, x22, [%0, #16]\n\t"
                     "stp x23, x24, [%0, #32]\n\t"
                     "stp x25, x26, [%0, #48]\n\t"
                     "stp x27, x28, [%0, #64]\n\t"
                     "str x29, [%0, #80]\n\t"
                     :
                     : "r"(base)
                     : "memory");
#endif
    (void)setjmp(roots->fallback);
}

PYRS_NOINLINE void pyrs_gc_collect(void) {
    if (!g_enabled || g_collecting || g_objects == NULL ||
        g_stack_bound == 0) {
        return;
    }
    PyrsGcRegisterRoots registers;
    capture_register_roots(&registers);
    gc_collect_inner(capture_stack_pointer(), &registers, sizeof(registers));
}

void *pyrs_gc_alloc(size_t size, int kind) {
    if (!g_initialized) {
        pyrs_gc_init(NULL);
    }
    if (size == 0) {
        size = 1;
    }
    if (g_enabled && !g_collecting && g_objects != NULL &&
        (g_stress || g_allocated_since_gc >= g_threshold ||
         size >= g_threshold -
                     (g_allocated_since_gc < g_threshold
                          ? g_allocated_since_gc
                          : g_threshold))) {
        pyrs_gc_collect();
    }

    if (size > SIZE_MAX - sizeof(PyrsGcHeader)) {
        gc_oom();
    }
    PyrsGcHeader *header = calloc(1, sizeof(*header) + size);
    if (header == NULL) {
        if (g_enabled && !g_collecting) {
            pyrs_gc_collect();
            header = calloc(1, sizeof(*header) + size);
        }
        if (header == NULL) {
            gc_oom();
        }
    }
    header->fields.next = g_objects;
    header->fields.size = size;
    header->fields.external_size = 0;
    header->fields.kind = (unsigned int)kind;
    header->fields.marked = 0;
    g_objects = header;
    g_live_bytes += size;
    g_live_objects++;
    g_allocated_bytes += size;
    g_allocated_since_gc += size;
    return (void *)(header + 1);
}

static PyrsGcHeader *gc_header_for_owner(void *owner) {
    if (owner == NULL) {
        return NULL;
    }
    return ((PyrsGcHeader *)owner) - 1;
}

void pyrs_gc_external_allocated(void *owner, size_t size) {
    if (size == 0) {
        return;
    }
    PyrsGcHeader *header = gc_header_for_owner(owner);
    if (header == NULL) {
        return;
    }
    if (header->fields.external_size > SIZE_MAX - size ||
        g_live_bytes > SIZE_MAX - size ||
        g_allocated_bytes > SIZE_MAX - size ||
        g_allocated_since_gc > SIZE_MAX - size) {
        gc_oom();
    }
    header->fields.external_size += size;
    g_live_bytes += size;
    g_allocated_bytes += size;
    g_allocated_since_gc += size;
}

void pyrs_gc_external_freed(void *owner, size_t size) {
    if (size == 0) {
        return;
    }
    PyrsGcHeader *header = gc_header_for_owner(owner);
    if (header == NULL) {
        return;
    }
    if (size > header->fields.external_size) {
        size = header->fields.external_size;
    }
    header->fields.external_size -= size;
    g_live_bytes -= size;
    g_reclaimed_bytes += size;
}
