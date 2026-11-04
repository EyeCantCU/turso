/* Minimal loadable extension: adds hello_answer() and hello_echo(X). */
#include "sqlite3ext.h"
SQLITE_EXTENSION_INIT1

static void answer(sqlite3_context *ctx, int argc, sqlite3_value **argv) {
  (void)argc; (void)argv;
  sqlite3_result_int(ctx, 42);
}

static void echo(sqlite3_context *ctx, int argc, sqlite3_value **argv) {
  (void)argc;
  sqlite3_result_text(ctx, (const char *)sqlite3_value_text(argv[0]), -1,
                      SQLITE_TRANSIENT);
}

static int install(sqlite3 *db, char **pzErrMsg, const sqlite3_api_routines *pApi) {
  int rc;
  (void)pzErrMsg;
  SQLITE_EXTENSION_INIT2(pApi);
  rc = sqlite3_create_function_v2(db, "hello_answer", 0, SQLITE_UTF8, 0, answer, 0, 0, 0);
  if (rc == SQLITE_OK)
    rc = sqlite3_create_function_v2(db, "hello_echo", 1, SQLITE_UTF8, 0, echo, 0, 0, 0);
  return rc;
}

#ifdef _WIN32
__declspec(dllexport)
#endif
int sqlite3_hello_init(sqlite3 *db, char **pzErrMsg, const sqlite3_api_routines *pApi) {
  return install(db, pzErrMsg, pApi);
}

/* Default entry point name, for load_extension() without zProc. */
#ifndef NO_DEFAULT_ENTRY
#ifdef _WIN32
__declspec(dllexport)
#endif
int sqlite3_extension_init(sqlite3 *db, char **pzErrMsg, const sqlite3_api_routines *pApi) {
  return install(db, pzErrMsg, pApi);
}
#endif

/* Always fails, to test error propagation. */
#ifdef _WIN32
__declspec(dllexport)
#endif
int sqlite3_failing_init(sqlite3 *db, char **pzErrMsg, const sqlite3_api_routines *pApi) {
  SQLITE_EXTENSION_INIT2(pApi);
  (void)db;
  *pzErrMsg = sqlite3_mprintf("deliberate failure");
  return SQLITE_ERROR;
}
