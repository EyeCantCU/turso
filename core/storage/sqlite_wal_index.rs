//! SQLite's wal-index (`-shm`), maintained by turso so that real SQLite
//! processes can read a WAL database while turso processes use it.
//!
//! Layout and protocol follow SQLite's wal.c: 32 KiB pages; page 0 starts
//! with two copies of the 48-byte `WalIndexHdr` and the 40-byte
//! `WalCkptInfo`; each page holds a page-number array and an 8192-slot u16
//! hash table at byte 16384. Turso indexes committed frames only, so every
//! entry is at or below `mxFrame`.
//!
//! SQLite readers pick a read mark (READ(i), bytes 123..127) and hold it
//! shared; turso checkpoints must not backfill past a held mark, must hold
//! READ(0) exclusively while backfilling, and may restart the WAL only when
//! READ(1..4) can all be taken exclusively. Turso and SQLite writers both
//! take WAL_WRITE_LOCK; turso imports SQLite's commits from this index.

use crate::io::{File, SharedWalLockKind, SharedWalMappedRegion};
use crate::sync::{Arc, Mutex};
use crate::Result;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{fence, AtomicU16, AtomicU32, Ordering};

const PAGE: usize = 32768;
const HDR_SIZE: usize = 136;
const NPAGE: u32 = 4096;
const NPAGE_ONE: u32 = NPAGE - (HDR_SIZE as u32 / 4);
const NSLOT: usize = 8192;
const HASH_OFFSET: usize = NPAGE as usize * 4;
const WRITE_LOCK: u64 = 120;
const CKPT_LOCK: u64 = 121;
const RECOVER_LOCK: u64 = 122;
const READ_LOCK_0: u64 = 123;
const NREADER: u64 = 5;
const READMARK_NOT_USED: u32 = 0xffff_ffff;
const WAL_HEADER_SIZE: u64 = 32;
const FRAME_HEADER_SIZE: u64 = 24;
const KIND: SharedWalLockKind = SharedWalLockKind::LinuxOfd;
// WalCkptInfo offsets.
const CKPT_NBACKFILL: usize = 96;
const CKPT_READMARK: usize = 100;
const CKPT_NBACKFILL_ATTEMPTED: usize = 128;

/// WAL header fields the index header mirrors.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WalIndexHeader {
    pub max_frame: u32,
    pub page_size: u32,
    pub salt_1: u32,
    pub salt_2: u32,
    pub checksum: (u32, u32),
    pub big_endian_checksum: bool,
}

impl WalIndexHeader {
    pub(crate) fn from_wal_header(
        header: &crate::storage::sqlite3_ondisk::WalHeader,
        max_frame: u64,
        checksum: (u32, u32),
    ) -> Self {
        Self {
            max_frame: max_frame as u32,
            page_size: header.page_size,
            salt_1: header.salt_1,
            salt_2: header.salt_2,
            checksum,
            big_endian_checksum: header.magic & 1 == 1,
        }
    }
}

pub(crate) struct SqliteWalIndex {
    shm: Arc<dyn File>,
    wal: WalReader,
    /// Mapped 32 KiB pages; held only while mapping.
    pages: Mutex<Vec<Box<dyn SharedWalMappedRegion>>>,
    /// Serializes index writes in this process.
    writes: Mutex<()>,
    /// Serializes read-mark changes in this process. Reentrant and never
    /// held with other state: VFS lock callbacks may use this database again
    /// on the same thread.
    marks: parking_lot::ReentrantMutex<()>,
    /// This process's holders of each SQLite read mark: the kernel keeps
    /// one OFD lock per mark however many connections share it.
    read_holders: [AtomicU32; NREADER as usize],
}

/// A consistent copy of the index header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexSnapshot {
    pub change: u32,
    pub max_frame: u32,
    pub n_page: u32,
    pub checksum: (u32, u32),
    pub salt_1: u32,
    pub salt_2: u32,
}

/// Fields of a WAL file header.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WalFileHeader {
    pub magic: u32,
    pub page_size: u32,
    pub checkpoint_seq: u32,
    pub salt_1: u32,
    pub salt_2: u32,
    pub checksum: (u32, u32),
}

fn frame_page(frame: u32) -> usize {
    ((frame + NPAGE - NPAGE_ONE - 1) / NPAGE) as usize
}

