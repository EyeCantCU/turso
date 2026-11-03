/*
** Variadic C entry points that cannot be written in stable Rust (C variadics
** are a nightly-only Rust feature, and on some ABIs — e.g. Apple arm64 —
** variadic arguments are passed differently from named arguments, so the
** entry point must genuinely be variadic).
**
** Each function here decodes its va_list and forwards to a non-variadic
** `turso_*` function exported from the Rust side of the crate. The public
** sqlite3_* symbol itself is a naked-function trampoline in lib.rs that
** tail-jumps here: rustc only exports Rust items from cdylibs, so the
** exported symbol must be a Rust item, and a tail jump preserves the
** variadic call frame intact.
*/
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct sqlite3 sqlite3;

#define SQLITE_ERROR 1

extern int turso_db_config_int(sqlite3 *db, int op, int value, int *p_out);

/*
** sqlite3_db_config() argument shapes, by op:
**   SQLITE_DBCONFIG_MAINDBNAME (1000)       takes (const char*)
**   SQLITE_DBCONFIG_LOOKASIDE  (1001)       takes (void*, int, int)
**   the boolean ops (1002..1017)            take (int, int*)
** The va_list is decoded only for ops known to use the (int, int*) shape;
** any other op — including ops newer than this header — is rejected without
** reading the va_list, since decoding an unknown shape is undefined
** behavior when the caller passed something else.
*/
#define DBCONFIG_BOOL_FIRST 1002 /* SQLITE_DBCONFIG_ENABLE_FKEY */
#define DBCONFIG_BOOL_LAST  1017 /* SQLITE_DBCONFIG_TRUSTED_SCHEMA */

int turso_sqlite3_db_config_va(sqlite3 *db, int op, ...) {
    va_list ap;
    int value;
    int *p_out;

    if (op < DBCONFIG_BOOL_FIRST || op > DBCONFIG_BOOL_LAST) {
        return SQLITE_ERROR;
    }
    va_start(ap, op);
    value = va_arg(ap, int);
    p_out = va_arg(ap, int *);
    va_end(ap);
    return turso_db_config_int(db, op, value, p_out);
}

extern char *turso_printf_va(const char *fmt, void *ap);
extern void sqlite3_free(void *p);

/*
** One-line va_arg pumps. The Rust bridge walks the format string through
** core's specifier grammar (printf_c_arg_plan) and calls back here for
** each argument with the C type that grammar dictates — the only work a
** va_list requires from C.
*/
int turso_va_i32(void *ap) { return va_arg(*(va_list *)ap, int); }
long long turso_va_i64(void *ap) { return va_arg(*(va_list *)ap, long long); }
double turso_va_f64(void *ap) { return va_arg(*(va_list *)ap, double); }
void *turso_va_ptr(void *ap) { return va_arg(*(va_list *)ap, void *); }

char *turso_sqlite3_mprintf_va(const char *fmt, ...) {
    va_list ap;
    char *r;

    va_start(ap, fmt);
    r = turso_printf_va(fmt, &ap);
    va_end(ap);
    return r;
}

char *turso_sqlite3_snprintf_va(int n, char *buf, const char *fmt, ...) {
    va_list ap;
    char *s;
    size_t len;

    if (n <= 0 || !buf) {
        return buf;
    }
    va_start(ap, fmt);
    s = turso_printf_va(fmt, &ap);
    va_end(ap);
    if (!s) {
        buf[0] = 0;
        return buf;
    }
    len = strlen(s);
    if (len >= (size_t)n) {
        len = (size_t)n - 1;
    }
    memcpy(buf, s, len);
    buf[len] = 0;
    sqlite3_free(s);
    return buf;
}

/*
** va_list-taking variants (sqlite3_vmprintf / sqlite3_vsnprintf). The
** exported symbols are tail-jumping trampolines in compat_wrappers.rs, so
** these see the caller's original arguments. The va_list parameter is
** va_copy'd into a local so the pumps above can take its address on every
** ABI (on x86_64 a va_list parameter decays to a pointer).
*/
char *turso_sqlite3_vmprintf_va(const char *fmt, va_list ap) {
    va_list cp;
    char *r;

    va_copy(cp, ap);
    r = turso_printf_va(fmt, &cp);
    va_end(cp);
    return r;
}

char *turso_sqlite3_vsnprintf_va(int n, char *buf, const char *fmt, va_list ap) {
    va_list cp;
    char *s;
    size_t len;

    if (n <= 0 || !buf) {
        return buf;
    }
    va_copy(cp, ap);
    s = turso_printf_va(fmt, &cp);
    va_end(cp);
    if (!s) {
        buf[0] = 0;
        return buf;
    }
    len = strlen(s);
    if (len >= (size_t)n) {
        len = (size_t)n - 1;
    }
    memcpy(buf, s, len);
    buf[len] = 0;
    sqlite3_free(s);
    return buf;
}

/*
** sqlite3_config() argument shapes, by op (SQLite 3.5x):
**   SINGLETHREAD(1) MULTITHREAD(2) SERIALIZED(3)        no arguments
**   MEMSTATUS(9) URI(17) COVERING_INDEX_SCAN(20)
**   STMTJRNL_SPILL(26) SMALL_MALLOC(27) SORTERREF_SIZE(28)   (int)
**   LOOKASIDE(13)                                       (int, int)
**   LOG(16)                                             (void(*)(void*,int,const char*), void*)
**   MMAP_SIZE(22)                                       (sqlite3_int64, sqlite3_int64)
**   PMASZ(25)                                           (unsigned int)
**   MEMDB_MAXSIZE(29)                                   (sqlite3_int64)
** Only these ops are decoded. Any other op is forwarded without touching the
** va_list; the Rust side rejects it.
*/
extern int turso_sqlite3_config_apply(int op, long long i1, long long i2,
                                      void *p1, void *p2);

int turso_sqlite3_config_va(int op, ...) {
    va_list ap;
    long long i1 = 0, i2 = 0;
    void *p1 = 0, *p2 = 0;

    va_start(ap, op);
    switch (op) {
    case 9: case 17: case 20: case 26: case 27: case 28:
        i1 = va_arg(ap, int);
        break;
    case 13:
        i1 = va_arg(ap, int);
        i2 = va_arg(ap, int);
        break;
    case 16:
        p1 = va_arg(ap, void *);
        p2 = va_arg(ap, void *);
        break;
    case 22:
        i1 = va_arg(ap, long long);
        i2 = va_arg(ap, long long);
        break;
    case 25:
        i1 = va_arg(ap, unsigned int);
        break;
    case 29:
        i1 = va_arg(ap, long long);
        break;
    default:
        break;
    }
    va_end(ap);
    return turso_sqlite3_config_apply(op, i1, i2, p1, p2);
}
