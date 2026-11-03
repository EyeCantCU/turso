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
//! READ(1..4) can all be taken exclusively. SQLite writers stay locked out
//! (turso holds WAL_WRITE_LOCK shared), so turso is the only index writer.

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

pub(crate) struct SqliteWalIndex {
    shm: Arc<dyn File>,
    wal: std::fs::File,
    state: Mutex<State>,
}

struct State {
    pages: Vec<Box<dyn SharedWalMappedRegion>>,
    change: u32,
    /// This process's holders of each SQLite read mark: the kernel keeps
    /// one OFD lock per mark however many connections share it.
    read_holders: [u32; NREADER as usize],
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

/// SQLite's walChecksumBytes over native-order u32 pairs.
fn checksum_native(words: &[u32]) -> (u32, u32) {
    let (mut s1, mut s2) = (0u32, 0u32);
    for pair in words.chunks(2) {
        s1 = s1.wrapping_add(pair[0]).wrapping_add(s2);
        s2 = s2.wrapping_add(pair[1]).wrapping_add(s1);
    }
    (s1, s2)
}

impl SqliteWalIndex {
    pub(crate) fn new(shm: Arc<dyn File>, wal_path: &str) -> Result<Self> {
        let wal = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(wal_path)
            .map_err(|e| crate::error::io_error(e, "open WAL for wal-index"))?;
        Ok(Self {
            shm,
            wal,
            state: Mutex::new(State {
                pages: Vec::new(),
                change: 0,
                read_holders: [0; NREADER as usize],
            }),
        })
    }

    fn base(&self, state: &mut State, page: usize) -> Result<*mut u8> {
        while state.pages.len() <= page {
            let n = state.pages.len();
            let len = ((n + 1) * PAGE) as u64;
            if self.shm.size()? < len {
                self.shm.shared_wal_set_len(len)?;
            }
            state
                .pages
                .push(self.shm.shared_wal_map((n * PAGE) as u64, PAGE)?);
        }
        Ok(state.pages[page].ptr().as_ptr())
    }

    fn u32_at(base: *mut u8, offset: usize) -> &'static AtomicU32 {
        unsafe { &*(base.add(offset) as *const AtomicU32) }
    }

    /// Read the header the way walIndexTryHdr does: both copies equal,
    /// initialised and with a valid checksum.
    pub(crate) fn read_header(&self) -> Result<Option<IndexSnapshot>> {
        let mut state = self.state.lock();
        let base = self.base(&mut state, 0)?;
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
    pub(crate) fn frame_pages(
        &self,
        from: u32,
        to: u32,
        page_size: u32,
    ) -> Result<Vec<(u32, u32)>> {
        (from..=to)
            .map(|frame| {
                let hdr = self.read_frame_header(frame, page_size)?;
                Ok((frame, u32::from_be_bytes(hdr[0..4].try_into().unwrap())))
            })
            .collect()
    }

    /// WAL_WRITE_LOCK exclusively: the one SQLite or turso writer.
    pub(crate) fn try_lock_write(&self) -> Result<bool> {
        self.shm.shared_wal_try_lock_byte(WRITE_LOCK, true, KIND)
    }

    pub(crate) fn unlock_write(&self) {
        let _ = self.shm.shared_wal_unlock_byte(WRITE_LOCK, KIND);
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
        let mut state = self.state.lock();
        if max_frame <= nbackfills {
            // Reads only the database file: READ(0), which holds off
            // backfills but not WAL restarts.
            if state.read_holders[0] == 0
                && !self
                    .shm
                    .shared_wal_try_lock_byte(READ_LOCK_0, false, KIND)?
            {
                return Ok(None);
            }
            state.read_holders[0] += 1;
            return Ok(Some(0));
        }
        let max_frame = max_frame as u32;
        let base = self.base(&mut state, 0)?;
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
                    if state.read_holders[i] == 0
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
            if state.read_holders[i] == 0
                && !self
                    .shm
                    .shared_wal_try_lock_byte(READ_LOCK_0 + i as u64, false, KIND)?
            {
                continue;
            }
            state.read_holders[i] += 1;
            // The mark may have moved before our lock landed.
            if mark(i).load(Ordering::Acquire) <= max_frame {
                return Ok(Some(i));
            }
            Self::release_mark(&self.shm, &mut state, i);
        }
        Ok(None)
    }

    fn release_mark(shm: &Arc<dyn File>, state: &mut State, i: usize) {
        state.read_holders[i] -= 1;
        if state.read_holders[i] == 0 {
            let _ = shm.shared_wal_unlock_byte(READ_LOCK_0 + i as u64, KIND);
        }
    }

    pub(crate) fn release_read_mark(&self, i: usize) {
        let mut state = self.state.lock();
        Self::release_mark(&self.shm, &mut state, i);
    }

