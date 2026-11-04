#ifndef LIMBO_SQLITE3_H
#define LIMBO_SQLITE3_H

#include <stdint.h>

#define SQLITE_OK 0

#define SQLITE_ERROR 1

#define SQLITE_ABORT 4

#define SQLITE_BUSY 5

#define SQLITE_NOMEM 7

#define SQLITE_INTERRUPT 9

#define SQLITE_NOTFOUND 12

#define SQLITE_CANTOPEN 14

#define SQLITE_CONSTRAINT 19

#define SQLITE_MISUSE 21

#define SQLITE_ROW 100

#define SQLITE_DONE 101

#define SQLITE_ABORT_ROLLBACK (SQLITE_ABORT | (2 << 8))




#define SQLITE_CHECKPOINT_PASSIVE 0

#define SQLITE_CHECKPOINT_FULL 1

#define SQLITE_CHECKPOINT_RESTART 2

#define SQLITE_CHECKPOINT_TRUNCATE 3

/* Added in SQLite 3.51 - 3.54 */
#define SQLITE_CHECKPOINT_NOOP -1
#define SQLITE_EMPTY 16
#define SQLITE_DBCONFIG_FP_DIGITS 1023
#define SQLITE_DBSTATUS_TEMPBUF_SPILL 13
#define SQLITE_LIMIT_PARSER_DEPTH 12
#define SQLITE_LIMIT_SCHEMA 13
#define SQLITE_LIMIT_TRIGGER_STEPS 14
#define SQLITE_PREPARE_FROM_DDL 0x20
#define SQLITE_UTF8_ZT 16
#define SQLITE_COPY 0
#define SQLITE_XFER 1
#define SQLITE_FINISH 2

#define SQLITE_OPEN_READONLY         0x00000001
#define SQLITE_OPEN_READWRITE        0x00000002
#define SQLITE_OPEN_CREATE           0x00000004
#define SQLITE_OPEN_URI              0x00000040
#define SQLITE_OPEN_MEMORY           0x00000080
#define SQLITE_OPEN_NOMUTEX          0x00008000
#define SQLITE_OPEN_FULLMUTEX        0x00010000
#define SQLITE_OPEN_SHAREDCACHE      0x00020000
#define SQLITE_OPEN_PRIVATECACHE     0x00040000
#define SQLITE_OPEN_NOFOLLOW         0x01000000

#define SQLITE_UTF8 1

#define SQLITE_INTEGER  1
#define SQLITE_FLOAT    2
#define SQLITE_BLOB     4
#define SQLITE_NULL     5
#define SQLITE_TEXT     3
#define SQLITE3_TEXT     3

typedef void (*sqlite3_destructor_type)(void*);
#define SQLITE_STATIC    ((sqlite3_destructor_type)0)
#define SQLITE_TRANSIENT ((sqlite3_destructor_type)-1)

typedef struct sqlite3 sqlite3;

typedef struct sqlite3_stmt sqlite3_stmt;
typedef long long int sqlite_int64;
typedef unsigned long long int sqlite_uint64;
typedef sqlite_int64 sqlite3_int64;
typedef sqlite_uint64 sqlite3_uint64;
typedef struct sqlite3_context sqlite3_context;
typedef struct sqlite3_value sqlite3_value;
typedef struct sqlite3_blob sqlite3_blob;
typedef struct sqlite3_backup sqlite3_backup;
typedef struct sqlite3_mutex sqlite3_mutex;
typedef const char *sqlite3_filename;


#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/* Added in SQLite 3.51 - 3.54 (and the sqlite3_str API they extend) */
typedef struct sqlite3_str sqlite3_str;
sqlite3_int64 sqlite3_incomplete(const char *sql);
int sqlite3_complete16(const void *sql);
int sqlite3_set_errmsg(sqlite3 *db, int errcode, const char *zErrMsg);
int sqlite3_db_status64(sqlite3*, int, sqlite3_int64*, sqlite3_int64*, int);
sqlite3_str *sqlite3_str_new(sqlite3*);
char *sqlite3_str_finish(sqlite3_str*);
void sqlite3_str_free(sqlite3_str*);
void sqlite3_str_appendf(sqlite3_str*, const char *zFormat, ...);
void sqlite3_str_append(sqlite3_str*, const char *zIn, int N);
void sqlite3_str_appendall(sqlite3_str*, const char *zIn);
void sqlite3_str_appendchar(sqlite3_str*, int N, char C);
void sqlite3_str_reset(sqlite3_str*);
void sqlite3_str_truncate(sqlite3_str*, int N);
int sqlite3_str_errcode(sqlite3_str*);
int sqlite3_str_length(sqlite3_str*);
char *sqlite3_str_value(sqlite3_str*);

int sqlite3_initialize(void);

int sqlite3_shutdown(void);

int sqlite3_open(const char *filename, sqlite3 **ppDb);

int sqlite3_open_v2(const char *filename, sqlite3 **ppDb, int flags, const char *zVfs);

int sqlite3_close(sqlite3*);

int sqlite3_close_v2(sqlite3*);

sqlite3_filename sqlite3_db_filename(sqlite3 *db, const char *zDbName);

#define SQLITE_TRACE_STMT 0x01
#define SQLITE_TRACE_PROFILE 0x02
#define SQLITE_TRACE_ROW 0x04
#define SQLITE_TRACE_CLOSE 0x08

int sqlite3_trace_v2(sqlite3*,
                     unsigned uMask,
                     int(*xCallback)(unsigned, void*, void*, void*),
                     void *pCtx);

void *sqlite3_trace(sqlite3*, void(*xTrace)(void*, const char*), void*);

void *sqlite3_profile(sqlite3*, void(*xProfile)(void*, const char*, sqlite3_uint64), void*);

int sqlite3_collation_needed(sqlite3*, void*, void(*)(void*, sqlite3*, int eTextRep, const char*));

int sqlite3_collation_needed16(sqlite3*,
                               void*,
                               void(*)(void*, sqlite3*, int eTextRep, const void*));

int sqlite3_open16(const void *filename, sqlite3 **ppDb);

void sqlite3_progress_handler(sqlite3*, int, int(*)(void*), void*);

int sqlite3_busy_handler(sqlite3*, int(*)(void*, int), void*);
int sqlite3_busy_timeout(sqlite3*, int ms);
int sqlite3_db_readonly(sqlite3 *db, const char *zDbName);

int sqlite3_set_authorizer(sqlite3*,
                           int (*xAuth)(void*, int, const char*, const char*, const char*, const char*),
                           void *pUserData);

/* Authorizer callback return codes */
#define SQLITE_DENY   1
#define SQLITE_IGNORE 2

