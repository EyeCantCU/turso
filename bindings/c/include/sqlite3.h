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

#define SQLITE_STATE_OPEN 118

#define SQLITE_STATE_SICK 186

#define SQLITE_STATE_BUSY 109

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
typedef int64_t sqlite3_int64;
typedef sqlite3_int64 sqlite_int64;

typedef int (*exec_callback)(void *context, int n_column, char **argv, char **colv);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/* Added in SQLite 3.51 - 3.54 (and the sqlite3_str API they extend) */
typedef struct sqlite3_str sqlite3_str;
sqlite3_int64 sqlite3_incomplete(const char *sql);
int sqlite3_complete16(const void *sql);
int sqlite3_set_errmsg(sqlite3 *db, int errcode, const char *zErrMsg);
int sqlite3_db_status64(sqlite3 *db, int op, sqlite3_int64 *pCur, sqlite3_int64 *pHiwtr, int resetFlg);
sqlite3_str *sqlite3_str_new(sqlite3 *db);
char *sqlite3_str_finish(sqlite3_str *p);
void sqlite3_str_free(sqlite3_str *p);
void sqlite3_str_appendf(sqlite3_str *p, const char *zFormat, ...);
void sqlite3_str_append(sqlite3_str *p, const char *zIn, int N);
void sqlite3_str_appendall(sqlite3_str *p, const char *zIn);
void sqlite3_str_appendchar(sqlite3_str *p, int N, char C);
void sqlite3_str_reset(sqlite3_str *p);
void sqlite3_str_truncate(sqlite3_str *p, int N);
int sqlite3_str_errcode(sqlite3_str *p);
int sqlite3_str_length(sqlite3_str *p);
char *sqlite3_str_value(sqlite3_str *p);

int sqlite3_initialize(void);

int sqlite3_shutdown(void);

int sqlite3_open(const char *filename, sqlite3 **db_out);

int sqlite3_open_v2(const char *filename, sqlite3 **db_out, int _flags, const char *_z_vfs);

int sqlite3_close(sqlite3 *db);

int sqlite3_close_v2(sqlite3 *db);

const char *sqlite3_db_filename(sqlite3 *db, const char *db_name);

#define SQLITE_TRACE_STMT 0x01
#define SQLITE_TRACE_PROFILE 0x02
#define SQLITE_TRACE_ROW 0x04
#define SQLITE_TRACE_CLOSE 0x08

int sqlite3_trace_v2(sqlite3 *db,
                     unsigned int mask,
                     int (*callback)(unsigned int, void*, void*, void*),
                     void *context);

void *sqlite3_trace(sqlite3 *db, void (*xTrace)(void*, const char*), void *context);

void *sqlite3_profile(sqlite3 *db, void (*xProfile)(void*, const char*, uint64_t), void *context);

int sqlite3_collation_needed(sqlite3 *db,
                             void *context,
                             void (*callback)(void*, sqlite3*, int, const char*));

int sqlite3_collation_needed16(sqlite3 *db,
                               void *context,
                               void (*callback)(void*, sqlite3*, int, const void*));

int sqlite3_open16(const void *filename, sqlite3 **db_out);

void sqlite3_progress_handler(sqlite3 *_db, int _n, int (*_callback)(void *), void *_context);

int sqlite3_busy_timeout(sqlite3 *_db, int _ms);
int sqlite3_db_readonly(sqlite3 *_db, const char *_db_name);

