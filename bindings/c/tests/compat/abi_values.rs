//! C ABI and value-accessor conformance: `int` arguments, text/blob
//! conversions and NUL termination, as SQLite defines them.
use super::*;
use std::ffi::CStr;

const SQLITE_STATIC: Option<unsafe extern "C" fn(*mut libc::c_void)> = None;

extern "C" {
    fn sqlite3_column_double(stmt: *mut sqlite3_stmt, idx: i32) -> f64;
    static sqlite3_version: [libc::c_char; 0];
    static sqlite3_temp_directory: *mut libc::c_char;
    static sqlite3_data_directory: *mut libc::c_char;
}

unsafe fn open_memory() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn prepare(db: *mut sqlite3, sql: &CStr) -> *mut sqlite3_stmt {
    let mut stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()),
        SQLITE_OK
    );
    stmt
}

/// Reads `n` bytes at `p` plus the byte after them.
unsafe fn bytes_and_terminator(p: *const u8, n: usize) -> (Vec<u8>, u8) {
    (std::slice::from_raw_parts(p, n).to_vec(), *p.add(n))
}

/// Calls `sqlite3_bind_int` with the upper half of the argument registers
/// poisoned, as a C caller may leave them for an `int` parameter.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
unsafe fn bind_int_poisoned(stmt: *mut sqlite3_stmt, idx: i32, val: i32) -> i32 {
    const POISON: u64 = 0xdead_beef_0000_0000;
    let f = sqlite3_bind_int as unsafe extern "C" fn(*mut sqlite3_stmt, i32, i32) -> i32;
    let idx = idx as u32 as u64 | POISON;
    let val = val as u32 as u64 | POISON;
    let rc: u64;
    #[cfg(target_arch = "x86_64")]
    std::arch::asm!(
        "call {f}",
        f = in(reg) f,
        in("rdi") stmt,
        in("rsi") idx,
        in("rdx") val,
        lateout("rax") rc,
        clobber_abi("C"),
    );
    #[cfg(target_arch = "aarch64")]
    std::arch::asm!(
        "blr {f}",
        f = in(reg) f,
        inlateout("x0") stmt => rc,
        in("x1") idx,
        in("x2") val,
        clobber_abi("C"),
    );
    rc as u32 as i32
}