/* Authorizer action codes (third parameter meanings per SQLite docs) */
#define SQLITE_CREATE_INDEX          1
#define SQLITE_CREATE_TABLE          2
#define SQLITE_CREATE_TEMP_INDEX     3
#define SQLITE_CREATE_TEMP_TABLE     4
#define SQLITE_CREATE_TEMP_TRIGGER   5
#define SQLITE_CREATE_TEMP_VIEW      6
#define SQLITE_CREATE_TRIGGER        7
#define SQLITE_CREATE_VIEW           8
#define SQLITE_DELETE                9
#define SQLITE_DROP_INDEX           10
#define SQLITE_DROP_TABLE           11
#define SQLITE_DROP_TEMP_INDEX      12
#define SQLITE_DROP_TEMP_TABLE      13
#define SQLITE_DROP_TEMP_TRIGGER    14
#define SQLITE_DROP_TEMP_VIEW       15
#define SQLITE_DROP_TRIGGER         16
#define SQLITE_DROP_VIEW            17
#define SQLITE_INSERT               18
#define SQLITE_PRAGMA               19
#define SQLITE_READ                 20
#define SQLITE_SELECT               21
#define SQLITE_TRANSACTION          22
#define SQLITE_UPDATE               23
#define SQLITE_ATTACH               24
#define SQLITE_DETACH               25
#define SQLITE_ALTER_TABLE          26
#define SQLITE_REINDEX              27
#define SQLITE_ANALYZE              28
#define SQLITE_CREATE_VTABLE        29
#define SQLITE_DROP_VTABLE          30
#define SQLITE_FUNCTION             31
#define SQLITE_SAVEPOINT            32
#define SQLITE_COPY                  0
#define SQLITE_RECURSIVE            33

sqlite3 *sqlite3_context_db_handle(sqlite3_context*);

int sqlite3_prepare_v2(sqlite3 *db,
                       const char *zSql,
                       int nByte,
                       sqlite3_stmt **ppStmt,
                       const char **pzTail);

int sqlite3_prepare_v3(sqlite3 *db,
                       const char *zSql,
                       int nByte,
                       unsigned int prepFlags,
                       sqlite3_stmt **ppStmt,
                       const char **pzTail);

#define SQLITE_PREPARE_PERSISTENT 0x01
#define SQLITE_PREPARE_NORMALIZE  0x02
#define SQLITE_PREPARE_NO_VTAB    0x04

int sqlite3_finalize(sqlite3_stmt *pStmt);

int sqlite3_step(sqlite3_stmt*);

int sqlite3_exec(sqlite3*,
                 const char *sql,
                 int (*callback)(void*, int, char**, char**),
                 void *,
                 char **errmsg);

int sqlite3_reset(sqlite3_stmt *pStmt);

int sqlite3_clear_bindings(sqlite3_stmt*);

int sqlite3_changes(sqlite3*);

sqlite3_int64 sqlite3_changes64(sqlite3*);

int sqlite3_stmt_readonly(sqlite3_stmt *pStmt);

int sqlite3_stmt_busy(sqlite3_stmt*);

int sqlite3_stmt_status(sqlite3_stmt*, int op, int resetFlg);

#define SQLITE_STMTSTATUS_FULLSCAN_STEP 1
#define SQLITE_STMTSTATUS_SORT 2
#define SQLITE_STMTSTATUS_AUTOINDEX 3
#define SQLITE_STMTSTATUS_VM_STEP 4
#define SQLITE_STMTSTATUS_REPREPARE 5
#define SQLITE_STMTSTATUS_RUN 6
#define SQLITE_STMTSTATUS_FILTER_MISS 7
#define SQLITE_STMTSTATUS_FILTER_HIT 8
#define SQLITE_STMTSTATUS_MEMUSED 99

#define LIBSQL_STMTSTATUS_BASE 1024
#define LIBSQL_STMTSTATUS_ROWS_READ (LIBSQL_STMTSTATUS_BASE + 1)
#define LIBSQL_STMTSTATUS_ROWS_WRITTEN (LIBSQL_STMTSTATUS_BASE + 2)

sqlite3_stmt *sqlite3_next_stmt(sqlite3 *pDb, sqlite3_stmt *pStmt);

unsigned char *sqlite3_serialize(sqlite3 *db,
                                 const char *zSchema,
                                 sqlite3_int64 *piSize,
                                 unsigned int mFlags);

int sqlite3_deserialize(sqlite3 *db,
                        const char *zSchema,
                        unsigned char *pData,
                        sqlite3_int64 szDb,
                        sqlite3_int64 szBuf,
                        unsigned mFlags);

int sqlite3_get_autocommit(sqlite3*);

int sqlite3_total_changes(sqlite3*);

sqlite3_int64 sqlite3_total_changes64(sqlite3*);

sqlite3_int64 sqlite3_last_insert_rowid(sqlite3*);

void sqlite3_interrupt(sqlite3*);

int sqlite3_db_config(sqlite3*, int op, ...);

#define SQLITE_DBCONFIG_MAINDBNAME            1000
#define SQLITE_DBCONFIG_LOOKASIDE             1001
#define SQLITE_DBCONFIG_ENABLE_FKEY           1002
#define SQLITE_DBCONFIG_ENABLE_TRIGGER        1003
#define SQLITE_DBCONFIG_ENABLE_FTS3_TOKENIZER 1004
#define SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION 1005
#define SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE      1006
#define SQLITE_DBCONFIG_ENABLE_QPSG           1007
#define SQLITE_DBCONFIG_TRIGGER_EQP           1008
#define SQLITE_DBCONFIG_RESET_DATABASE        1009
#define SQLITE_DBCONFIG_DEFENSIVE             1010
#define SQLITE_DBCONFIG_WRITABLE_SCHEMA       1011
#define SQLITE_DBCONFIG_LEGACY_ALTER_TABLE    1012
#define SQLITE_DBCONFIG_DQS_DML               1013
#define SQLITE_DBCONFIG_DQS_DDL               1014
#define SQLITE_DBCONFIG_ENABLE_VIEW           1015
#define SQLITE_DBCONFIG_LEGACY_FILE_FORMAT    1016
#define SQLITE_DBCONFIG_TRUSTED_SCHEMA        1017

int sqlite3_extended_result_codes(sqlite3*, int onoff);

sqlite3 *sqlite3_db_handle(sqlite3_stmt*);

int sqlite3_sleep(int);

#define SQLITE_LIMIT_LENGTH 0
#define SQLITE_LIMIT_SQL_LENGTH 1
#define SQLITE_LIMIT_COLUMN 2
#define SQLITE_LIMIT_EXPR_DEPTH 3
#define SQLITE_LIMIT_COMPOUND_SELECT 4
#define SQLITE_LIMIT_VDBE_OP 5
#define SQLITE_LIMIT_FUNCTION_ARG 6
#define SQLITE_LIMIT_ATTACHED 7
#define SQLITE_LIMIT_LIKE_PATTERN_LENGTH 8
#define SQLITE_LIMIT_VARIABLE_NUMBER 9
#define SQLITE_LIMIT_TRIGGER_DEPTH 10
#define SQLITE_LIMIT_WORKER_THREADS 11

