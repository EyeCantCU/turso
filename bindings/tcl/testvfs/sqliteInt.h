/*
** The parts of SQLite's internal sqliteInt.h that test_vfs.c (vendored
** unchanged from SQLite 3.54.1) uses, on top of the public sqlite3.h.
*/
#ifndef TURSO_TESTVFS_SQLITEINT_H
#define TURSO_TESTVFS_SQLITEINT_H

#include <assert.h>
#include <ctype.h>
#include <stdlib.h>
#include <stddef.h>
#include <string.h>
#include "sqlite3.h"

typedef unsigned char u8;
typedef unsigned int u32;

#define ArraySize(X) ((int)(sizeof(X) / sizeof(X[0])))
#define sqlite3Isdigit(x) isdigit((unsigned char)(x))
#define sqlite3Isspace(x) isspace((unsigned char)(x))

/* os.c wrappers */
static inline int sqlite3OsOpen(sqlite3_vfs *v, const char *z, sqlite3_file *f,
                                int fl, int *o) {
  return v->xOpen(v, z, f, fl & 0x1087f7f, o);
}
static inline void sqlite3OsClose(sqlite3_file *f) {
  if (f->pMethods) {
    f->pMethods->xClose(f);
    f->pMethods = 0;
  }
}
#define sqlite3OsRead(f, b, n, o) ((f)->pMethods->xRead(f, b, n, o))
#define sqlite3OsWrite(f, b, n, o) ((f)->pMethods->xWrite(f, b, n, o))
#define sqlite3OsTruncate(f, s) ((f)->pMethods->xTruncate(f, s))
#define sqlite3OsSync(f, fl) ((fl) ? (f)->pMethods->xSync(f, fl) : SQLITE_OK)
#define sqlite3OsFileSize(f, p) ((f)->pMethods->xFileSize(f, p))
#define sqlite3OsLock(f, l) ((f)->pMethods->xLock(f, l))
#define sqlite3OsUnlock(f, l) ((f)->pMethods->xUnlock(f, l))
#define sqlite3OsCheckReservedLock(f, p) ((f)->pMethods->xCheckReservedLock(f, p))
static inline int sqlite3OsFileControl(sqlite3_file *f, int op, void *p) {
  if (f->pMethods == 0) return SQLITE_NOTFOUND;
  return f->pMethods->xFileControl(f, op, p);
}
static inline int sqlite3OsSectorSize(sqlite3_file *f) {
  int (*x)(sqlite3_file *) = f->pMethods->xSectorSize;
  return x ? x(f) : 4096;
}
#define sqlite3OsDeviceCharacteristics(f) ((f)->pMethods->xDeviceCharacteristics(f))
static inline int sqlite3OsFetch(sqlite3_file *f, sqlite3_int64 off, int n, void **pp) {
  if (f->pMethods->iVersion > 2 && f->pMethods->xFetch) {
    return f->pMethods->xFetch(f, off, n, pp);
  }
  *pp = 0;
  return SQLITE_OK;
}
static inline int sqlite3OsUnfetch(sqlite3_file *f, sqlite3_int64 off, void *p) {
  if (f->pMethods->iVersion > 2 && f->pMethods->xUnfetch) {
    return f->pMethods->xUnfetch(f, off, p);
  }
  return SQLITE_OK;
}
#define sqlite3OsDelete(v, z, s) ((v)->xDelete(v, z, s))
#define sqlite3OsAccess(v, z, fl, o) ((v)->xAccess(v, z, fl, o))
static inline int sqlite3OsFullPathname(sqlite3_vfs *v, const char *z, int n, char *o) {
  o[0] = 0;
  return v->xFullPathname(v, z, n, o);
}
#define sqlite3OsDlOpen(v, z) ((v)->xDlOpen(v, z))
#define sqlite3OsDlError(v, n, z) ((v)->xDlError(v, n, z))
#define sqlite3OsDlSym(v, p, z) ((v)->xDlSym(v, p, z))
#define sqlite3OsDlClose(v, p) ((v)->xDlClose(v, p))
static inline int sqlite3OsRandomness(sqlite3_vfs *v, int n, char *o) {
  memset(o, 0, (size_t)n);
  return v->xRandomness(v, n, o);
}
#define sqlite3OsSleep(v, n) ((v)->xSleep(v, n))

#endif
