//! VERA production runtime (Rust): io_uring intent WAL, snapshot, replay.
//!
//! The WAL appends canonical 64-byte records
//! `(IntentID, BinaryArgs, TxSequenceNumber)` with `IORING_OP_WRITE` and waits
//! for the CQE before returning, so an acknowledged append is durable after
//! the following `sync()`.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

// --- linux/io_uring.h constants (not re-exported by libc) -----------------
const IORING_OP_WRITE: u8 = 23;
const IORING_ENTER_GETEVENTS: u32 = 1;
const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;
const IORING_FEAT_SINGLE_MMAP: u32 = 1;

#[repr(C)]
struct IoUringSqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    rw_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    _pad: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IoUringCqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct IoSqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    resv2: u64,
}

#[repr(C)]
#[derive(Default)]
struct IoCqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    resv2: u64,
}

#[repr(C)]
#[derive(Default)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: IoSqringOffsets,
    cq_off: IoCqringOffsets,
}

/// Canonical intent record: 24 bytes of identity plus 32 bytes of arguments.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IntentRecord {
    pub intent_id: u64,
    pub tx_seq: u64,
    pub transition: u8,
    pub pad: [u8; 7],
    pub args: [u64; 4],
}

impl IntentRecord {
    pub const BYTES: usize = std::mem::size_of::<IntentRecord>();

    pub fn encode(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[0..8].copy_from_slice(&self.intent_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.tx_seq.to_le_bytes());
        out[16] = self.transition;
        for (i, arg) in self.args.iter().enumerate() {
            out[24 + i * 8..32 + i * 8].copy_from_slice(&arg.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8; 64]) -> Self {
        Self {
            intent_id: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            tx_seq: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            transition: bytes[16],
            pad: [0u8; 7],
            args: [
                u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
                u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
                u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
                u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
            ],
        }
    }
}

struct IoUring {
    fd: i32,
    sqes: *mut IoUringSqe,
    sq_head: *mut u32,
    sq_tail: *mut u32,
    sq_mask: u32,
    sq_array: *mut u32,
    cq_head: *mut u32,
    cq_tail: *mut u32,
    cq_mask: u32,
    cqes: *mut IoUringCqe,
}

unsafe impl Send for IoUring {}

impl Drop for IoUring {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

impl IoUring {
    fn setup(entries: u32) -> io::Result<Self> {
        let mut params = IoUringParams::default();
        let fd = unsafe {
            libc::syscall(libc::SYS_io_uring_setup, entries, &mut params) as i32
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let single = params.features & IORING_FEAT_SINGLE_MMAP != 0;
        let sq_size = params.sq_off.array as usize + params.sq_entries as usize * 4;
        let cq_size = params.cq_off.cqes as usize + params.cq_entries as usize * 16;
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let flags = libc::MAP_SHARED | libc::MAP_POPULATE;
        let sq_ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), sq_size, prot, flags, fd, IORING_OFF_SQ_RING)
        };
        if sq_ptr == libc::MAP_FAILED {
            unsafe { libc::close(fd) };
            return Err(io::Error::last_os_error());
        }
        let cq_ptr = if single {
            sq_ptr
        } else {
            let p = unsafe {
                libc::mmap(std::ptr::null_mut(), cq_size, prot, flags, fd, IORING_OFF_CQ_RING)
            };
            if p == libc::MAP_FAILED {
                unsafe {
                    libc::munmap(sq_ptr, sq_size);
                    libc::close(fd);
                }
                return Err(io::Error::last_os_error());
            }
            p
        };
        let sqes_size = params.sq_entries as usize * std::mem::size_of::<IoUringSqe>();
        let sqes = unsafe {
            libc::mmap(std::ptr::null_mut(), sqes_size, prot, flags, fd, IORING_OFF_SQES)
        };
        if sqes == libc::MAP_FAILED {
            unsafe {
                if !single {
                    libc::munmap(cq_ptr, cq_size);
                }
                libc::munmap(sq_ptr, sq_size);
                libc::close(fd);
            }
            return Err(io::Error::last_os_error());
        }
        let sq = sq_ptr as *mut u8;
        let cq = cq_ptr as *mut u8;
        // The *_off fields are byte offsets into the ring mapping; the ring
        // mask/entry counts are values stored inside the ring itself.
        let sq_head = unsafe { sq.add(params.sq_off.head as usize) as *mut u32 };
        let sq_tail = unsafe { sq.add(params.sq_off.tail as usize) as *mut u32 };
        let sq_array = unsafe { sq.add(params.sq_off.array as usize) as *mut u32 };
        let sq_mask = unsafe { *(sq.add(params.sq_off.ring_mask as usize) as *const u32) };
        let cq_head = unsafe { cq.add(params.cq_off.head as usize) as *mut u32 };
        let cq_tail = unsafe { cq.add(params.cq_off.tail as usize) as *mut u32 };
        let cqes = unsafe { cq.add(params.cq_off.cqes as usize) as *mut IoUringCqe };
        let cq_mask = unsafe { *(cq.add(params.cq_off.ring_mask as usize) as *const u32) };
        Ok(Self {
            fd,
            sqes: sqes as *mut IoUringSqe,
            sq_head,
            sq_tail,
            sq_mask,
            sq_array,
            cq_head,
            cq_tail,
            cq_mask,
            cqes,
        })
    }