    /// Mark the header invalid so SQLite will not trust stale contents.
    pub(crate) fn invalidate(&self) -> Result<()> {
        let mut state = self.state.lock();
        let base = self.base(&mut state, 0)?;
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

    fn append(&self, state: &mut State, frame: u32, pgno: u32) -> Result<()> {
        let page = frame_page(frame);
        let base = self.base(state, page)?;
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
        while slot(k).load(Ordering::Relaxed) != 0 {
            k = (k + 1) & (NSLOT - 1);
        }
        unsafe { (pgnos as *mut u32).add(idx - 1).write_volatile(pgno) };
        slot(k).store(idx as u16, Ordering::Release);
        Ok(())
    }

    fn write_header(&self, state: &mut State, header: WalIndexHeader, n_page: u32) -> Result<()> {
        let base = self.base(state, 0)?;
        // iChange must advance whoever wrote last: SQLite readers compare it
        // to decide whether their page cache is still valid.
        state.change = Self::u32_at(base, 8)
            .load(Ordering::Relaxed)
            .wrapping_add(1);
        let size_field = ((header.page_size & 0xff00) | (header.page_size >> 16)) as u16;
        // isInit, bigEndCksum, then szPage as a native u16.
        let size = size_field.to_ne_bytes();
        let flags = u32::from_ne_bytes([1, header.big_endian_checksum as u8, size[0], size[1]]);
        let mut words = [
            3007000,
            0,
            state.change,
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
        let mut state = self.state.lock();
        let base = self.base(&mut state, 0)?;
        unsafe { std::ptr::write_bytes(base, 0, PAGE) };
        let mut n_page = 0;
        for frame in 1..=header.max_frame {
            let hdr = self.read_frame_header(frame, header.page_size)?;
            let pgno = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
            let commit = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
            if commit != 0 {
                n_page = commit;
            }
            self.append(&mut state, frame, pgno)?;
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
        self.write_header(&mut state, header, n_page)
    }

    /// Index the frames of a commit and publish the new header.
    pub(crate) fn commit(&self, header: WalIndexHeader) -> Result<()> {
        let mut state = self.state.lock();
        let base = self.base(&mut state, 0)?;
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
            self.append(
                &mut state,
                frame,
                u32::from_be_bytes(hdr[0..4].try_into().unwrap()),
            )?;
        }
        self.write_header(&mut state, header, n_page)
    }

    /// Lower `max_safe` below every frame a SQLite reader still needs from
    /// the WAL, retiring read marks nobody holds, like walCheckpoint.
    pub(crate) fn limit_backfill(&self, mut max_safe: u64) -> Result<u64> {
        let mut state = self.state.lock();
        let base = self.base(&mut state, 0)?;
        for i in 1..NREADER {
            let mark = Self::u32_at(base, CKPT_READMARK + i as usize * 4);
            let value = mark.load(Ordering::Acquire);
            if (value as u64) < max_safe {
                if state.read_holders[i as usize] == 0
                    && self
                        .shm
                        .shared_wal_try_lock_byte(READ_LOCK_0 + i, true, KIND)?
                {
                    mark.store(
                        if i == 1 {
                            max_safe as u32
                        } else {
                            READMARK_NOT_USED
                        },
                        Ordering::Release,
                    );
                    self.shm.shared_wal_unlock_byte(READ_LOCK_0 + i, KIND)?;
                } else {
                    max_safe = value as u64;
                }
            }
        }
        Ok(max_safe)
    }

    /// READ(0) exclusively, held while backfilling the database file.
    pub(crate) fn try_lock_read0(&self) -> Result<bool> {
        self.shm.shared_wal_try_lock_byte(READ_LOCK_0, true, KIND)
    }

    pub(crate) fn unlock_read0(&self) {
        let _ = self.shm.shared_wal_unlock_byte(READ_LOCK_0, KIND);
    }

    pub(crate) fn publish_backfill(&self, nbackfills: u64) -> Result<()> {
        let mut state = self.state.lock();
        let base = self.base(&mut state, 0)?;
        Self::u32_at(base, CKPT_NBACKFILL).store(nbackfills as u32, Ordering::Release);
        Self::u32_at(base, CKPT_NBACKFILL_ATTEMPTED).store(nbackfills as u32, Ordering::Release);
        Ok(())
    }

    /// Restart the index with the WAL, if no SQLite reader holds a mark.
    /// Returns false (and changes nothing) while one does.
    pub(crate) fn try_restart(&self, header: WalIndexHeader) -> Result<bool> {
        if self.state.lock().read_holders[1..].iter().any(|&n| n > 0) {
            return Ok(false);
        }
        let mut held = Vec::new();
        for i in 1..NREADER {
            if !self
                .shm
                .shared_wal_try_lock_byte(READ_LOCK_0 + i, true, KIND)?
            {
                for j in held {
                    self.shm.shared_wal_unlock_byte(READ_LOCK_0 + j, KIND)?;
                }
                return Ok(false);
            }
            held.push(i);
        }
        let result = (|| {
            let mut state = self.state.lock();
            let base = self.base(&mut state, 0)?;
            Self::u32_at(base, CKPT_NBACKFILL).store(0, Ordering::Release);
            Self::u32_at(base, CKPT_NBACKFILL_ATTEMPTED).store(0, Ordering::Release);
            Self::u32_at(base, CKPT_READMARK + 4).store(0, Ordering::Release);
            for i in 2..NREADER as usize {
                Self::u32_at(base, CKPT_READMARK + i * 4)
                    .store(READMARK_NOT_USED, Ordering::Release);
            }
            self.write_header(
                &mut state,
                WalIndexHeader {
                    max_frame: 0,
                    ..header
                },
                0,
            )
        })();
        for j in held {
            self.shm.shared_wal_unlock_byte(READ_LOCK_0 + j, KIND)?;
        }
        result.map(|_| true)
    }
}
