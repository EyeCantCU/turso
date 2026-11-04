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

int sqlite3_initialize(void);

int sqlite3_shutdown(void);

int sqlite3_open(const char *filename, sqlite3 **db_out);

int sqlite3_open_v2(const char *filename, sqlite3 **db_out, int _flags, const char *_z_vfs);

int sqlite3_close(sqlite3 *db);

int sqlite3_close_v2(sqlite3 *db);

const char *sqlite3_db_filename(sqlite3 *db, const char *db_name);

int sqlite3_trace_v2(sqlite3 *_db,
                     unsigned int _mask,
                     void (*_callback)(unsigned int, void*, void*, void*),
                     void *_context);

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

int sqlite3_serialize(sqlite3 *_db, const char *_schema, void **_out, int *_out_bytes, unsigned int _flags);

int sqlite3_deserialize(sqlite3 *_db, const char *_schema, const void *_in_, int _in_bytes, unsigned int _flags);

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

void sqlite3_sleep(int _ms);

int sqlite3_limit(sqlite3 *_db, int _id, int _new_value);

void *sqlite3_malloc64(int _n);

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
typedef const char *sqlite3_filename;

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
