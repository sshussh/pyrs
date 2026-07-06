/* pyrs runtime: tiny C support library linked into every compiled program.
 *
 * Printing matches CPython:
 * - floats use the shortest representation that round-trips, and whole
 *   floats keep their ".0" (1.0 prints as "1.0", not "1")
 * - bools print True/False
 * - runtime errors (ZeroDivisionError) print to stderr and exit(1)
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>

void pyrs_print_int(long long v) {
    printf("%lld", v);
}

void pyrs_print_float(double v) {
    if (isnan(v)) {
        fputs("nan", stdout);
        return;
    }
    if (isinf(v)) {
        fputs(v < 0 ? "-inf" : "inf", stdout);
        return;
    }
    char buf[40];
    /* shortest %g form that parses back to exactly the same double */
    for (int prec = 1; prec <= 17; prec++) {
        snprintf(buf, sizeof buf, "%.*g", prec, v);
        if (strtod(buf, NULL) == v) {
            break;
        }
    }
    fputs(buf, stdout);
    if (!strpbrk(buf, ".eE")) {
        fputs(".0", stdout);
    }
}

void pyrs_print_bool(int v) {
    fputs(v ? "True" : "False", stdout);
}

void pyrs_print_str(const char* s) {
    fputs(s, stdout);
}

void pyrs_print_sep(void) {
    fputc(' ', stdout);
}

void pyrs_print_end(void) {
    fputc('\n', stdout);
}

_Noreturn void pyrs_die(const char* msg) {
    fflush(stdout);
    fputs(msg, stderr);
    fputc('\n', stderr);
    exit(1);
}