int sqlite3_limit(sqlite3*, int id, int newVal);

void *sqlite3_malloc(int);
void *sqlite3_malloc64(sqlite3_uint64);

void sqlite3_free(void*);

char *sqlite3_mprintf(const char*, ...);

char *sqlite3_snprintf(int, char*, const char*, ...);

int sqlite3_errcode(sqlite3 *db);

const char *sqlite3_errstr(int);

void *sqlite3_user_data(sqlite3_context*);

sqlite3_backup *sqlite3_backup_init(sqlite3 *pDest,
                                    const char *zDestName,
                                    sqlite3 *pSource,
                                    const char *zSourceName);

int sqlite3_backup_step(sqlite3_backup *p, int nPage);

int sqlite3_backup_remaining(sqlite3_backup *p);

int sqlite3_backup_pagecount(sqlite3_backup *p);

int sqlite3_backup_finish(sqlite3_backup *p);

const char *sqlite3_sql(sqlite3_stmt *pStmt);

char *sqlite3_expanded_sql(sqlite3_stmt *pStmt);

int sqlite3_data_count(sqlite3_stmt *pStmt);

int sqlite3_bind_parameter_count(sqlite3_stmt*);

const char *sqlite3_bind_parameter_name(sqlite3_stmt*, int);

int sqlite3_bind_parameter_index(sqlite3_stmt*, const char *zName);

int sqlite3_bind_null(sqlite3_stmt*, int);

int sqlite3_bind_int(sqlite3_stmt*, int, int);
int sqlite3_bind_int64(sqlite3_stmt*, int, sqlite3_int64);

int sqlite3_bind_double(sqlite3_stmt*, int, double);

int sqlite3_bind_text(sqlite3_stmt*, int, const char*, int, void(*)(void*));

int sqlite3_bind_blob(sqlite3_stmt*, int, const void*, int n, void(*)(void*));

int sqlite3_column_type(sqlite3_stmt*, int iCol);

int sqlite3_column_count(sqlite3_stmt *pStmt);

const char *sqlite3_column_decltype(sqlite3_stmt*, int);

const char *sqlite3_column_name(sqlite3_stmt*, int N);

const char *sqlite3_column_table_name(sqlite3_stmt*, int);

int sqlite3_column_int(sqlite3_stmt*, int iCol);
sqlite3_int64 sqlite3_column_int64(sqlite3_stmt*, int iCol);

double sqlite3_column_double(sqlite3_stmt*, int iCol);

const void *sqlite3_column_blob(sqlite3_stmt*, int iCol);

int sqlite3_column_bytes(sqlite3_stmt*, int iCol);

sqlite3_value *sqlite3_column_value(sqlite3_stmt*, int iCol);

int sqlite3_value_type(sqlite3_value*);

sqlite3_int64 sqlite3_value_int64(sqlite3_value*);

int sqlite3_value_int(sqlite3_value*);

double sqlite3_value_double(sqlite3_value*);

const unsigned char *sqlite3_value_text(sqlite3_value*);

const void *sqlite3_value_blob(sqlite3_value*);
int sqlite3_value_encoding(sqlite3_value*);

int sqlite3_value_bytes(sqlite3_value*);

sqlite3_value *sqlite3_value_dup(const sqlite3_value*);

void sqlite3_value_free(sqlite3_value*);

const unsigned char *sqlite3_column_text(sqlite3_stmt*, int iCol);


int sqlite3_get_table(sqlite3 *db,
                      const char *zSql,
                      char ***pazResult,
                      int *pnRow,
                      int *pnColumn,
                      char **pzErrmsg);

void sqlite3_free_table(char **result);

void sqlite3_result_null(sqlite3_context*);

void sqlite3_result_int64(sqlite3_context*, sqlite3_int64);

void sqlite3_result_int(sqlite3_context*, int);

void sqlite3_result_double(sqlite3_context*, double);

void sqlite3_result_text(sqlite3_context*, const char*, int, void(*)(void*));

void sqlite3_result_blob(sqlite3_context*, const void*, int, void(*)(void*));

void sqlite3_result_error_nomem(sqlite3_context*);

void sqlite3_result_error_toobig(sqlite3_context*);

void sqlite3_result_error(sqlite3_context*, const char*, int);

void *sqlite3_aggregate_context(sqlite3_context*, int nBytes);

int sqlite3_blob_open(sqlite3*,
                      const char *zDb,
                      const char *zTable,
                      const char *zColumn,
                      sqlite3_int64 iRow,
                      int flags,
                      sqlite3_blob **ppBlob);

int sqlite3_blob_read(sqlite3_blob *, void *Z, int N, int iOffset);

int sqlite3_blob_write(sqlite3_blob *, const void *z, int n, int iOffset);

int sqlite3_blob_reopen(sqlite3_blob *, sqlite3_int64);

int sqlite3_blob_bytes(sqlite3_blob *);

int sqlite3_blob_close(sqlite3_blob *);

int sqlite3_stricmp(const char *, const char *);

int sqlite3_strnicmp(const char *, const char *, int);

int sqlite3_create_collation_v2(sqlite3*,
                                const char *zName,
                                int eTextRep,
                                void *pArg,
                                int(*xCompare)(void*, int, const void*, int, const void*),
                                void(*xDestroy)(void*));

int sqlite3_create_function_v2(sqlite3 *db,
                               const char *zFunctionName,
                               int nArg,
                               int eTextRep,
                               void *pApp,
                               void (*xFunc)(sqlite3_context*, int, sqlite3_value**),
                               void (*xStep)(sqlite3_context*, int, sqlite3_value**),
                               void (*xFinal)(sqlite3_context*),
                               void(*xDestroy)(void*));

int sqlite3_create_window_function(sqlite3 *db,
                                   const char *zFunctionName,
                                   int nArg,
                                   int eTextRep,
                                   void *pApp,
                                   void (*xStep)(sqlite3_context*, int, sqlite3_value**),
                                   void (*xFinal)(sqlite3_context*),
                                   void (*xValue)(sqlite3_context*),
                                   void (*xInverse)(sqlite3_context*, int, sqlite3_value**),
                                   void(*xDestroy)(void*));

const char *sqlite3_errmsg(sqlite3*);

int sqlite3_extended_errcode(sqlite3 *db);

int sqlite3_complete(const char *sql);

int sqlite3_threadsafe(void);

const char *sqlite3_libversion(void);

int sqlite3_libversion_number(void);

int sqlite3_wal_checkpoint(sqlite3 *db, const char *zDb);

int sqlite3_wal_checkpoint_v2(sqlite3 *db, const char *zDb, int eMode, int *pnLog, int *pnCkpt);