#[test]
fn bind_int_negative_and_boundary_values() {
    unsafe {
        let db = open_memory();
        let ins = prepare(db, c"SELECT ?1, typeof(?1)");
        for v in [
            0,
            1,
            -1,
            42,
            -42,
            i32::MAX,
            i32::MIN,
            i32::MIN + 1,
            65535,
            -65536,
        ] {
            assert_eq!(sqlite3_bind_int(ins, 1, v), SQLITE_OK);
            assert_eq!(sqlite3_step(ins), SQLITE_ROW);
            assert_eq!(sqlite3_column_int64(ins, 0), v as i64, "bind_int({v})");
            assert_eq!(sqlite3_column_int(ins, 0), v);
            assert_eq!(
                CStr::from_ptr(sqlite3_column_text(ins, 1) as *const libc::c_char),
                c"integer"
            );
            assert_eq!(sqlite3_reset(ins), SQLITE_OK);
        }
        assert_eq!(sqlite3_bind_int(ins, 0, 1), SQLITE_RANGE);
        assert_eq!(sqlite3_bind_int(ins, 2, 1), SQLITE_RANGE);
        assert_eq!(sqlite3_finalize(ins), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn bind_int_ignores_upper_register_half() {
    unsafe {
        let db = open_memory();
        let stmt = prepare(db, c"SELECT ?1");
        for v in [-1, i32::MIN, i32::MAX, 7, -7] {
            assert_eq!(bind_int_poisoned(stmt, 1, v), SQLITE_OK);
            assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
            assert_eq!(sqlite3_column_int64(stmt, 0), v as i64, "bind_int({v})");
            assert_eq!(sqlite3_reset(stmt), SQLITE_OK);
        }
        // idx is an int too
        assert_eq!(bind_int_poisoned(stmt, 0, 1), SQLITE_RANGE);
        assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn column_int_truncates_like_c_int() {
    unsafe {
        let db = open_memory();
        let stmt = prepare(db, c"SELECT 4294967297, -4294967297, 2147483648");
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(stmt, 0), 1);
        assert_eq!(sqlite3_column_int(stmt, 1), -1);
        assert_eq!(sqlite3_column_int(stmt, 2), i32::MIN);
        assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// Expected conversions of one result column.
struct Col {
    sql: &'static str,
    ty: i32,
    /// text conversion; None for NULL
    text: Option<&'static [u8]>,
    /// blob conversion; None for NULL or zero length
    blob: Option<&'static [u8]>,
    int: i64,
    double: f64,
}

const COLS: &[Col] = &[
    Col {
        sql: "'hello'",
        ty: SQLITE_TEXT,
        text: Some(b"hello"),
        blob: Some(b"hello"),
        int: 0,
        double: 0.0,
    },
    Col {
        sql: "'h\u{e9}llo'",
        ty: SQLITE_TEXT,
        text: Some("h\u{e9}llo".as_bytes()),
        blob: Some("h\u{e9}llo".as_bytes()),
        int: 0,
        double: 0.0,
    },
    Col {
        sql: "''",
        ty: SQLITE_TEXT,
        text: Some(b""),
        blob: None,
        int: 0,
        double: 0.0,
    },
    Col {
        sql: "x'414243'",
        ty: SQLITE_BLOB,
        text: Some(b"ABC"),
        blob: Some(b"ABC"),
        int: 0,
        double: 0.0,
    },
    Col {
        sql: "x'3132'",
        ty: SQLITE_BLOB,
        text: Some(b"12"),
        blob: Some(b"12"),
        int: 12,
        double: 12.0,
    },
    Col {
        sql: "123",
        ty: SQLITE_INTEGER,
        text: Some(b"123"),
        blob: Some(b"123"),
        int: 123,
        double: 123.0,
    },
    Col {
        sql: "-45",
        ty: SQLITE_INTEGER,
        text: Some(b"-45"),
        blob: Some(b"-45"),
        int: -45,
        double: -45.0,
    },
    Col {
        sql: "1.5",
        ty: SQLITE_FLOAT,
        text: Some(b"1.5"),
        blob: Some(b"1.5"),
        int: 1,
        double: 1.5,
    },
    Col {
        sql: "100.0",
        ty: SQLITE_FLOAT,
        text: Some(b"100.0"),
        blob: Some(b"100.0"),
        int: 100,
        double: 100.0,
    },
    Col {
        sql: "NULL",
        ty: SQLITE_NULL,
        text: None,
        blob: None,
        int: 0,
        double: 0.0,
    },
    Col {
        sql: "'12abc'",
        ty: SQLITE_TEXT,
        text: Some(b"12abc"),
        blob: Some(b"12abc"),
        int: 12,
        double: 12.0,
    },
    Col {
        sql: "'1.9'",
        ty: SQLITE_TEXT,
        text: Some(b"1.9"),
        blob: Some(b"1.9"),
        int: 1,
        double: 1.9,
    },
    Col {
        sql: "'  7'",
        ty: SQLITE_TEXT,
        text: Some(b"  7"),
        blob: Some(b"  7"),
        int: 7,
        double: 7.0,
    },
    Col {
        sql: "'-3'",
        ty: SQLITE_TEXT,
        text: Some(b"-3"),
        blob: Some(b"-3"),
        int: -3,
        double: -3.0,
    },
    Col {
        sql: "'3.7e2'",
        ty: SQLITE_TEXT,
        text: Some(b"3.7e2"),
        blob: Some(b"3.7e2"),
        int: 3,
        double: 370.0,
    },
    Col {
        sql: "'1e2'",
        ty: SQLITE_TEXT,
        text: Some(b"1e2"),
        blob: Some(b"1e2"),
        int: 1,
        double: 100.0,
    },
    Col {
        sql: "'0x10'",
        ty: SQLITE_TEXT,
        text: Some(b"0x10"),
        blob: Some(b"0x10"),
        int: 0,
        double: 0.0,
    },
    Col {
        sql: "'99999999999999999999'",
        ty: SQLITE_TEXT,
        text: Some(b"99999999999999999999"),
        blob: Some(b"99999999999999999999"),
        int: i64::MAX,
        double: 1e20,
    },
    Col {
        sql: "'-99999999999999999999'",
        ty: SQLITE_TEXT,
        text: Some(b"-99999999999999999999"),
        blob: Some(b"-99999999999999999999"),
        int: i64::MIN,
        double: -1e20,
    },
    Col {
        sql: "1e30",
        ty: SQLITE_FLOAT,
        text: Some(b"1.0e+30"),
        blob: Some(b"1.0e+30"),
        int: i64::MAX,
        double: 1e30,
    },
    Col {
        sql: "-1.9",
        ty: SQLITE_FLOAT,
        text: Some(b"-1.9"),
        blob: Some(b"-1.9"),
        int: -1,
        double: -1.9,
    },
    Col {
        sql: "'abc'",
        ty: SQLITE_TEXT,
        text: Some(b"abc"),
        blob: Some(b"abc"),
        int: 0,
        double: 0.0,
    },
];

unsafe fn check_value_accessors(v: *mut libc::c_void, c: &Col) {
    let what = c.sql;
    // Conversions may run in any order and repeat; run them twice.
    for _ in 0..2 {
        let text = sqlite3_value_text(v) as *const u8;
        let n = sqlite3_value_bytes(v) as usize;
        match c.text {
            None => {
                assert!(text.is_null(), "{what}: value_text of NULL");
                assert_eq!(n, 0);
            }
            Some(want) => {
                assert!(!text.is_null(), "{what}: value_text is NULL");
                assert_eq!(n, want.len(), "{what}: value_bytes");
                let (got, term) = bytes_and_terminator(text, n);
                assert_eq!(got, want, "{what}: value_text bytes");
                assert_eq!(term, 0, "{what}: value_text NUL terminator");
            }
        }
        let blob = sqlite3_value_blob(v) as *const u8;
        let nb = sqlite3_value_bytes(v) as usize;
        match c.blob {
            None => assert!(blob.is_null(), "{what}: value_blob should be NULL"),
            Some(want) => {
                assert!(!blob.is_null(), "{what}: value_blob is NULL");
                assert_eq!(nb, want.len(), "{what}: value_bytes after value_blob");
                assert_eq!(
                    std::slice::from_raw_parts(blob, nb),
                    want,
                    "{what}: value_blob bytes"
                );
            }
        }
        assert_eq!(sqlite3_value_int64(v), c.int, "{what}: value_int64");
        assert_eq!(sqlite3_value_int(v), c.int as i32, "{what}: value_int");
        assert!(
            (sqlite3_value_double(v) - c.double).abs() < 1e-9,
            "{what}: value_double"
        );
    }
}

#[test]
fn value_accessors_through_column_value() {
    unsafe {
        let db = open_memory();
        for c in COLS {
            let sql = std::ffi::CString::new(format!("SELECT {}", c.sql)).unwrap();
            let stmt = prepare(db, &sql);
            assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
            let v = sqlite3_column_value(stmt, 0);
            assert!(!v.is_null());
            assert_eq!(sqlite3_value_type(v), c.ty, "{}: value_type", c.sql);
            check_value_accessors(v, c);
            assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
        }
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn column_accessors_convert_and_terminate() {
    unsafe {
        let db = open_memory();
        for c in COLS {
            let sql = std::ffi::CString::new(format!("SELECT {}", c.sql)).unwrap();
            let stmt = prepare(db, &sql);
            assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
            let what = c.sql;
            assert_eq!(sqlite3_column_type(stmt, 0), c.ty, "{what}: column_type");
            for _ in 0..2 {
                let text = sqlite3_column_text(stmt, 0) as *const u8;
                let n = sqlite3_column_bytes(stmt, 0) as usize;
                match c.text {
                    None => {
                        assert!(text.is_null(), "{what}: column_text of NULL");
                        assert_eq!(n, 0);
                    }
                    Some(want) => {
                        assert!(!text.is_null(), "{what}: column_text is NULL");
                        assert_eq!(n, want.len(), "{what}: column_bytes");
                        let (got, term) = bytes_and_terminator(text, n);
                        assert_eq!(got, want, "{what}: column_text bytes");
                        assert_eq!(term, 0, "{what}: column_text NUL terminator");
                    }
                }
                let blob = sqlite3_column_blob(stmt, 0) as *const u8;
                let nb = sqlite3_column_bytes(stmt, 0) as usize;
                match c.blob {
                    None => assert!(blob.is_null(), "{what}: column_blob should be NULL"),
                    Some(want) => {
                        assert!(!blob.is_null(), "{what}: column_blob is NULL");
                        assert_eq!(nb, want.len(), "{what}: column_bytes after column_blob");
                        assert_eq!(
                            std::slice::from_raw_parts(blob, nb),
                            want,
                            "{what}: column_blob bytes"
                        );
                    }
                }
                assert_eq!(sqlite3_column_int64(stmt, 0), c.int, "{what}: column_int64");
                assert_eq!(
                    sqlite3_column_int(stmt, 0),
                    c.int as i32,
                    "{what}: column_int"
                );
                assert!(
                    (sqlite3_column_double(stmt, 0) - c.double).abs() < 1e-9,
                    "{what}: column_double"
                );
            }
            assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
        }
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn value_accessors_in_user_function() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CHECKED: AtomicUsize = AtomicUsize::new(0);
    static FAILED: AtomicUsize = AtomicUsize::new(0);

    // f(i, ...) checks its argument against COLS[i], returning 1 when ok
    unsafe extern "C" fn check_fn(ctx: *mut libc::c_void, argc: i32, argv: *mut *mut libc::c_void) {
        assert_eq!(argc, 2);
        let i = sqlite3_value_int64(*argv) as usize;
        let v = *argv.add(1);
        let c = &COLS[i];
        let r = std::panic::catch_unwind(|| check_value_accessors(v, c));
        CHECKED.fetch_add(1, Ordering::SeqCst);
        if r.is_err() {
            FAILED.fetch_add(1, Ordering::SeqCst);
        }
        sqlite3_result_int(ctx, r.is_ok() as i32);
    }

    unsafe {
        let db = open_memory();
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"check_value".as_ptr(),
                2,
                SQLITE_UTF8,
                ptr::null_mut(),
                Some(check_fn),
                None,
                None,
                None,
            ),
            SQLITE_OK
        );
        for (i, c) in COLS.iter().enumerate() {
            let sql =
                std::ffi::CString::new(format!("SELECT check_value({i}, {})", c.sql)).unwrap();
            let stmt = prepare(db, &sql);
            assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
            assert_eq!(
                sqlite3_column_int(stmt, 0),
                1,
                "{}: accessors in UDF",
                c.sql
            );
            assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
        }
        assert_eq!(CHECKED.load(Ordering::SeqCst), COLS.len());
        assert_eq!(FAILED.load(Ordering::SeqCst), 0);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn value_blob_of_text_in_user_function_matches_text() {
    unsafe extern "C" fn len_fn(ctx: *mut libc::c_void, _argc: i32, argv: *mut *mut libc::c_void) {
        let v = *argv;
        // blob first, then text: both views must agree
        let blob = sqlite3_value_blob(v) as *const u8;
        let n = sqlite3_value_bytes(v);
        let text = sqlite3_value_text(v) as *const u8;
        let ok = !blob.is_null()
            && !text.is_null()
            && n == sqlite3_value_bytes(v)
            && std::slice::from_raw_parts(blob, n as usize)
                == std::slice::from_raw_parts(text, n as usize);
        sqlite3_result_int(ctx, if ok { n } else { -1 });
    }
    unsafe {
        let db = open_memory();
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"blob_len".as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                Some(len_fn),
                None,
                None,
                None,
            ),
            SQLITE_OK
        );
        let stmt = prepare(db, c"SELECT blob_len(?)");
        assert_eq!(
            sqlite3_bind_text(stmt, 1, c"h\xc3\xa9llo".as_ptr(), 6, SQLITE_STATIC),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(stmt, 0), 6);
        assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn version_and_directory_data_symbols() {
    unsafe {
        let v = CStr::from_ptr(ptr::addr_of!(sqlite3_version) as *const libc::c_char);
        assert_eq!(v, CStr::from_ptr(sqlite3_libversion()));
        assert!(sqlite3_temp_directory.is_null());
        assert!(sqlite3_data_directory.is_null());
    }
}
