/*
** sqlite3_str_appendf() and sqlite3_str_vappendf(): C because they consume
** C variadic arguments / a va_list. The exported symbols are naked
** trampolines in sqlite354.rs that tail-jump here (see varargs.c).
*/
#include <stdarg.h>

typedef struct sqlite3_str sqlite3_str;

extern char *turso_printf_va(const char *fmt, void *ap);
extern void turso_str_append_printf(sqlite3_str *p, char *z);

void turso_sqlite3_str_vappendf_va(sqlite3_str *p, const char *fmt, va_list ap) {
    va_list cp;
    char *z;

    va_copy(cp, ap);
    z = turso_printf_va(fmt, &cp);
    va_end(cp);
    turso_str_append_printf(p, z);
}

void turso_sqlite3_str_appendf_va(sqlite3_str *p, const char *fmt, ...) {
    va_list ap;
    char *z;

    va_start(ap, fmt);
    z = turso_printf_va(fmt, &ap);
    va_end(ap);
    turso_str_append_printf(p, z);
}