/**
 * Get the number of frames in the WAL.
 *
 * The `libsql_wal_frame_count` function returns the number of frames
 * in the WAL in the `p_frame_count` parameter.
 *
 * # Returns
 *
 * - `SQLITE_OK` if the number of frames in the WAL file is
 *   successfully returned.
 * - `SQLITE_MISUSE` if the `db` is `NULL`.
 * - `SQLITE_ERROR` if an error occurs while getting the number of frames
 *   in the WAL file.
 *
 * # Safety
 *
 * - The `db` must be a valid pointer to a `sqlite3` database connection.
 * - The `p_frame_count` must be a valid pointer to a `u32` that will store
 *   the number of frames in the WAL file.
 */
int libsql_wal_frame_count(sqlite3 *db, uint32_t *p_frame_count);

/**
 * Return meta information about a specific column of a database table.
 * 
 * @param db Connection handle
 * @param zDbName Database name or NULL for main database
 * @param zTableName Table name
 * @param zColumnName Column name
 * @param pzDataType OUTPUT: Declared data type
 * @param pzCollSeq OUTPUT: Collation sequence name
 * @param pNotNull OUTPUT: True if NOT NULL constraint exists
 * @param pPrimaryKey OUTPUT: True if column part of PK
 * @param pAutoinc OUTPUT: True if column is auto-increment
 * @return SQLITE_OK on success, SQLITE_ERROR on error
 */
int sqlite3_table_column_metadata(sqlite3 *db,
                                  const char *zDbName,
                                  const char *zTableName,
                                  const char *zColumnName,
                                  char const **pzDataType,
                                  char const **pzCollSeq,
                                  int *pNotNull,
                                  int *pPrimaryKey,
                                  int *pAutoinc);

/* ---- src/wrappers.rs ---- */
#include <stdarg.h>
extern const char sqlite3_version[];
int sqlite3_create_function(sqlite3 *db,
                            const char *zFunctionName,
                            int nArg,
                            int eTextRep,
                            void *pApp,
                            void (*xFunc)(sqlite3_context*, int, sqlite3_value**),
                            void (*xStep)(sqlite3_context*, int, sqlite3_value**),
                            void (*xFinal)(sqlite3_context*));
int sqlite3_create_collation(sqlite3*,
                             const char *zName,
                             int eTextRep,
                             void *pArg,
                             int(*xCompare)(void*, int, const void*, int, const void*));
int sqlite3_prepare(sqlite3 *db,
                    const char *zSql,
                    int nByte,
                    sqlite3_stmt **ppStmt,
                    const char **pzTail);
char *sqlite3_vmprintf(const char*, va_list);
char *sqlite3_vsnprintf(int, char*, const char*, va_list);
int sqlite3_bind_text64(sqlite3_stmt*,
                        int,
                        const char*,
                        sqlite3_uint64,
                        void(*)(void*),
                        unsigned char encoding);
int sqlite3_bind_blob64(sqlite3_stmt*, int, const void*, sqlite3_uint64, void(*)(void*));
void sqlite3_result_text64(sqlite3_context*,
                           const char *z,
                           sqlite3_uint64 n,
                           void(*)(void*),
                           unsigned char encoding);
void *sqlite3_realloc(void*, int);
void *sqlite3_realloc64(void*, sqlite3_uint64);
const char *sqlite3_sourceid(void);
int sqlite3_keyword_count(void);
int sqlite3_keyword_name(int, const char**, int*);
int sqlite3_keyword_check(const char*, int);
void sqlite3_result_value(sqlite3_context*, sqlite3_value*);
void sqlite3_result_zeroblob(sqlite3_context*, int n);
int sqlite3_result_zeroblob64(sqlite3_context*, sqlite3_uint64 n);
int sqlite3_value_numeric_type(sqlite3_value*);
const char *sqlite3_column_origin_name(sqlite3_stmt*, int);
int sqlite3_db_release_memory(sqlite3*);
int sqlite3_system_errno(sqlite3*);
int sqlite3_stmt_isexplain(sqlite3_stmt *pStmt);
int sqlite3_stmt_explain(sqlite3_stmt *pStmt, int eMode);
int sqlite3_error_offset(sqlite3 *db);
int sqlite3_enable_shared_cache(int);
/* ---- end src/wrappers.rs ---- */
/* ---- Chainguard compat (stubs group): UTF-16 API ---- */
#define SQLITE_SERIALIZE_NOCOPY 0x001
#define SQLITE_DESERIALIZE_FREEONCLOSE 1
#define SQLITE_DESERIALIZE_RESIZEABLE  2
#define SQLITE_DESERIALIZE_READONLY    4
int sqlite3_prepare16_v2(sqlite3 *db,
                         const void *zSql,
                         int nByte,
                         sqlite3_stmt **ppStmt,
                         const void **pzTail);
const void *sqlite3_errmsg16(sqlite3*);
int sqlite3_bind_text16(sqlite3_stmt*, int, const void*, int, void(*)(void*));
const void *sqlite3_column_text16(sqlite3_stmt*, int iCol);
int sqlite3_column_bytes16(sqlite3_stmt*, int iCol);
const void *sqlite3_column_name16(sqlite3_stmt*, int N);
const void *sqlite3_column_decltype16(sqlite3_stmt*, int);
const void *sqlite3_column_table_name16(sqlite3_stmt*, int);

/*
** Data-change and transaction hooks (hooks.rs).
*/
void *sqlite3_commit_hook(sqlite3*, int(*)(void*), void*);
void *sqlite3_rollback_hook(sqlite3*, void(*)(void *), void*);
void *sqlite3_update_hook(sqlite3*,
                          void(*)(void *, int, char const *, char const *, sqlite3_int64),
                          void*);
void *sqlite3_wal_hook(sqlite3*, int(*)(void *, sqlite3*, const char*, int), void*);
void *sqlite3_preupdate_hook(sqlite3 *db,
                             void(*xPreUpdate)(void *pCtx, sqlite3 *db, int op, char const *zDb, char const *zName, sqlite3_int64 iKey1, sqlite3_int64 iKey2),
                             void*);
int sqlite3_preupdate_old(sqlite3 *, int, sqlite3_value **);
int sqlite3_preupdate_new(sqlite3 *, int, sqlite3_value **);
int sqlite3_preupdate_count(sqlite3 *);
int sqlite3_preupdate_depth(sqlite3 *);
int sqlite3_preupdate_blobwrite(sqlite3 *);
int sqlite3_wal_autocheckpoint(sqlite3 *db, int N);

/*
** ===== vtab group: virtual table C API =====
** Layouts match SQLite's sqlite3.h. As elsewhere in this header,
** sqlite3_context* and sqlite3_value* are spelled void*. xFindFunction,
** xShadowName and xIntegrity may be supplied but are never called.
*/
typedef struct sqlite3_vtab sqlite3_vtab;
typedef struct sqlite3_index_info sqlite3_index_info;
typedef struct sqlite3_vtab_cursor sqlite3_vtab_cursor;
typedef struct sqlite3_module sqlite3_module;