fn hash(pgno: u32) -> usize {
    (pgno.wrapping_mul(383) as usize) & (NSLOT - 1)
}

/// walChecksumBytes over `data` as big- or little-endian u32 pairs.
fn wal_checksum(big_endian: bool, data: &[u8], (mut s1, mut s2): (u32, u32)) -> (u32, u32) {
    for pair in data.chunks_exact(8) {
        let word = |b: &[u8]| {
            let b: [u8; 4] = b.try_into().unwrap();
            if big_endian {
                u32::from_be_bytes(b)
            } else {
                u32::from_le_bytes(b)
            }
        };
        s1 = s1.wrapping_add(word(&pair[0..4])).wrapping_add(s2);
        s2 = s2.wrapping_add(word(&pair[4..8])).wrapping_add(s1);
    }
    (s1, s2)
}

/// SQLite's walChecksumBytes over native-order u32 pairs.
fn checksum_native(words: &[u32]) -> (u32, u32) {
    let (mut s1, mut s2) = (0u32, 0u32);
    for pair in words.chunks(2) {
        s1 = s1.wrapping_add(pair[0]).wrapping_add(s2);
        s2 = s2.wrapping_add(pair[1]).wrapping_add(s1);
    }
    (s1, s2)
}

/// Where the index reads WAL frame headers from: the WAL's own descriptor
/// when the database has it open, else a file opened by path.
pub(crate) struct WalSource {
    pub path: String,
    pub shared: Option<(Arc<dyn File>, Arc<dyn crate::io::IO>)>,
}

enum WalReader {
    Shared(Arc<dyn File>, Arc<dyn crate::io::IO>),
    Host(std::fs::File),
}

impl WalReader {
    fn len(&self) -> std::io::Result<u64> {
        match self {
            WalReader::Shared(file, _) => file.size().map_err(std::io::Error::other),
            WalReader::Host(file) => file.metadata().map(|m| m.len()),
        }
    }

    fn read_exact_at(&self, out: &mut [u8], offset: u64) -> std::io::Result<()> {
        let (file, io) = match self {
            WalReader::Host(file) => return file.read_exact_at(out, offset),
            WalReader::Shared(file, io) => (file, io),
        };
        let buf = Arc::new(crate::Buffer::new_temporary(out.len()));
        let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = {
            let read = read.clone();
            crate::Completion::new_read(
                buf.clone(),
                move |res: std::result::Result<
                    (Arc<crate::Buffer>, i32),
                    crate::CompletionError,
                >| {
                    if let Ok((_buf, n)) = res {
                        read.store(n.max(0) as usize, Ordering::Release);
                    }
                    None
                },
            )
        };
        let c = file.pread(offset, c).map_err(std::io::Error::other)?;
        io.wait_for_completion(c).map_err(std::io::Error::other)?;
        if read.load(Ordering::Acquire) < out.len() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        out.copy_from_slice(&buf.as_slice()[..out.len()]);
        Ok(())
    }
}

