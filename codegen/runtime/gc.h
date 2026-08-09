#ifndef PYRS_GC_H
#define PYRS_GC_H

#include <stddef.h>
#include <stdint.h>

/*
 * Collector-visible allocation kinds.  Payload layouts remain owned by
 * runtime.c; gc.c asks the runtime to trace and destroy a payload by kind.
 * Keeping this boundary narrow lets the allocator change without changing
 * the generated-program ABI.
 */
typedef enum {
    PYRS_GC_STRING = 1,
    PYRS_GC_EXCEPTION,
    PYRS_GC_BIGINT,
    PYRS_GC_LIST,
    PYRS_GC_FILE,
    PYRS_GC_TUPLE,
    PYRS_GC_DICT,
    PYRS_GC_SET,
    PYRS_GC_CELL,
    PYRS_GC_CLOSURE,
    PYRS_GC_GENERATOR,
    PYRS_GC_CLASS,
    PYRS_GC_UNION_BOX,
    PYRS_GC_BOUND_METHOD,
} PyrsGcKind;

typedef struct PyrsGcRoot {
    struct PyrsGcRoot *prev;
    void *start;
    size_t size;
} PyrsGcRoot;

typedef void (*PyrsGcVisitFn)(uintptr_t candidate, void *context);
typedef void (*PyrsGcRangeFn)(void *start, size_t size, void *context);

void pyrs_gc_init(void *stack_anchor);
void pyrs_gc_add_root_range(void *start, size_t size);
void pyrs_gc_add_global(void *start, size_t size);
void pyrs_gc_root_push(PyrsGcRoot *root, void *start, size_t size);
void pyrs_gc_root_pop(PyrsGcRoot *root);
void pyrs_gc_collect(void);
void *pyrs_gc_alloc(size_t size, int kind);
void pyrs_gc_external_allocated(void *owner, size_t size);
void pyrs_gc_external_freed(void *owner, size_t size);

/* Implemented by runtime.c, which owns the concrete payload layouts. */
void pyrs_gc_trace_object(int kind, void *object, size_t size,
                          PyrsGcVisitFn visit, void *context);
void pyrs_gc_visit_owned_ranges(int kind, void *object, size_t size,
                                PyrsGcRangeFn visit, void *context);
void pyrs_gc_destroy_object(int kind, void *object, size_t size);

#endif