struct sqlite3_module {
  int iVersion;
  int (*xCreate)(sqlite3*, void *pAux, int argc, const char *const *argv,
                 sqlite3_vtab **ppVTab, char **pzErr);
  int (*xConnect)(sqlite3*, void *pAux, int argc, const char *const *argv,
                  sqlite3_vtab **ppVTab, char **pzErr);
  int (*xBestIndex)(sqlite3_vtab *pVTab, sqlite3_index_info*);
  int (*xDisconnect)(sqlite3_vtab *pVTab);
  int (*xDestroy)(sqlite3_vtab *pVTab);
  int (*xOpen)(sqlite3_vtab *pVTab, sqlite3_vtab_cursor **ppCursor);
  int (*xClose)(sqlite3_vtab_cursor*);
  int (*xFilter)(sqlite3_vtab_cursor*, int idxNum, const char *idxStr,
                 int argc, void **argv);
  int (*xNext)(sqlite3_vtab_cursor*);
  int (*xEof)(sqlite3_vtab_cursor*);
  int (*xColumn)(sqlite3_vtab_cursor*, void *ctx, int);
  int (*xRowid)(sqlite3_vtab_cursor*, sqlite3_int64 *pRowid);
  int (*xUpdate)(sqlite3_vtab *, int, void **, sqlite3_int64 *);
  int (*xBegin)(sqlite3_vtab *pVTab);
  int (*xSync)(sqlite3_vtab *pVTab);
  int (*xCommit)(sqlite3_vtab *pVTab);
  int (*xRollback)(sqlite3_vtab *pVTab);
  int (*xFindFunction)(sqlite3_vtab *pVtab, int nArg, const char *zName,
                       void (**pxFunc)(void*, int, void**), void **ppArg);
  int (*xRename)(sqlite3_vtab *pVtab, const char *zNew);
  int (*xSavepoint)(sqlite3_vtab *pVTab, int);
  int (*xRelease)(sqlite3_vtab *pVTab, int);
  int (*xRollbackTo)(sqlite3_vtab *pVTab, int);
  int (*xShadowName)(const char*);
  int (*xIntegrity)(sqlite3_vtab *pVTab, const char *zSchema,
                    const char *zTabName, int mFlags, char **pzErr);
};

struct sqlite3_index_info {
  int nConstraint;
  struct sqlite3_index_constraint {
     int iColumn;
     unsigned char op;
     unsigned char usable;
     int iTermOffset;
  } *aConstraint;
  int nOrderBy;
  struct sqlite3_index_orderby {
     int iColumn;
     unsigned char desc;
  } *aOrderBy;
  struct sqlite3_index_constraint_usage {
    int argvIndex;
    unsigned char omit;
  } *aConstraintUsage;
  int idxNum;
  char *idxStr;
  int needToFreeIdxStr;
  int orderByConsumed;
  double estimatedCost;
  sqlite3_int64 estimatedRows;
  int idxFlags;
  uint64_t colUsed;
};

#define SQLITE_INDEX_SCAN_UNIQUE      0x00000001
#define SQLITE_INDEX_SCAN_HEX         0x00000002
#define SQLITE_INDEX_CONSTRAINT_EQ          2
#define SQLITE_INDEX_CONSTRAINT_GT          4
#define SQLITE_INDEX_CONSTRAINT_LE          8
#define SQLITE_INDEX_CONSTRAINT_LT         16
#define SQLITE_INDEX_CONSTRAINT_GE         32
#define SQLITE_INDEX_CONSTRAINT_MATCH      64
#define SQLITE_INDEX_CONSTRAINT_LIKE       65
#define SQLITE_INDEX_CONSTRAINT_GLOB       66
#define SQLITE_INDEX_CONSTRAINT_REGEXP     67
#define SQLITE_INDEX_CONSTRAINT_NE         68
#define SQLITE_INDEX_CONSTRAINT_ISNOT      69
#define SQLITE_INDEX_CONSTRAINT_ISNOTNULL  70
#define SQLITE_INDEX_CONSTRAINT_ISNULL     71
#define SQLITE_INDEX_CONSTRAINT_IS         72
#define SQLITE_INDEX_CONSTRAINT_LIMIT      73
#define SQLITE_INDEX_CONSTRAINT_OFFSET     74
#define SQLITE_INDEX_CONSTRAINT_FUNCTION  150

struct sqlite3_vtab {
  const sqlite3_module *pModule;
  int nRef;
  char *zErrMsg;
};

struct sqlite3_vtab_cursor {
  sqlite3_vtab *pVtab;
};

#define SQLITE_VTAB_CONSTRAINT_SUPPORT 1
#define SQLITE_VTAB_INNOCUOUS          2
#define SQLITE_VTAB_DIRECTONLY         3
#define SQLITE_VTAB_USES_ALL_SCHEMAS   4

int sqlite3_create_module(sqlite3 *db,
                          const char *zName,
                          const sqlite3_module *p,
                          void *pClientData);
int sqlite3_create_module_v2(sqlite3 *db,
                             const char *zName,
                             const sqlite3_module *p,
                             void *pClientData,
                             void(*xDestroy)(void*));
int sqlite3_declare_vtab(sqlite3*, const char *zSQL);
int sqlite3_vtab_config(sqlite3*, int op, ...);
int sqlite3_vtab_on_conflict(sqlite3 *);
int sqlite3_vtab_nochange(sqlite3_context*);
const char *sqlite3_vtab_collation(sqlite3_index_info*, int);
int sqlite3_vtab_distinct(sqlite3_index_info*);
int sqlite3_vtab_in(sqlite3_index_info*, int iCons, int bHandle);
int sqlite3_vtab_in_first(sqlite3_value *pVal, sqlite3_value **ppOut);
int sqlite3_vtab_in_next(sqlite3_value *pVal, sqlite3_value **ppOut);
int sqlite3_vtab_rhs_value(sqlite3_index_info*, int, sqlite3_value **ppVal);
/* ===== end vtab group ===== */
/*
** Session extension (SQLite's sqlite3session.h; always built in).
*/
#ifndef __SQLITESESSION_H_
#define __SQLITESESSION_H_ 1

typedef struct sqlite3_session sqlite3_session;
typedef struct sqlite3_changeset_iter sqlite3_changeset_iter;
typedef struct sqlite3_changegroup sqlite3_changegroup;
typedef struct sqlite3_rebaser sqlite3_rebaser;

#define SQLITE_SESSION_OBJCONFIG_SIZE  1
#define SQLITE_SESSION_OBJCONFIG_ROWID 2
#define SQLITE_CHANGESETSTART_INVERT        0x0002
#define SQLITE_CHANGESETAPPLY_NOSAVEPOINT   0x0001
#define SQLITE_CHANGESETAPPLY_INVERT        0x0002
#define SQLITE_CHANGESETAPPLY_IGNORENOOP    0x0004
#define SQLITE_CHANGESETAPPLY_FKNOACTION    0x0008
#define SQLITE_CHANGESETAPPLY_NOUPDATELOOP  0x0010
#define SQLITE_CHANGESET_DATA        1
#define SQLITE_CHANGESET_NOTFOUND    2
#define SQLITE_CHANGESET_CONFLICT    3
#define SQLITE_CHANGESET_CONSTRAINT  4
#define SQLITE_CHANGESET_FOREIGN_KEY 5
#define SQLITE_CHANGESET_OMIT       0
#define SQLITE_CHANGESET_REPLACE    1
#define SQLITE_CHANGESET_ABORT      2
#define SQLITE_SESSION_CONFIG_STRMSIZE 1
#define SQLITE_CHANGEGROUP_CONFIG_PATCHSET 1