int sqlite3_set_authorizer(sqlite3 *db,
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

void *sqlite3_context_db_handle(void *_context);

int sqlite3_prepare_v2(sqlite3 *db, const char *sql, int _len, sqlite3_stmt **out_stmt, const char **_tail);

int sqlite3_prepare_v3(sqlite3 *db, const char *sql, int _len, unsigned int _prep_flags, sqlite3_stmt **out_stmt, const char **_tail);

#define SQLITE_PREPARE_PERSISTENT 0x01
#define SQLITE_PREPARE_NORMALIZE  0x02
#define SQLITE_PREPARE_NO_VTAB    0x04

int sqlite3_finalize(sqlite3_stmt *stmt);

int sqlite3_step(sqlite3_stmt *stmt);

int sqlite3_exec(sqlite3 *db, const char *sql, exec_callback _callback, void *_context, char **_err);

int sqlite3_reset(sqlite3_stmt *stmt);

int sqlite3_clear_bindings(sqlite3_stmt *stmt);

int sqlite3_changes(sqlite3 *_db);

int64_t sqlite3_changes64(sqlite3 *_db);

int sqlite3_stmt_readonly(sqlite3_stmt *_stmt);

int sqlite3_stmt_busy(sqlite3_stmt *_stmt);

int sqlite3_stmt_status(sqlite3_stmt *stmt, int op, int resetFlg);

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

sqlite3_stmt *sqlite3_next_stmt(sqlite3 *db, sqlite3_stmt *stmt);

unsigned char *sqlite3_serialize(sqlite3 *db, const char *zSchema, sqlite3_int64 *piSize, unsigned int mFlags);

int sqlite3_deserialize(sqlite3 *db, const char *zSchema, unsigned char *pData, sqlite3_int64 szDb, sqlite3_int64 szBuf, unsigned int mFlags);

int sqlite3_get_autocommit(sqlite3 *_db);

int sqlite3_total_changes(sqlite3 *_db);

int64_t sqlite3_total_changes64(sqlite3 *_db);

int64_t sqlite3_last_insert_rowid(sqlite3 *_db);

void sqlite3_interrupt(sqlite3 *_db);

int sqlite3_db_config(sqlite3 *db, int op, ...);

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

int sqlite3_extended_result_codes(sqlite3 *db, int onoff);

sqlite3 *sqlite3_db_handle(sqlite3_stmt *_stmt);

int sqlite3_sleep(int _ms);

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

int sqlite3_limit(sqlite3 *db, int id, int new_value);

void *sqlite3_malloc64(uint64_t _n);

void sqlite3_free(void *_ptr);

char *sqlite3_mprintf(const char *fmt, ...);

char *sqlite3_snprintf(int n, char *buf, const char *fmt, ...);

int sqlite3_errcode(sqlite3 *_db);

const char *sqlite3_errstr(int _err);

void *sqlite3_user_data(void *_context);

void *sqlite3_backup_init(sqlite3 *_dest_db, const char *_dest_name, sqlite3 *_source_db, const char *_source_name);

int sqlite3_backup_step(void *_backup, int _n_pages);

int sqlite3_backup_remaining(void *_backup);

int sqlite3_backup_pagecount(void *_backup);

int sqlite3_backup_finish(void *_backup);

const char *sqlite3_sql(sqlite3_stmt *stmt);

char *sqlite3_expanded_sql(sqlite3_stmt *stmt);

int sqlite3_data_count(sqlite3_stmt *stmt);

int sqlite3_bind_parameter_count(sqlite3_stmt *_stmt);

const char *sqlite3_bind_parameter_name(sqlite3_stmt *_stmt, int _idx);

int sqlite3_bind_parameter_index(sqlite3_stmt *_stmt, const char *_name);

int sqlite3_bind_null(sqlite3_stmt *_stmt, int _idx);

int sqlite3_bind_int64(sqlite3_stmt *_stmt, int _idx, int64_t _val);

int sqlite3_bind_double(sqlite3_stmt *_stmt, int _idx, double _val);

int sqlite3_bind_text(sqlite3_stmt *_stmt, int _idx, const char *_text, int _len, void *_destroy);

int sqlite3_bind_blob(sqlite3_stmt *_stmt, int _idx, const void *_blob, int _len, void *_destroy);

int sqlite3_column_type(sqlite3_stmt *_stmt, int _idx);

int sqlite3_column_count(sqlite3_stmt *_stmt);

const char *sqlite3_column_decltype(sqlite3_stmt *_stmt, int _idx);

const char *sqlite3_column_name(sqlite3_stmt *_stmt, int _idx);

const char *sqlite3_column_table_name(sqlite3_stmt *_stmt, int _idx);

int64_t sqlite3_column_int64(sqlite3_stmt *_stmt, int _idx);

double sqlite3_column_double(sqlite3_stmt *_stmt, int _idx);

const void *sqlite3_column_blob(sqlite3_stmt *_stmt, int _idx);

int sqlite3_column_bytes(sqlite3_stmt *_stmt, int _idx);

void *sqlite3_column_value(sqlite3_stmt *_stmt, int _idx);

int sqlite3_value_type(void *value);

int64_t sqlite3_value_int64(void *value);

int sqlite3_value_int(void *value);

double sqlite3_value_double(void *value);

const unsigned char *sqlite3_value_text(void *value);

const void *sqlite3_value_blob(void *value);

int sqlite3_value_bytes(void *value);

void *sqlite3_value_dup(void *value);

void sqlite3_value_free(void *value);

const unsigned char *sqlite3_column_text(sqlite3_stmt *stmt, int idx);

int sqlite_get_table_cb(void *context, int n_column, char **argv, char **colv);

int sqlite3_get_table(sqlite3 *db, const char *sql, char ***paz_result, int *pn_row, int *pn_column, char **pz_err_msg);

void sqlite3_free_table(char **az_result);

void sqlite3_result_null(void *_context);

void sqlite3_result_int64(void *_context, int64_t _val);

void sqlite3_result_int(void *_context, int _val);

void sqlite3_result_double(void *_context, double _val);

void sqlite3_result_text(void *_context, const char *_text, int _len, void *_destroy);

void sqlite3_result_blob(void *_context, const void *_blob, int _len, void *_destroy);

void sqlite3_result_error_nomem(void *_context);

void sqlite3_result_error_toobig(void *_context);

void sqlite3_result_error(void *_context, const char *_err, int _len);

void *sqlite3_aggregate_context(void *_context, int _n);

int sqlite3_blob_open(sqlite3 *_db,
                      const char *_db_name,
                      const char *_table_name,
                      const char *_column_name,
                      int64_t _rowid,
                      int _flags,
                      void **_blob_out);

int sqlite3_blob_read(void *_blob, void *_data, int _n, int _offset);

int sqlite3_blob_write(void *_blob, const void *_data, int _n, int _offset);

int sqlite3_blob_reopen(void *blob, int64_t rowid);

int sqlite3_blob_bytes(void *_blob);

int sqlite3_blob_close(void *_blob);

int sqlite3_stricmp(const char *_a, const char *_b);

int sqlite3_strnicmp(const char *_a, const char *_b, int _n);

int sqlite3_create_collation_v2(sqlite3 *_db,
                                const char *_name,
                                int _enc,
                                void *_context,
                                int (*_cmp)(void),
                                void (*_destroy)(void));

int sqlite3_create_function_v2(sqlite3 *_db,
                               const char *_name,
                               int _n_args,
                               int _enc,
                               void *_context,
                               void (*_func)(void),
                               void (*_step)(void),
                               void (*_final_)(void),
                               void (*_destroy)(void));

int sqlite3_create_window_function(sqlite3 *_db,
                                   const char *_name,
                                   int _n_args,
                                   int _enc,
                                   void *_context,
                                   void (*_x_step)(void),
                                   void (*_x_final)(void),
                                   void (*_x_value)(void),
                                   void (*_x_inverse)(void),
                                   void (*_destroy)(void));

const char *sqlite3_errmsg(sqlite3 *_db);

int sqlite3_extended_errcode(sqlite3 *_db);

int sqlite3_complete(const char *_sql);

int sqlite3_threadsafe(void);

const char *sqlite3_libversion(void);

int sqlite3_libversion_number(void);

int sqlite3_wal_checkpoint(sqlite3 *_db, const char *_db_name);

int sqlite3_wal_checkpoint_v2(sqlite3 *db, const char *_db_name, int _mode, int *_log_size, int *_checkpoint_count);

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
int sqlite3_table_column_metadata(
    sqlite3 *db,
    const char *zDbName,
    const char *zTableName,
    const char *zColumnName,
    char const **pzDataType,
    char const **pzCollSeq,
    int *pNotNull,
    int *pPrimaryKey,
    int *pAutoinc
);

/* ---- src/wrappers.rs ---- */
#include <stdarg.h>
typedef uint64_t sqlite3_uint64;
extern const char sqlite3_version[];
int sqlite3_create_function(sqlite3 *db, const char *zName, int nArg, int eTextRep, void *pApp,
    void (*xFunc)(void*, int, void**), void (*xStep)(void*, int, void**), void (*xFinal)(void*));
int sqlite3_create_collation(sqlite3 *db, const char *zName, int eTextRep, void *pArg,
    int (*xCompare)(void*, int, const void*, int, const void*));
int sqlite3_prepare(sqlite3 *db, const char *zSql, int nByte, sqlite3_stmt **ppStmt, const char **pzTail);
char *sqlite3_vmprintf(const char *zFormat, va_list ap);
char *sqlite3_vsnprintf(int n, char *zBuf, const char *zFormat, va_list ap);
int sqlite3_bind_text64(sqlite3_stmt *stmt, int idx, const char *z, sqlite3_uint64 n,
    void (*xDel)(void*), unsigned char enc);
int sqlite3_bind_blob64(sqlite3_stmt *stmt, int idx, const void *z, sqlite3_uint64 n, void (*xDel)(void*));
void sqlite3_result_text64(void *ctx, const char *z, sqlite3_uint64 n, void (*xDel)(void*), unsigned char enc);
void *sqlite3_realloc(void *p, int n);
void *sqlite3_realloc64(void *p, sqlite3_uint64 n);
const char *sqlite3_sourceid(void);
int sqlite3_keyword_count(void);
int sqlite3_keyword_name(int i, const char **pzName, int *pnName);
int sqlite3_keyword_check(const char *z, int n);
void sqlite3_result_value(void *ctx, void *value);
void sqlite3_result_zeroblob(void *ctx, int n);
int sqlite3_result_zeroblob64(void *ctx, sqlite3_uint64 n);
int sqlite3_value_numeric_type(void *value);
const char *sqlite3_column_origin_name(sqlite3_stmt *stmt, int idx);
int sqlite3_db_release_memory(sqlite3 *db);
int sqlite3_system_errno(sqlite3 *db);
int sqlite3_stmt_isexplain(sqlite3_stmt *stmt);
int sqlite3_stmt_explain(sqlite3_stmt *stmt, int eMode);
int sqlite3_error_offset(sqlite3 *db);
int sqlite3_enable_shared_cache(int enable);
/* ---- end src/wrappers.rs ---- */
/* ---- Chainguard compat (stubs group): UTF-16 API ---- */
#define SQLITE_SERIALIZE_NOCOPY 0x001
#define SQLITE_DESERIALIZE_FREEONCLOSE 1
#define SQLITE_DESERIALIZE_RESIZEABLE  2
#define SQLITE_DESERIALIZE_READONLY    4
int sqlite3_prepare16_v2(sqlite3 *db, const void *zSql, int nByte, sqlite3_stmt **ppStmt, const void **pzTail);
const void *sqlite3_errmsg16(sqlite3 *db);
int sqlite3_bind_text16(sqlite3_stmt *stmt, int idx, const void *data, int nByte, void (*destructor)(void*));
const void *sqlite3_column_text16(sqlite3_stmt *stmt, int idx);
int sqlite3_column_bytes16(sqlite3_stmt *stmt, int idx);
const void *sqlite3_column_name16(sqlite3_stmt *stmt, int idx);
const void *sqlite3_column_decltype16(sqlite3_stmt *stmt, int idx);
const void *sqlite3_column_table_name16(sqlite3_stmt *stmt, int idx);

/*
** Data-change and transaction hooks (hooks.rs).
*/
void *sqlite3_commit_hook(sqlite3 *db, int (*xCallback)(void *), void *pArg);
void *sqlite3_rollback_hook(sqlite3 *db, void (*xCallback)(void *), void *pArg);
void *sqlite3_update_hook(
    sqlite3 *db,
    void (*xCallback)(void *, int, char const *, char const *, sqlite3_int64),
    void *pArg
);
void *sqlite3_wal_hook(
    sqlite3 *db,
    int (*xCallback)(void *, sqlite3 *, const char *, int),
    void *pArg
);
void *sqlite3_preupdate_hook(
    sqlite3 *db,
    void (*xPreUpdate)(void *pCtx, sqlite3 *db, int op, char const *zDb,
                       char const *zName, sqlite3_int64 iKey1, sqlite3_int64 iKey2),
    void *pCtx
);
int sqlite3_preupdate_old(sqlite3 *db, int i, void **ppValue);
int sqlite3_preupdate_new(sqlite3 *db, int i, void **ppValue);
int sqlite3_preupdate_count(sqlite3 *db);
int sqlite3_preupdate_depth(sqlite3 *db);
int sqlite3_preupdate_blobwrite(sqlite3 *db);
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

int sqlite3_create_module(sqlite3 *db, const char *zName,
                          const sqlite3_module *p, void *pClientData);
int sqlite3_create_module_v2(sqlite3 *db, const char *zName,
                             const sqlite3_module *p, void *pClientData,
                             void (*xDestroy)(void*));
int sqlite3_declare_vtab(sqlite3 *db, const char *zSQL);
int sqlite3_vtab_config(sqlite3 *db, int op, ...);
int sqlite3_vtab_on_conflict(sqlite3 *db);
int sqlite3_vtab_nochange(void *ctx);
const char *sqlite3_vtab_collation(sqlite3_index_info *pIdxInfo, int iCons);
int sqlite3_vtab_distinct(sqlite3_index_info *pIdxInfo);
int sqlite3_vtab_in(sqlite3_index_info *pIdxInfo, int iCons, int bHandle);
int sqlite3_vtab_in_first(void *pVal, void **ppOut);
int sqlite3_vtab_in_next(void *pVal, void **ppOut);
int sqlite3_vtab_rhs_value(sqlite3_index_info *pIdxInfo, int iCons, void **ppVal);
/* ===== end vtab group ===== */

/*
** Enable all Turso experimental features for subsequently opened databases.
*/
void turso_enable_experimental(void);

/* ---- config / file control / status / VFS / extensions ---- */
typedef struct sqlite3_vfs sqlite3_vfs;

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

int sqlite3_config(int op, ...);
int sqlite3_test_control(int op, ...);
int sqlite3_file_control(sqlite3 *db, const char *zDbName, int op, void *pArg);
int sqlite3_status(int op, int *pCurrent, int *pHighwater, int resetFlag);
int sqlite3_status64(int op, sqlite3_int64 *pCurrent, sqlite3_int64 *pHighwater, int resetFlag);
int sqlite3_db_status(sqlite3 *db, int op, int *pCurrent, int *pHighwater, int resetFlag);
void *sqlite3_db_mutex(sqlite3 *db);
int sqlite3_unlock_notify(sqlite3 *db, void (*xNotify)(void **apArg, int nArg), void *pArg);
int sqlite3_auto_extension(void (*xEntryPoint)(void));
int sqlite3_cancel_auto_extension(void (*xEntryPoint)(void));
void sqlite3_reset_auto_extension(void);
int sqlite3_enable_load_extension(sqlite3 *db, int onoff);
int sqlite3_load_extension(sqlite3 *db, const char *zFile, const char *zProc, char **pzErrMsg);
sqlite3_vfs *sqlite3_vfs_find(const char *zVfsName);
int sqlite3_vfs_register(sqlite3_vfs *pVfs, int makeDflt);
int sqlite3_vfs_unregister(sqlite3_vfs *pVfs);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* LIMBO_SQLITE3_H */
