//! Durable VERA intent persistence with a canonical versioned WAL format.
//! io_uring is optional: denied or unsupported kernels use the same positional
//! write protocol. append() completes the write; sync() acknowledges durability.

mod wal;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU32, Ordering};
use wal::WAL;
pub use wal::{atomic_write, checksum, replay_file, IntentRecord, IntoIntentArgs};

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

#[cfg(target_os = "linux")]
struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}
#[cfg(target_os = "linux")]
impl Mapping {
    fn new(fd: i32, len: usize, offset: i64) -> io::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                offset,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }
    fn bytes(&self) -> *mut u8 {
        self.ptr.cast()
    }
}
#[cfg(target_os = "linux")]
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

#[cfg(target_os = "linux")]
struct IoUring {
    fd: OwnedFd,
    _sq: Mapping,
    _cq: Option<Mapping>,
    _sqes: Mapping,
    sqes: *mut IoUringSqe,
    sq_head: *mut AtomicU32,
    sq_tail: *mut AtomicU32,
    sq_mask: u32,
    sq_array: *mut u32,
    cq_head: *mut AtomicU32,
    cq_tail: *mut AtomicU32,
    cq_mask: u32,
    cqes: *mut IoUringCqe,
    serial: u64,
}

// The ring has one submitting owner. It is never shared and submit_write waits
// for completion before returning the caller's borrowed buffer.
#[cfg(target_os = "linux")]
unsafe impl Send for IoUring {}