int sqlite3session_create(sqlite3 *db, const char *zDb, sqlite3_session **ppSession);
void sqlite3session_delete(sqlite3_session *pSession);
int sqlite3session_object_config(sqlite3_session*, int op, void *pArg);
int sqlite3session_enable(sqlite3_session *pSession, int bEnable);
int sqlite3session_indirect(sqlite3_session *pSession, int bIndirect);
int sqlite3session_attach(sqlite3_session *pSession, const char *zTab);
void sqlite3session_table_filter(
    sqlite3_session *pSession,
    int(*xFilter)(void *pCtx, const char *zTab),
    void *pCtx
);
int sqlite3session_changeset(sqlite3_session *pSession, int *pnChangeset, void **ppChangeset);
sqlite3_int64 sqlite3session_changeset_size(sqlite3_session *pSession);
int sqlite3session_diff(sqlite3_session *pSession, const char *zFromDb, const char *zTbl, char **pzErrMsg);
int sqlite3session_patchset(sqlite3_session *pSession, int *pnPatchset, void **ppPatchset);
int sqlite3session_isempty(sqlite3_session *pSession);
sqlite3_int64 sqlite3session_memory_used(sqlite3_session *pSession);

int sqlite3changeset_start(sqlite3_changeset_iter **pp, int nChangeset, void *pChangeset);
int sqlite3changeset_start_v2(sqlite3_changeset_iter **pp, int nChangeset, void *pChangeset, int flags);
int sqlite3changeset_next(sqlite3_changeset_iter *pIter);
int sqlite3changeset_op(sqlite3_changeset_iter *pIter, const char **pzTab, int *pnCol, int *pOp, int *pbIndirect);
int sqlite3changeset_pk(sqlite3_changeset_iter *pIter, unsigned char **pabPK, int *pnCol);
int sqlite3changeset_old(sqlite3_changeset_iter *pIter, int iVal, sqlite3_value **ppValue);
int sqlite3changeset_new(sqlite3_changeset_iter *pIter, int iVal, sqlite3_value **ppValue);
int sqlite3changeset_conflict(sqlite3_changeset_iter *pIter, int iVal, sqlite3_value **ppValue);
int sqlite3changeset_fk_conflicts(sqlite3_changeset_iter *pIter, int *pnOut);
int sqlite3changeset_finalize(sqlite3_changeset_iter *pIter);
int sqlite3changeset_invert(int nIn, const void *pIn, int *pnOut, void **ppOut);
int sqlite3changeset_concat(int nA, void *pA, int nB, void *pB, int *pnOut, void **ppOut);

int sqlite3changegroup_new(sqlite3_changegroup **pp);
int sqlite3changegroup_schema(sqlite3_changegroup*, sqlite3*, const char *zDb);
int sqlite3changegroup_add(sqlite3_changegroup*, int nData, void *pData);
int sqlite3changegroup_add_change(sqlite3_changegroup*, sqlite3_changeset_iter*);
int sqlite3changegroup_output(sqlite3_changegroup*, int *pnData, void **ppData);
void sqlite3changegroup_delete(sqlite3_changegroup*);
int sqlite3changegroup_config(sqlite3_changegroup*, int, void *pArg);
int sqlite3changegroup_change_begin(sqlite3_changegroup*, int eOp, const char *zTab, int bIndirect, char **pzErr);
int sqlite3changegroup_change_int64(sqlite3_changegroup*, int bNew, int iCol, sqlite3_int64 iVal);
int sqlite3changegroup_change_null(sqlite3_changegroup*, int bNew, int iCol);
int sqlite3changegroup_change_double(sqlite3_changegroup*, int bNew, int iCol, double fVal);
int sqlite3changegroup_change_text(sqlite3_changegroup*, int bNew, int iCol, const char *pVal, int nVal);
int sqlite3changegroup_change_blob(sqlite3_changegroup*, int bNew, int iCol, const void *pVal, int nVal);
int sqlite3changegroup_change_finish(sqlite3_changegroup*, int bDiscard, char **pzErr);

int sqlite3changeset_apply(
    sqlite3 *db, int nChangeset, void *pChangeset,
    int(*xFilter)(void *pCtx, const char *zTab),
    int(*xConflict)(void *pCtx, int eConflict, sqlite3_changeset_iter *p),
    void *pCtx
);
int sqlite3changeset_apply_v2(
    sqlite3 *db, int nChangeset, void *pChangeset,
    int(*xFilter)(void *pCtx, const char *zTab),
    int(*xConflict)(void *pCtx, int eConflict, sqlite3_changeset_iter *p),
    void *pCtx, void **ppRebase, int *pnRebase, int flags
);
int sqlite3changeset_apply_v3(
    sqlite3 *db, int nChangeset, void *pChangeset,
    int(*xFilter)(void *pCtx, sqlite3_changeset_iter *p),
    int(*xConflict)(void *pCtx, int eConflict, sqlite3_changeset_iter *p),
    void *pCtx, void **ppRebase, int *pnRebase, int flags
);

int sqlite3rebaser_create(sqlite3_rebaser **ppNew);
int sqlite3rebaser_configure(sqlite3_rebaser*, int nRebase, const void *pRebase);
int sqlite3rebaser_rebase(sqlite3_rebaser*, int nIn, const void *pIn, int *pnOut, void **ppOut);
void sqlite3rebaser_delete(sqlite3_rebaser *p);