impl SqliteWalIndex {
    pub(crate) fn new(shm: Arc<dyn File>, wal: &WalSource) -> Result<Self> {
        let wal = match &wal.shared {
            Some((file, io)) => WalReader::Shared(file.clone(), io.clone()),
            None => WalReader::Host(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&wal.path)
                    .or_else(|_| std::fs::File::open(&wal.path))
                    .map_err(|e| crate::error::io_error(e, "open WAL for wal-index"))?,
            ),
        };
        Ok(Self {
            shm,
            wal,
            pages: Mutex::new(Vec::new()),
            writes: Mutex::new(()),
            marks: parking_lot::ReentrantMutex::new(()),
            read_holders: Default::default(),
        })
    }

    fn base(&self, page: usize) -> Result<*mut u8> {
        let mut pages = self.pages.lock();
        while pages.len() <= page {
            let n = pages.len();
            let len = ((n + 1) * PAGE) as u64;
            if self.shm.size()? < len {
                self.shm.shared_wal_set_len(len)?;
            }
            pages.push(self.shm.shared_wal_map((n * PAGE) as u64, PAGE)?);
        }
        Ok(pages[page].ptr().as_ptr())
    }

    fn holders(&self, i: usize) -> &AtomicU32 {
        &self.read_holders[i]
    }

    fn u32_at(base: *mut u8, offset: usize) -> &'static AtomicU32 {
        unsafe { &*(base.add(offset) as *const AtomicU32) }
    }

    /// Read the header the way walIndexTryHdr does: both copies equal,
    /// initialised and with a valid checksum.
    pub(crate) fn read_header(&self) -> Result<Option<IndexSnapshot>> {
        let base = self.base(0)?;
        let mut copies = [[0u32; 12]; 2];
        for (copy, words) in copies.iter_mut().enumerate() {
            for (i, w) in words.iter_mut().enumerate() {
                *w = Self::u32_at(base, copy * 48 + i * 4).load(Ordering::Relaxed);
            }
            fence(Ordering::SeqCst);
        }
        let w = copies[0];
        if copies[0] != copies[1] || w[3].to_ne_bytes()[0] == 0 {
            return Ok(None);
        }
        if checksum_native(&w[..10]) != (w[10], w[11]) {
            return Ok(None);
        }
        Ok(Some(IndexSnapshot {
            change: w[2],
            max_frame: w[4],
            n_page: w[5],
            checksum: (w[6], w[7]),
            salt_1: u32::from_be_bytes(w[8].to_ne_bytes()),
            salt_2: u32::from_be_bytes(w[9].to_ne_bytes()),
        }))
    }

    /// Whether a valid index describes the WAL file as it is on disk: same
    /// generation (salts) and every indexed frame present.
    pub(crate) fn matches_wal_file(&self) -> bool {
        let (Ok(Some(index)), Some(wal)) = (self.read_header(), self.read_wal_header()) else {
            return false;
        };
        let frame_size = u64::from(wal.page_size) + FRAME_HEADER_SIZE;
        let needed = WAL_HEADER_SIZE + u64::from(index.max_frame) * frame_size;
        index.salt_1 == wal.salt_1
            && index.salt_2 == wal.salt_2
            && self.wal.len().is_ok_and(|len| len >= needed)
    }

    pub(crate) fn read_wal_header(&self) -> Option<WalFileHeader> {
        let mut buf = [0u8; 32];
        self.wal.read_exact_at(&mut buf, 0).ok()?;
        let be = |i: usize| u32::from_be_bytes(buf[i..i + 4].try_into().unwrap());
        Some(WalFileHeader {
            magic: be(0),
            page_size: be(8),
            checkpoint_seq: be(12),
            salt_1: be(16),
            salt_2: be(20),
            checksum: (be(24), be(28)),
        })
    }

    /// (frame, page) for each frame in `from..=to`, from the WAL frame headers.
    /// Fails with SQLITE_BUSY if a frame carries other salts than `salts`.
    pub(crate) fn frame_pages(
        &self,
        from: u32,
        to: u32,
        page_size: u32,
        salts: (u32, u32),
    ) -> Result<Vec<(u32, u32)>> {
        let be = |b: &[u8]| u32::from_be_bytes(b.try_into().unwrap());
        (from..=to)
            .map(|frame| {
                let hdr = self.read_frame_header(frame, page_size)?;
                if (be(&hdr[8..12]), be(&hdr[12..16])) != salts {
                    return Err(crate::LimboError::Busy);
                }
                Ok((frame, be(&hdr[0..4])))
            })
            .collect()
    }

    /// WalCkptInfo.nBackfill.
    pub(crate) fn backfill(&self) -> Result<u32> {
        let base = self.base(0)?;
        Ok(Self::u32_at(base, CKPT_NBACKFILL).load(Ordering::Acquire))
    }

    /// Read the header and hold a read mark in 1..=4 so that no WAL restart
    /// can overwrite its frames, re-reading until both agree, as
    /// walTryBeginRead does. The mark is None for an empty WAL; Ok(None)
    /// means try again later.
    pub(crate) fn pin_header(&self) -> Result<Option<(Option<usize>, IndexSnapshot)>> {
        for _ in 0..8 {
            let Some(header) = self.read_header()? else {
                return Ok(None);
            };
            if header.max_frame == 0 {
                return Ok(Some((None, header)));
            }
            let Some(mark) = self.acquire_read_mark(header.max_frame as u64, 0)? else {
                return Ok(None);
            };
            if self.read_header()? == Some(header) {
                return Ok(Some((Some(mark), header)));
            }
            self.release_read_mark(mark);
        }
        Ok(None)
    }

    /// WAL_WRITE_LOCK exclusively: the one SQLite or turso writer.
    pub(crate) fn try_lock_write(&self) -> Result<bool> {
        self.shm.shared_wal_try_lock_byte(WRITE_LOCK, true, KIND)
    }

    pub(crate) fn unlock_write(&self) {
        let _ = self.shm.shared_wal_unlock_byte(WRITE_LOCK, KIND);
    }

    /// WAL_RECOVER_LOCK exclusively, held while rebuilding the index.
    pub(crate) fn try_lock_recover(&self) -> Result<bool> {
        self.shm.shared_wal_try_lock_byte(RECOVER_LOCK, true, KIND)
    }

    pub(crate) fn unlock_recover(&self) {
        let _ = self.shm.shared_wal_unlock_byte(RECOVER_LOCK, KIND);
    }

    /// Rebuild an invalid index from the WAL file, as walIndexRecover does:
    /// keep frames up to the last commit frame whose salts and cumulative
    /// checksum are valid. The caller holds WAL_WRITE_LOCK, WAL_CKPT_LOCK and
    /// WAL_RECOVER_LOCK. Returns the new header.
    pub(crate) fn recover(&self) -> Result<IndexSnapshot> {
        let len = self
            .wal
            .len()
            .map_err(|e| crate::error::io_error(e, "stat WAL for wal-index"))?;
        let mut header = WalIndexHeader {
            max_frame: 0,
            page_size: 4096,
            salt_1: 0,
            salt_2: 0,
            checksum: (0, 0),
            big_endian_checksum: false,
        };
        let mut hdr = [0u8; 32];
        let be = |b: &[u8]| u32::from_be_bytes(b.try_into().unwrap());
        if len >= WAL_HEADER_SIZE && self.wal.read_exact_at(&mut hdr, 0).is_ok() {
            let magic = be(&hdr[0..4]);
            let page_size = be(&hdr[8..12]);
            let valid_size = (512..=65536).contains(&page_size) && page_size.is_power_of_two();
            if (magic & !1) == 0x377f_0682 && be(&hdr[4..8]) == 3007000 && valid_size {
                let big = magic & 1 == 1;
                let sum = wal_checksum(big, &hdr[..24], (0, 0));
                if sum == (be(&hdr[24..28]), be(&hdr[28..32])) {
                    header.page_size = page_size;
                    header.big_endian_checksum = big;
                    header.salt_1 = be(&hdr[16..20]);
                    header.salt_2 = be(&hdr[20..24]);
                    header.checksum = sum;
                    let frame_len = FRAME_HEADER_SIZE + page_size as u64;
                    let mut frame = vec![0u8; frame_len as usize];
                    let mut running = sum;
                    let mut n = 0u32;
                    while WAL_HEADER_SIZE + (n as u64 + 1) * frame_len <= len {
                        let offset = WAL_HEADER_SIZE + n as u64 * frame_len;
                        if self.wal.read_exact_at(&mut frame, offset).is_err()
                            || (be(&frame[8..12]), be(&frame[12..16]))
                                != (header.salt_1, header.salt_2)
                            || be(&frame[0..4]) == 0
                        {
                            break;
                        }
                        let s = wal_checksum(big, &frame[..8], running);
                        let s = wal_checksum(big, &frame[24..], s);
                        if s != (be(&frame[16..20]), be(&frame[20..24])) {
                            break;
                        }
                        running = s;
                        n += 1;
                        if be(&frame[4..8]) != 0 {
                            header.max_frame = n;
                            header.checksum = s;
                        }
                    }
                }
            }
        }
        self.rebuild(header, 0)?;
        self.read_header()?
            .ok_or_else(|| crate::LimboError::InternalError("wal-index recovery failed".into()))
    }

    /// WAL_CKPT_LOCK exclusively: the one SQLite or turso checkpointer.
    pub(crate) fn try_lock_ckpt(&self) -> Result<bool> {
        self.shm.shared_wal_try_lock_byte(CKPT_LOCK, true, KIND)
    }

    pub(crate) fn unlock_ckpt(&self) {
        let _ = self.shm.shared_wal_unlock_byte(CKPT_LOCK, KIND);
    }

    /// Pin frames up to `max_frame` against SQLite checkpoints and restarts
    /// with a read mark in 1..=4 at or below it, as walTryBeginRead does.
    /// None if no mark can be had right now.
    pub(crate) fn acquire_read_mark(
        &self,
        max_frame: u64,
        nbackfills: u64,
    ) -> Result<Option<usize>> {
        let _marks = self.marks.lock();
        if max_frame <= nbackfills {
            // Reads only the database file: READ(0), which holds off
            // backfills but not WAL restarts.
            if self.holders(0).load(Ordering::Acquire) == 0
                && !self
                    .shm
                    .shared_wal_try_lock_byte(READ_LOCK_0, false, KIND)?
            {
                return Ok(None);
            }
            self.holders(0).fetch_add(1, Ordering::AcqRel);
            return Ok(Some(0));
        }
        let max_frame = max_frame as u32;
        let base = self.base(0)?;
        let mark = |i: usize| Self::u32_at(base, CKPT_READMARK + i * 4);
        for _ in 0..8 {
            let mut best: Option<usize> = None;
            for i in 1..NREADER as usize {
                let m = mark(i).load(Ordering::Acquire);
                if m <= max_frame && best.is_none_or(|b| m > mark(b).load(Ordering::Acquire)) {
                    best = Some(i);
                }
            }
            if best.is_none_or(|b| mark(b).load(Ordering::Acquire) < max_frame) {
                // Point a mark nobody holds at our frame.
                for i in 1..NREADER as usize {
                    if self.holders(i).load(Ordering::Acquire) == 0
                        && self
                            .shm
                            .shared_wal_try_lock_byte(READ_LOCK_0 + i as u64, true, KIND)?
                    {
                        mark(i).store(max_frame, Ordering::Release);
                        self.shm
                            .shared_wal_unlock_byte(READ_LOCK_0 + i as u64, KIND)?;
                        best = Some(i);
                        break;
                    }
                }
            }
            let Some(i) = best else {
                return Ok(None);
            };
            if self.holders(i).load(Ordering::Acquire) == 0
                && !self
                    .shm
                    .shared_wal_try_lock_byte(READ_LOCK_0 + i as u64, false, KIND)?
            {
                continue;
            }
            self.holders(i).fetch_add(1, Ordering::AcqRel);
            // The mark may have moved before our lock landed.
            if mark(i).load(Ordering::Acquire) <= max_frame {
                return Ok(Some(i));
            }
            self.release_read_mark(i);
        }
        Ok(None)
    }

    pub(crate) fn release_read_mark(&self, i: usize) {
        let _marks = self.marks.lock();
        if self.holders(i).fetch_sub(1, Ordering::AcqRel) == 1 {
            let _ = self
                .shm
                .shared_wal_unlock_byte(READ_LOCK_0 + i as u64, KIND);
        }
    }

    /// Mark the header invalid so SQLite will not trust stale contents.
    pub(crate) fn invalidate(&self) -> Result<()> {
        let _writes = self.writes.lock();
        let base = self.base(0)?;
        unsafe { std::ptr::write_bytes(base, 0, HDR_SIZE) };
        fence(Ordering::SeqCst);
        Ok(())
    }

    fn read_frame_header(&self, frame: u32, page_size: u32) -> Result<[u8; 24]> {
        let mut buf = [0u8; 24];
        let offset = WAL_HEADER_SIZE + (frame as u64 - 1) * (FRAME_HEADER_SIZE + page_size as u64);
        self.wal
            .read_exact_at(&mut buf, offset)
            .map_err(|e| crate::error::io_error(e, "read WAL frame header"))?;
        Ok(buf)
    }

    fn append(&self, frame: u32, pgno: u32) -> Result<()> {
        let page = frame_page(frame);
        let base = self.base(page)?;
        let (pgnos, zero) = if page == 0 {
            (unsafe { base.add(HDR_SIZE) }, 0)
        } else {
            (base, NPAGE_ONE + (page as u32 - 1) * NPAGE)
        };
        let idx = (frame - zero) as usize;
        let hashes = unsafe { base.add(HASH_OFFSET) };
        if idx == 1 {
            let len = unsafe { base.add(PAGE).offset_from(pgnos) } as usize;
            unsafe { std::ptr::write_bytes(pgnos, 0, len) };
        }
        let slot = |k: usize| unsafe { &*(hashes.add(k * 2) as *const AtomicU16) };
        let mut k = hash(pgno);
        let mut probes = 0;
        while slot(k).load(Ordering::Relaxed) != 0 {
            // A full table is corrupt, as walIndexAppend's nCollide check says.
            probes += 1;
            if probes >= NSLOT {
                return Err(crate::LimboError::Corrupt(
                    "SQLite wal-index hash table is full".into(),
                ));
            }
            k = (k + 1) & (NSLOT - 1);
        }
        unsafe { (pgnos as *mut u32).add(idx - 1).write_volatile(pgno) };
        slot(k).store(idx as u16, Ordering::Release);
        Ok(())
    }

    fn write_header(&self, header: WalIndexHeader, n_page: u32) -> Result<()> {
        let base = self.base(0)?;
        // iChange must advance whoever wrote last: SQLite readers compare it
        // to decide whether their page cache is still valid.
        let change = Self::u32_at(base, 8)
            .load(Ordering::Relaxed)
            .wrapping_add(1);
        let size_field = ((header.page_size & 0xff00) | (header.page_size >> 16)) as u16;
        // isInit, bigEndCksum, then szPage as a native u16.
        let size = size_field.to_ne_bytes();
        let flags = u32::from_ne_bytes([1, header.big_endian_checksum as u8, size[0], size[1]]);
        let mut words = [
            3007000,
            0,
            change,
            flags,
            header.max_frame,
            n_page,
            header.checksum.0,
            header.checksum.1,
            u32::from_ne_bytes(header.salt_1.to_be_bytes()),
            u32::from_ne_bytes(header.salt_2.to_be_bytes()),
            0,
            0,
        ];
        let (c1, c2) = checksum_native(&words[..10]);
        words[10] = c1;
        words[11] = c2;
        for copy in [1, 0] {
            for (i, w) in words.iter().enumerate() {
                Self::u32_at(base, copy * 48 + i * 4).store(*w, Ordering::Relaxed);
            }
            fence(Ordering::SeqCst);
        }
        Ok(())
    }

    /// Rebuild the whole index for the committed WAL (first turso process).
    pub(crate) fn rebuild(&self, header: WalIndexHeader, nbackfills: u32) -> Result<()> {
        let writes = self.writes.lock();
        let base = self.base(0)?;
        unsafe { std::ptr::write_bytes(base, 0, PAGE) };
        let mut n_page = 0;
        for frame in 1..=header.max_frame {
            let hdr = self.read_frame_header(frame, header.page_size)?;
            let pgno = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
            let commit = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
            if commit != 0 {
                n_page = commit;
            }
            self.append(frame, pgno)?;
        }
        Self::u32_at(base, CKPT_NBACKFILL).store(nbackfills, Ordering::Relaxed);
        Self::u32_at(base, CKPT_NBACKFILL_ATTEMPTED).store(nbackfills, Ordering::Relaxed);
        for i in 0..NREADER as usize {
            let mark = match i {
                0 => 0,
                1 if header.max_frame > 0 => header.max_frame,
                _ => READMARK_NOT_USED,
            };
            Self::u32_at(base, CKPT_READMARK + i * 4).store(mark, Ordering::Relaxed);
        }
        let result = self.write_header(header, n_page);
        drop(writes);
        self.shm.shared_wal_barrier();
        result
    }

    /// Index the frames of a commit and publish the new header.
    pub(crate) fn commit(&self, header: WalIndexHeader) -> Result<()> {
        let writes = self.writes.lock();
        let base = self.base(0)?;
        let previous = Self::u32_at(base, 16).load(Ordering::Relaxed);
        let previous = if previous > header.max_frame {
            0
        } else {
            previous
        };
        let mut n_page = Self::u32_at(base, 20).load(Ordering::Relaxed);
        for frame in previous + 1..=header.max_frame {
            let hdr = self.read_frame_header(frame, header.page_size)?;
            let commit = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
            if commit != 0 {
                n_page = commit;
            }
            self.append(frame, u32::from_be_bytes(hdr[0..4].try_into().unwrap()))?;
        }
        let result = self.write_header(header, n_page);
        drop(writes);
        // The VFS's xShmBarrier, outside the write lock: a VFS callback may
        // re-enter this database (the header copies are fenced already).
        self.shm.shared_wal_barrier();
        result
    }

    /// Lower `max_safe` below every frame a SQLite reader still needs from
    /// the WAL, retiring read marks nobody holds, like walCheckpoint.
    pub(crate) fn limit_backfill(&self, mut max_safe: u64) -> Result<u64> {
        let _marks = self.marks.lock();
        let base = self.base(0)?;
        for i in 1..NREADER {
            let mark = Self::u32_at(base, CKPT_READMARK + i as usize * 4);
            let value = mark.load(Ordering::Acquire);
            if (value as u64) < max_safe {
                if self.exclusive(i as usize)? {
                    // A mark this process holds keeps its value: SQLite
                    // checkpointers must not backfill past our readers.
                    if self.holders(i as usize).load(Ordering::Acquire) == 0 {
                        mark.store(
                            if i == 1 {
                                max_safe as u32
                            } else {
                                READMARK_NOT_USED
                            },
                            Ordering::Release,
                        );
                    }
                    self.unexclusive(i as usize)?;
                } else {
                    max_safe = value as u64;
                }
            }
        }
        Ok(max_safe)
    }

    /// Take READ(i) exclusively. The kernel keeps one lock per mark for the
    /// whole process, so when this process holds it shared this converts the
    /// lock and succeeds exactly when no other process holds it.
    fn exclusive(&self, i: usize) -> Result<bool> {
        self.shm
            .shared_wal_try_lock_byte(READ_LOCK_0 + i as u64, true, KIND)
    }

    /// Undo [`Self::exclusive`], back to shared while this process's readers
    /// still hold the mark.
    fn unexclusive(&self, i: usize) -> Result<()> {
        if self.holders(i).load(Ordering::Acquire) > 0 {
            self.shm
                .shared_wal_lock_byte(READ_LOCK_0 + i as u64, false, KIND)
        } else {
            self.shm
                .shared_wal_unlock_byte(READ_LOCK_0 + i as u64, KIND)
        }
    }

    /// READ(0) exclusively, held while backfilling the database file.
    pub(crate) fn try_lock_read0(&self) -> Result<bool> {
        let _marks = self.marks.lock();
        self.exclusive(0)
    }

    pub(crate) fn unlock_read0(&self) {
        let _marks = self.marks.lock();
        let _ = self.unexclusive(0);
    }

    pub(crate) fn publish_backfill(&self, nbackfills: u64) -> Result<()> {
        let base = self.base(0)?;
        Self::u32_at(base, CKPT_NBACKFILL).store(nbackfills as u32, Ordering::Release);
        Self::u32_at(base, CKPT_NBACKFILL_ATTEMPTED).store(nbackfills as u32, Ordering::Release);
        Ok(())
    }

    /// Restart the index with the WAL, if no SQLite reader holds a mark.
    /// Returns false (and changes nothing) while one does.
    pub(crate) fn try_restart(&self, header: WalIndexHeader) -> Result<bool> {
        let _marks = self.marks.lock();
        let mut held = Vec::new();
        for i in 1..NREADER as usize {
            if !self.exclusive(i)? {
                for j in held {
                    self.unexclusive(j)?;
                }
                return Ok(false);
            }
            held.push(i);
        }
        let result = (|| {
            let _writes = self.writes.lock();
            let base = self.base(0)?;
            Self::u32_at(base, CKPT_NBACKFILL).store(0, Ordering::Release);
            Self::u32_at(base, CKPT_NBACKFILL_ATTEMPTED).store(0, Ordering::Release);
            Self::u32_at(base, CKPT_READMARK + 4).store(0, Ordering::Release);
            for i in 2..NREADER as usize {
                Self::u32_at(base, CKPT_READMARK + i * 4)
                    .store(READMARK_NOT_USED, Ordering::Release);
            }
            self.write_header(
                WalIndexHeader {
                    max_frame: 0,
                    ..header
                },
                0,
            )
        })();
        self.shm.shared_wal_barrier();
        for j in held {
            self.unexclusive(j)?;
        }
        result.map(|_| true)
    }
}