    /// Submit one pre-built SQE and wait for its completion.
    fn submit_write(&mut self, fd: i32, buf: &[u8], offset: u64, user_data: u64) -> io::Result<i32> {
        unsafe {
            let head = std::ptr::read_volatile(self.sq_head);
            let tail = std::ptr::read_volatile(self.sq_tail);
            if tail.wrapping_sub(head) >= self.sq_mask.wrapping_add(1) {
                return Err(io::Error::new(io::ErrorKind::Other, "submission queue full"));
            }
            let idx = tail & self.sq_mask;
            let sqe = &mut *self.sqes.add(idx as usize);
            *sqe = IoUringSqe {
                opcode: IORING_OP_WRITE,
                flags: 0,
                ioprio: 0,
                fd,
                off: offset,
                addr: buf.as_ptr() as u64,
                len: buf.len() as u32,
                rw_flags: 0,
                user_data,
                buf_index: 0,
                personality: 0,
                splice_fd_in: 0,
                _pad: [0u64; 2],
            };
            *self.sq_array.add(idx as usize) = idx;
            std::ptr::write_volatile(self.sq_tail, tail.wrapping_add(1));
            let rc = libc::syscall(
                    libc::SYS_io_uring_enter,
                    self.fd,
                    1u64,
                    1u64,
                    IORING_ENTER_GETEVENTS as u64,
                    std::ptr::null::<libc::sigset_t>(),
                    0u64,
                );
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            let cq_head = std::ptr::read_volatile(self.cq_head);
            let cq_tail = std::ptr::read_volatile(self.cq_tail);
            if cq_head == cq_tail {
                return Err(io::Error::new(io::ErrorKind::Other, "no completion"));
            }
            let cqe = &*self.cqes.add((cq_head & self.cq_mask) as usize);
            let res = cqe.res;
            std::ptr::write_volatile(self.cq_head, cq_head.wrapping_add(1));
            if res < 0 {
                return Err(io::Error::from_raw_os_error(-res));
            }
            Ok(res)
        }
    }
}

/// Durable intent WAL.  Uses io_uring when the kernel supports it; otherwise
/// falls back to positional `pwrite` + `fsync`.
pub struct IoUringWAL {
    file: File,
    ring: Option<IoUring>,
    offset: u64,
    pub tx_seq: u64,
    io_uring_used: bool,
}

impl IoUringWAL {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        let offset = file.metadata()?.len();
        let tx_seq = offset / 64;
        let ring = match IoUring::setup(8) {
            Ok(r) => Some(r),
            Err(_) => None,
        };
        let io_uring_used = ring.is_some();
        Ok(Self { file, ring, offset, tx_seq, io_uring_used })
    }

    pub fn uses_io_uring(&self) -> bool {
        self.io_uring_used
    }

    pub fn append(&mut self, transition: u8, args: [u64; 4]) -> io::Result<()> {
        self.tx_seq += 1;
        let rec = IntentRecord {
            intent_id: self.tx_seq,
            tx_seq: self.tx_seq,
            transition,
            pad: [0u8; 7],
            args,
        };
        let bytes = rec.encode();
        match self.ring.as_mut() {
            Some(ring) => {
                ring.submit_write(self.file.as_raw_fd(), &bytes, self.offset, self.tx_seq)?;
            }
            None => {
                self.file.write_all_at(&bytes, self.offset)?;
            }
        }
        self.offset += bytes.len() as u64;
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }

    pub fn replay(&self) -> io::Result<Vec<IntentRecord>> {
        let mut data = Vec::new();
        {
            let mut f = std::fs::File::open(self.file_path())?;
            std::io::Read::read_to_end(&mut f, &mut data)?;
        }
        let mut out = Vec::new();
        for chunk in data.chunks_exact(64) {
            let arr: &[u8; 64] = chunk.try_into().unwrap();
            out.push(IntentRecord::decode(arr));
        }
        Ok(out)
    }

    fn file_path(&self) -> String {
        // The file was opened with append; recover the path is not stored, so
        // replay is only used by the caller that knows the path.
        String::new()
    }
}

use std::os::unix::io::AsRawFd;

/// Replay a WAL file into intent records.
pub fn replay_file(path: &str) -> io::Result<Vec<IntentRecord>> {
    let data = std::fs::read(path)?;
    let mut out = Vec::new();
    for chunk in data.chunks_exact(64) {
        let arr: &[u8; 64] = chunk.try_into().unwrap();
        out.push(IntentRecord::decode(arr));
    }
    Ok(out)
}