int sqlite3changeset_apply_strm(
    sqlite3 *db,
    int (*xInput)(void *pIn, void *pData, int *pnData), void *pIn,
    int(*xFilter)(void *pCtx, const char *zTab),
    int(*xConflict)(void *pCtx, int eConflict, sqlite3_changeset_iter *p),
    void *pCtx
);
int sqlite3changeset_apply_v2_strm(
    sqlite3 *db,
    int (*xInput)(void *pIn, void *pData, int *pnData), void *pIn,
    int(*xFilter)(void *pCtx, const char *zTab),
    int(*xConflict)(void *pCtx, int eConflict, sqlite3_changeset_iter *p),
    void *pCtx, void **ppRebase, int *pnRebase, int flags
);
int sqlite3changeset_apply_v3_strm(
    sqlite3 *db,
    int (*xInput)(void *pIn, void *pData, int *pnData), void *pIn,
    int(*xFilter)(void *pCtx, sqlite3_changeset_iter *p),
    int(*xConflict)(void *pCtx, int eConflict, sqlite3_changeset_iter *p),
    void *pCtx, void **ppRebase, int *pnRebase, int flags
);
int sqlite3changeset_concat_strm(
    int (*xInputA)(void *pIn, void *pData, int *pnData), void *pInA,
    int (*xInputB)(void *pIn, void *pData, int *pnData), void *pInB,
    int (*xOutput)(void *pOut, const void *pData, int nData), void *pOut
);
int sqlite3changeset_invert_strm(
    int (*xInput)(void *pIn, void *pData, int *pnData), void *pIn,
    int (*xOutput)(void *pOut, const void *pData, int nData), void *pOut
);
int sqlite3changeset_start_strm(
    sqlite3_changeset_iter **pp,
    int (*xInput)(void *pIn, void *pData, int *pnData), void *pIn
);
int sqlite3changeset_start_v2_strm(
    sqlite3_changeset_iter **pp,
    int (*xInput)(void *pIn, void *pData, int *pnData), void *pIn,
    int flags
);
int sqlite3session_changeset_strm(
    sqlite3_session *pSession,
    int (*xOutput)(void *pOut, const void *pData, int nData), void *pOut
);
int sqlite3session_patchset_strm(
    sqlite3_session *pSession,
    int (*xOutput)(void *pOut, const void *pData, int nData), void *pOut
);
int sqlite3changegroup_add_strm(sqlite3_changegroup*,
    int (*xInput)(void *pIn, void *pData, int *pnData), void *pIn);
int sqlite3changegroup_output_strm(sqlite3_changegroup*,
    int (*xOutput)(void *pOut, const void *pData, int nData), void *pOut);
int sqlite3rebaser_rebase_strm(
    sqlite3_rebaser *pRebaser,
    int (*xInput)(void *pIn, void *pData, int *pnData), void *pIn,
    int (*xOutput)(void *pOut, const void *pData, int nData), void *pOut
);
int sqlite3session_config(int op, void *pArg);

#endif  /* __SQLITESESSION_H_ */

/*
** Enable all Turso experimental features for subsequently opened databases.
*/
void turso_enable_experimental(void);

/* ---- config / file control / status / VFS / extensions ---- */
typedef struct sqlite3_vfs sqlite3_vfs;

/* ---- VFS objects (layouts as in SQLite's sqlite3.h) ---- */
typedef struct sqlite3_file sqlite3_file;
typedef struct sqlite3_io_methods sqlite3_io_methods;
typedef void (*sqlite3_syscall_ptr)(void);

struct sqlite3_file {
  const struct sqlite3_io_methods *pMethods;
};

struct sqlite3_io_methods {
  int iVersion;
  int (*xClose)(sqlite3_file*);
  int (*xRead)(sqlite3_file*, void*, int iAmt, sqlite3_int64 iOfst);
  int (*xWrite)(sqlite3_file*, const void*, int iAmt, sqlite3_int64 iOfst);
  int (*xTruncate)(sqlite3_file*, sqlite3_int64 size);
  int (*xSync)(sqlite3_file*, int flags);
  int (*xFileSize)(sqlite3_file*, sqlite3_int64 *pSize);
  int (*xLock)(sqlite3_file*, int);
  int (*xUnlock)(sqlite3_file*, int);
  int (*xCheckReservedLock)(sqlite3_file*, int *pResOut);
  int (*xFileControl)(sqlite3_file*, int op, void *pArg);
  int (*xSectorSize)(sqlite3_file*);
  int (*xDeviceCharacteristics)(sqlite3_file*);
  /* Methods above are valid for version 1 */
  int (*xShmMap)(sqlite3_file*, int iPg, int pgsz, int, void volatile**);
  int (*xShmLock)(sqlite3_file*, int offset, int n, int flags);
  void (*xShmBarrier)(sqlite3_file*);
  int (*xShmUnmap)(sqlite3_file*, int deleteFlag);
  /* Methods above are valid for version 2 */
  int (*xFetch)(sqlite3_file*, sqlite3_int64 iOfst, int iAmt, void **pp);
  int (*xUnfetch)(sqlite3_file*, sqlite3_int64 iOfst, void *p);
  /* Methods above are valid for version 3 */
};

struct sqlite3_vfs {
  int iVersion;
  int szOsFile;
  int mxPathname;
  sqlite3_vfs *pNext;
  const char *zName;
  void *pAppData;
  int (*xOpen)(sqlite3_vfs*, sqlite3_filename zName, sqlite3_file*, int flags, int *pOutFlags);
  int (*xDelete)(sqlite3_vfs*, const char *zName, int syncDir);
  int (*xAccess)(sqlite3_vfs*, const char *zName, int flags, int *pResOut);
  int (*xFullPathname)(sqlite3_vfs*, const char *zName, int nOut, char *zOut);
  void *(*xDlOpen)(sqlite3_vfs*, const char *zFilename);
  void (*xDlError)(sqlite3_vfs*, int nByte, char *zErrMsg);
  void (*(*xDlSym)(sqlite3_vfs*,void*, const char *zSymbol))(void);
  void (*xDlClose)(sqlite3_vfs*, void*);
  int (*xRandomness)(sqlite3_vfs*, int nByte, char *zOut);
  int (*xSleep)(sqlite3_vfs*, int microseconds);
  int (*xCurrentTime)(sqlite3_vfs*, double*);
  int (*xGetLastError)(sqlite3_vfs*, int, char *);
  /* The methods above are in version 1 of the sqlite_vfs object. */
  int (*xCurrentTimeInt64)(sqlite3_vfs*, sqlite3_int64*);
  /* The methods above are in versions 1 and 2 of the sqlite_vfs object. */
  int (*xSetSystemCall)(sqlite3_vfs*, const char *zName, sqlite3_syscall_ptr);
  sqlite3_syscall_ptr (*xGetSystemCall)(sqlite3_vfs*, const char *zName);
  const char *(*xNextSystemCall)(sqlite3_vfs*, const char *zName);
};

#define SQLITE_LOCKED             6
#define SQLITE_READONLY           8
#define SQLITE_IOERR             10
#define SQLITE_FULL              13
#define SQLITE_IOERR_READ              (SQLITE_IOERR | (1<<8))
#define SQLITE_IOERR_SHORT_READ        (SQLITE_IOERR | (2<<8))
#define SQLITE_IOERR_WRITE             (SQLITE_IOERR | (3<<8))
#define SQLITE_IOERR_FSYNC             (SQLITE_IOERR | (4<<8))
#define SQLITE_IOERR_TRUNCATE          (SQLITE_IOERR | (6<<8))
#define SQLITE_IOERR_FSTAT             (SQLITE_IOERR | (7<<8))
#define SQLITE_IOERR_UNLOCK            (SQLITE_IOERR | (8<<8))
#define SQLITE_IOERR_RDLOCK            (SQLITE_IOERR | (9<<8))
#define SQLITE_IOERR_DELETE            (SQLITE_IOERR | (10<<8))
#define SQLITE_IOERR_LOCK              (SQLITE_IOERR | (15<<8))
#define SQLITE_IOERR_DELETE_NOENT      (SQLITE_IOERR | (23<<8))
#define SQLITE_READONLY_CANTINIT       (SQLITE_READONLY | (5<<8))