#[cfg(target_os = "linux")]
impl IoUring {
    fn setup(entries: u32) -> io::Result<Self> {
        const _: [(); 64] = [(); std::mem::size_of::<IoUringSqe>()];
        const _: [(); 16] = [(); std::mem::size_of::<IoUringCqe>()];
        let mut params = IoUringParams::default();
        let raw = unsafe { libc::syscall(libc::SYS_io_uring_setup, entries, &mut params) as i32 };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let single = params.features & IORING_FEAT_SINGLE_MMAP != 0;
        let sq_size = params.sq_off.array as usize + params.sq_entries as usize * 4;
        let cq_size = params.cq_off.cqes as usize + params.cq_entries as usize * 16;
        let sq = Mapping::new(
            raw,
            if single {
                sq_size.max(cq_size)
            } else {
                sq_size
            },
            IORING_OFF_SQ_RING,
        )?;
        let cq = if single {
            None
        } else {
            Some(Mapping::new(raw, cq_size, IORING_OFF_CQ_RING)?)
        };
        let sqes = Mapping::new(raw, params.sq_entries as usize * 64, IORING_OFF_SQES)?;
        let sq_ptr = sq.bytes();
        let cq_ptr = cq.as_ref().map(|m| m.bytes()).unwrap_or(sq_ptr);
        // Kernel ABI offsets are checked against the mapped lengths before use.
        let sq_len = if single {
            sq_size.max(cq_size)
        } else {
            sq_size
        };
        let cq_len = if single { sq_len } else { cq_size };
        for offset in [
            params.sq_off.head,
            params.sq_off.tail,
            params.sq_off.ring_mask,
        ] {
            if offset as usize + 4 > sq_len || offset % 4 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid SQ ring layout",
                ));
            }
        }
        for offset in [
            params.cq_off.head,
            params.cq_off.tail,
            params.cq_off.ring_mask,
        ] {
            if offset as usize + 4 > cq_len || offset % 4 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid CQ ring layout",
                ));
            }
        }
        if params.sq_off.array % 4 != 0
            || params.cq_off.cqes as usize % std::mem::align_of::<IoUringCqe>() != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unaligned ring array",
            ));
        }
        if params.sq_entries == 0
            || !params.sq_entries.is_power_of_two()
            || params.cq_entries == 0
            || !params.cq_entries.is_power_of_two()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid ring entry count",
            ));
        }
        unsafe {
            let sq_head = sq_ptr.add(params.sq_off.head as usize).cast::<AtomicU32>();
            let sq_tail = sq_ptr.add(params.sq_off.tail as usize).cast::<AtomicU32>();
            let sq_mask = *sq_ptr.add(params.sq_off.ring_mask as usize).cast::<u32>();
            let sq_array = sq_ptr.add(params.sq_off.array as usize).cast::<u32>();
            let cq_head = cq_ptr.add(params.cq_off.head as usize).cast::<AtomicU32>();
            let cq_tail = cq_ptr.add(params.cq_off.tail as usize).cast::<AtomicU32>();
            let cq_mask = *cq_ptr.add(params.cq_off.ring_mask as usize).cast::<u32>();
            let cqes = cq_ptr.add(params.cq_off.cqes as usize).cast::<IoUringCqe>();
            if sq_mask != params.sq_entries - 1 || cq_mask != params.cq_entries - 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid ring mask",
                ));
            }
            let sqes_ptr = sqes.bytes().cast::<IoUringSqe>();
            Ok(Self {
                fd,
                _sq: sq,
                _cq: cq,
                _sqes: sqes,
                sqes: sqes_ptr,
                sq_head,
                sq_tail,
                sq_mask,
                sq_array,
                cq_head,
                cq_tail,
                cq_mask,
                cqes,
                serial: 0,
            })
        }
    }

    fn submit_write(&mut self, fd: i32, bytes: &[u8], offset: u64) -> io::Result<usize> {
        let len = u32::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "io_uring write too large"))?;
        unsafe {
            let head = (*self.sq_head).load(Ordering::Acquire);
            let tail = (*self.sq_tail).load(Ordering::Relaxed);
            if tail.wrapping_sub(head) > self.sq_mask {
                return Err(io::Error::other("submission queue is full"));
            }
            self.serial = self.serial.wrapping_add(1);
            let serial = self.serial;
            let index = tail & self.sq_mask;
            *self.sqes.add(index as usize) = IoUringSqe {
                opcode: IORING_OP_WRITE,
                flags: 0,
                ioprio: 0,
                fd,
                off: offset,
                addr: bytes.as_ptr() as u64,
                len,
                rw_flags: 0,
                user_data: serial,
                buf_index: 0,
                personality: 0,
                splice_fd_in: 0,
                _pad: [0; 2],
            };
            *self.sq_array.add(index as usize) = index;
            (*self.sq_tail).store(tail.wrapping_add(1), Ordering::Release);
            let mut fatal = None;
            loop {
                let cq_head = (*self.cq_head).load(Ordering::Relaxed);
                let cq_tail = (*self.cq_tail).load(Ordering::Acquire);
                if cq_head != cq_tail {
                    let cqe = *self.cqes.add((cq_head & self.cq_mask) as usize);
                    (*self.cq_head).store(cq_head.wrapping_add(1), Ordering::Release);
                    if let Some(error) = fatal {
                        return Err(error);
                    }
                    if cqe.user_data != serial {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "unexpected io_uring completion",
                        ));
                    }
                    if cqe.res < 0 {
                        return Err(io::Error::from_raw_os_error(-cqe.res));
                    }
                    return Ok(cqe.res as usize);
                }
                if fatal.is_some() {
                    // A submitted request still owns bytes. Even if waiting via
                    // enter failed, retain the buffer until its CQE is visible.
                    std::thread::yield_now();
                    continue;
                }
                let submit = if (*self.sq_head).load(Ordering::Acquire) == head {
                    1u32
                } else {
                    0u32
                };
                let rc = libc::syscall(
                    libc::SYS_io_uring_enter,
                    self.fd.as_raw_fd(),
                    submit,
                    1u32,
                    IORING_ENTER_GETEVENTS,
                    std::ptr::null::<libc::sigset_t>(),
                    0usize,
                );
                if rc < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if (*self.sq_head).load(Ordering::Acquire) == head {
                        // No kernel consumer exists without SQPOLL and enter did
                        // not consume this SQE, so it can be retracted safely.
                        (*self.sq_tail).store(tail, Ordering::Release);
                        return Err(error);
                    }
                    fatal = Some(error);
                }
            }
        }
    }

    fn write_all(
        &mut self,
        file: &std::fs::File,
        mut bytes: &[u8],
        mut offset: u64,
    ) -> io::Result<()> {
        while !bytes.is_empty() {
            let count = bytes.len().min(u32::MAX as usize);
            let written = self.submit_write(file.as_raw_fd(), &bytes[..count], offset)?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "zero-length io_uring write",
                ));
            }
            if written > count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized io_uring completion",
                ));
            }
            bytes = &bytes[written..];
            offset += written as u64;
        }
        Ok(())
    }
}

/// The same canonical WAL protocol, accelerated by io_uring when available.
pub struct IoUringWAL {
    core: WAL,
    #[cfg(target_os = "linux")]
    ring: Option<IoUring>,
}
impl std::ops::Deref for IoUringWAL {
    type Target = WAL;
    fn deref(&self) -> &WAL {
        &self.core
    }
}
impl IoUringWAL {
    pub fn open(path: &str) -> io::Result<Self> {
        let core = WAL::open(path)?;
        #[cfg(target_os = "linux")]
        let ring = IoUring::setup(8).ok();
        Ok(Self {
            core,
            #[cfg(target_os = "linux")]
            ring,
        })
    }
    pub fn uses_io_uring(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.ring.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
    pub fn append<A: IntoIntentArgs>(&mut self, transition: u32, args: A) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(ring) = self.ring.as_mut() {
            return self
                .core
                .append_using(transition, args, |file, bytes, offset| {
                    ring.write_all(file, bytes, offset)
                });
        }
        self.core.append(transition, args)
    }
    pub fn sync(&mut self) -> io::Result<()> {
        self.core.sync()
    }
    pub fn replay(&self) -> io::Result<Vec<IntentRecord>> {
        self.core.replay()
    }
}
