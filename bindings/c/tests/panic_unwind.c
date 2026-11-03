/*
** Checks that a Rust panic inside the library becomes an error code and does
** not abort the host. Needs a library built with `panic = "unwind"`
** (`cargo build --profile release-capi`) that exports the test hook
** `turso_test_panic`; it skips when the hook is missing.
**
**   cc panic_unwind.c -o panic_unwind -ldl
**   ./panic_unwind target/release-capi/libturso_sqlite3.so
*/
#include <dlfcn.h>
#include <stdio.h>

#define SQLITE_INTERNAL 2

int main(int argc, char **argv) {
  void *lib, *db = 0;
  int (*open)(const char *, void **);
  int (*hook)(void *);
  int rc;

  if (argc < 2) {
    fprintf(stderr, "usage: %s <libturso_sqlite3.so>\n", argv[0]);
    return 2;
  }
  lib = dlopen(argv[1], RTLD_NOW);
  if (!lib) {
    fprintf(stderr, "dlopen: %s\n", dlerror());
    return 2;
  }
  open = (int (*)(const char *, void **))dlsym(lib, "sqlite3_open");
  hook = (int (*)(void *))dlsym(lib, "turso_test_panic");
  if (!open) {
    fprintf(stderr, "sqlite3_open not found\n");
    return 2;
  }
  if (!hook) {
    printf("SKIP: turso_test_panic is not exported\n");
    return 0;
  }
  if (open(":memory:", &db) != 0) {
    fprintf(stderr, "open failed\n");
    return 2;
  }
  rc = hook(db); /* aborts the process under panic = "abort" */
  if (rc != SQLITE_INTERNAL) {
    fprintf(stderr, "expected SQLITE_INTERNAL (%d), got %d\n", SQLITE_INTERNAL, rc);
    return 1;
  }
  printf("ok: panic became SQLITE_INTERNAL\n");
  return 0;
}