#define SQLITE_OPEN_DELETEONCLOSE    0x00000008
#define SQLITE_OPEN_EXCLUSIVE        0x00000010
#define SQLITE_OPEN_MAIN_DB          0x00000100
#define SQLITE_OPEN_TEMP_DB          0x00000200
#define SQLITE_OPEN_TRANSIENT_DB     0x00000400
#define SQLITE_OPEN_MAIN_JOURNAL     0x00000800
#define SQLITE_OPEN_TEMP_JOURNAL     0x00001000
#define SQLITE_OPEN_SUBJOURNAL       0x00002000
#define SQLITE_OPEN_SUPER_JOURNAL    0x00004000
#define SQLITE_OPEN_WAL              0x00080000

#define SQLITE_IOCAP_ATOMIC                 0x00000001
#define SQLITE_IOCAP_ATOMIC512              0x00000002
#define SQLITE_IOCAP_ATOMIC1K               0x00000004
#define SQLITE_IOCAP_ATOMIC2K               0x00000008
#define SQLITE_IOCAP_ATOMIC4K               0x00000010
#define SQLITE_IOCAP_ATOMIC8K               0x00000020
#define SQLITE_IOCAP_ATOMIC16K              0x00000040
#define SQLITE_IOCAP_ATOMIC32K              0x00000080
#define SQLITE_IOCAP_ATOMIC64K              0x00000100
#define SQLITE_IOCAP_SAFE_APPEND            0x00000200
#define SQLITE_IOCAP_SEQUENTIAL             0x00000400
#define SQLITE_IOCAP_UNDELETABLE_WHEN_OPEN  0x00000800
#define SQLITE_IOCAP_POWERSAFE_OVERWRITE    0x00001000
#define SQLITE_IOCAP_IMMUTABLE              0x00002000
#define SQLITE_IOCAP_BATCH_ATOMIC           0x00004000

#define SQLITE_LOCK_NONE          0
#define SQLITE_LOCK_SHARED        1
#define SQLITE_LOCK_RESERVED      2
#define SQLITE_LOCK_PENDING       3
#define SQLITE_LOCK_EXCLUSIVE     4

#define SQLITE_SYNC_NORMAL        0x00002
#define SQLITE_SYNC_FULL          0x00003
#define SQLITE_SYNC_DATAONLY      0x00010

#define SQLITE_ACCESS_EXISTS    0
#define SQLITE_ACCESS_READWRITE 1
#define SQLITE_ACCESS_READ      2

#define SQLITE_SHM_UNLOCK       1
#define SQLITE_SHM_LOCK         2
#define SQLITE_SHM_SHARED       4
#define SQLITE_SHM_EXCLUSIVE    8
#define SQLITE_SHM_NLOCK        8

#define SQLITE_FCNTL_LOCKSTATE             1
#define SQLITE_FCNTL_FILE_POINTER          7
#define SQLITE_FCNTL_PRAGMA               14
#define SQLITE_FCNTL_ZIPVFS               25
#define SQLITE_FCNTL_VFS_POINTER          27
#define SQLITE_FCNTL_JOURNAL_POINTER      28
#define SQLITE_FCNTL_BEGIN_ATOMIC_WRITE   31
#define SQLITE_FCNTL_COMMIT_ATOMIC_WRITE  32

#define SQLITE_CONFIG_SINGLETHREAD  1
#define SQLITE_CONFIG_MULTITHREAD   2
#define SQLITE_CONFIG_SERIALIZED    3
#define SQLITE_CONFIG_MEMSTATUS     9
#define SQLITE_CONFIG_LOG          16
#define SQLITE_CONFIG_URI          17

/* File-control opcodes honoured by sqlite3_file_control(); others return
** SQLITE_NOTFOUND. */
#define SQLITE_FCNTL_LAST_ERRNO           4
#define SQLITE_FCNTL_SIZE_HINT            5
#define SQLITE_FCNTL_CHUNK_SIZE           6
#define SQLITE_FCNTL_PERSIST_WAL         10
#define SQLITE_FCNTL_VFSNAME             12
#define SQLITE_FCNTL_POWERSAFE_OVERWRITE 13
#define SQLITE_FCNTL_TEMPFILENAME        16
#define SQLITE_FCNTL_MMAP_SIZE           18
#define SQLITE_FCNTL_HAS_MOVED           20
#define SQLITE_FCNTL_DATA_VERSION        35
#define SQLITE_FCNTL_SIZE_LIMIT          36
#define SQLITE_FCNTL_RESERVE_BYTES       38
#define SQLITE_FCNTL_RESET_CACHE         42

extern char *sqlite3_temp_directory;

int sqlite3_config(int, ...);
int sqlite3_test_control(int op, ...);
int sqlite3_file_control(sqlite3*, const char *zDbName, int op, void*);
int sqlite3_status(int op, int *pCurrent, int *pHighwater, int resetFlag);
int sqlite3_status64(int op, sqlite3_int64 *pCurrent, sqlite3_int64 *pHighwater, int resetFlag);
int sqlite3_db_status(sqlite3*, int op, int *pCur, int *pHiwtr, int resetFlg);
sqlite3_mutex *sqlite3_db_mutex(sqlite3*);
int sqlite3_unlock_notify(sqlite3 *pBlocked,
                          void (*xNotify)(void **apArg, int nArg),
                          void *pNotifyArg);
int sqlite3_auto_extension(void(*xEntryPoint)(void));
int sqlite3_cancel_auto_extension(void(*xEntryPoint)(void));
void sqlite3_reset_auto_extension(void);
int sqlite3_enable_load_extension(sqlite3 *db, int onoff);
int sqlite3_load_extension(sqlite3 *db, const char *zFile, const char *zProc, char **pzErrMsg);
sqlite3_vfs *sqlite3_vfs_find(const char *zVfsName);
int sqlite3_vfs_register(sqlite3_vfs*, int makeDflt);
int sqlite3_vfs_unregister(sqlite3_vfs*);

/* ---- additional declarations (abi) ---- */
void sqlite3_result_error_code(sqlite3_context*, int);
void sqlite3_result_str(sqlite3_context*, sqlite3_str*, int);
void sqlite3_str_vappendf(sqlite3_str*, const char *zFormat, va_list);
int sqlite3_value_bytes16(sqlite3_value*);
const void *sqlite3_value_text16(sqlite3_value*);
const void *sqlite3_value_text16be(sqlite3_value*);
const void *sqlite3_value_text16le(sqlite3_value*);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* LIMBO_SQLITE3_H */
