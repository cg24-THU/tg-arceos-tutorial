//! Linux syscall emulation for musl user programs (open/read/write/mmap/…).
//!
//! Syscall numbers differ by architecture; see `nums` modules below.

use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicUsize, Ordering};

use axerrno::LinuxError;
use axfs::fops::{File, OpenOptions};
use axhal::paging::MappingFlags;
use axhal::uspace::UserContext;
use axsync::Mutex;
use memory_addr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};

// ---- Architecture-specific syscall numbers ----

#[cfg(not(target_arch = "x86_64"))]
mod nums {
    pub const SYS_IOCTL: usize = 29;
    pub const SYS_WRITEV: usize = 66;
    pub const SYS_READ: usize = 63;
    pub const SYS_WRITE: usize = 64;
    pub const SYS_OPENAT: usize = 56;
    pub const SYS_CLOSE: usize = 57;
    pub const SYS_EXIT: usize = 93;
    pub const SYS_EXIT_GROUP: usize = 94;
    pub const SYS_SET_TID_ADDRESS: usize = 96;
    pub const SYS_MMAP: usize = 222;
    pub const SYS_BRK: usize = 214;
    pub const SYS_GETUID: usize = 174;
    pub const SYS_GETEUID: usize = 175;
    pub const SYS_GETGID: usize = 176;
    pub const SYS_GETEGID: usize = 177;
    pub const SYS_SET_ROBUST_LIST: usize = 99;
    pub const SYS_UNAME: usize = 160;
    pub const SYS_GETPID: usize = 172;
    pub const SYS_GETTID: usize = 178;
    pub const SYS_TGKILL: usize = 131;
    pub const SYS_RT_SIGACTION: usize = 134;
    pub const SYS_RT_SIGPROCMASK: usize = 135;
    pub const SYS_CLOCK_GETTIME: usize = 113;
    pub const SYS_MPROTECT: usize = 226;
    pub const SYS_PRLIMIT64: usize = 261;
    pub const SYS_GETRANDOM: usize = 278;
    pub const SYS_READLINKAT: usize = 78;
}

#[cfg(target_arch = "x86_64")]
mod nums {
    /// Legacy `open(2)`; musl may use this on x86_64 instead of `openat`.
    pub const SYS_OPEN: usize = 2;
    pub const SYS_IOCTL: usize = 16;
    pub const SYS_WRITEV: usize = 20;
    pub const SYS_READ: usize = 0;
    pub const SYS_WRITE: usize = 1;
    pub const SYS_OPENAT: usize = 257;
    pub const SYS_CLOSE: usize = 3;
    pub const SYS_EXIT: usize = 60;
    pub const SYS_EXIT_GROUP: usize = 231;
    pub const SYS_SET_TID_ADDRESS: usize = 218;
    pub const SYS_MMAP: usize = 9;
    pub const SYS_BRK: usize = 12;
    pub const SYS_ARCH_PRCTL: usize = 158;
    pub const ARCH_SET_FS: usize = 0x1002;
}

use nums::*;

const AT_FDCWD: i32 = -100;

// Linux open(2) flags (musl / kernel ABI)
const O_ACCMODE: u32 = 0o3;
const O_RDONLY: u32 = 0o0;
const O_WRONLY: u32 = 0o1;
const O_RDWR: u32 = 0o2;
const O_CREAT: u32 = 0o100;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
const O_EXCL: u32 = 0o200;

/// Program break for minimal `brk` emulation.
static PROGRAM_BRK: AtomicUsize = AtomicUsize::new(0x0300_0000);
static PROGRAM_BRK_MAPPED: AtomicUsize = AtomicUsize::new(0x0300_0000);
static NEXT_MMAP_ADDR: AtomicUsize = AtomicUsize::new(0x1000_0000);

static FD_TABLE: Mutex<Vec<Option<File>>> = Mutex::new(Vec::new());

#[repr(C)]
struct IoVec {
    iov_base: usize,
    iov_len: usize,
}

#[repr(C)]
struct UtsName {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
struct Rlimit {
    rlim_cur: u64,
    rlim_max: u64,
}

bitflags::bitflags! {
    #[derive(Debug)]
    /// permissions for sys_mmap
    ///
    /// See <https://github.com/bminor/glibc/blob/master/bits/mman.h>
    struct MmapProt: i32 {
        /// Page can be read.
        const PROT_READ = 1 << 0;
        /// Page can be written.
        const PROT_WRITE = 1 << 1;
        /// Page can be executed.
        const PROT_EXEC = 1 << 2;
    }
}

bitflags::bitflags! {
    #[derive(Debug)]
    /// flags for sys_mmap
    ///
    /// See <https://github.com/bminor/glibc/blob/master/bits/mman.h>
    struct MmapFlags: i32 {
        /// Share changes
        const MAP_SHARED = 1 << 0;
        /// Changes private; copy pages on write.
        const MAP_PRIVATE = 1 << 1;
        /// Map address must be exactly as requested, no matter whether it is available.
        const MAP_FIXED = 1 << 4;
        /// Don't use a file.
        const MAP_ANONYMOUS = 1 << 5;
        /// Don't check for reservations.
        const MAP_NORESERVE = 1 << 14;
        /// Allocation is for a stack.
        const MAP_STACK = 0x20000;
    }
}

impl From<MmapProt> for MappingFlags {
    fn from(value: MmapProt) -> Self {
        let mut flags = MappingFlags::USER;
        if value.contains(MmapProt::PROT_READ) {
            flags |= MappingFlags::READ;
        }
        if value.contains(MmapProt::PROT_WRITE) {
            flags |= MappingFlags::WRITE;
        }
        if value.contains(MmapProt::PROT_EXEC) {
            flags |= MappingFlags::EXECUTE;
        }
        flags
    }
}

fn get_syscall_num(uctx: &UserContext) -> usize {
    #[cfg(any(
        target_arch = "riscv64",
        target_arch = "riscv32",
        target_arch = "loongarch64"
    ))]
    {
        uctx.regs.a7 as usize
    }
    #[cfg(target_arch = "aarch64")]
    {
        uctx.x[8] as usize
    }
    #[cfg(target_arch = "x86_64")]
    {
        uctx.rax as usize
    }
}

fn set_syscall_ret(uctx: &mut UserContext, ret: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        uctx.rax = ret as u64;
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        uctx.set_arg0(ret);
    }
}

fn neg_errno(e: LinuxError) -> isize {
    -(e.code() as isize)
}

unsafe fn c_str_to_string(ptr: *const c_char) -> Result<String, LinuxError> {
    if ptr.is_null() {
        return Err(LinuxError::EFAULT);
    }
    let mut len = 0usize;
    loop {
        let b = unsafe { *ptr.add(len) };
        if b == 0 {
            break;
        }
        len += 1;
        if len > 4096 {
            return Err(LinuxError::ENAMETOOLONG);
        }
    }
    let slice = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
    let s = core::str::from_utf8(slice).map_err(|_| LinuxError::EINVAL)?;
    Ok(String::from(s))
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        String::from(path)
    } else {
        alloc::format!("/{path}")
    }
}

fn linux_flags_to_open_options(flags: u32, _mode: u32) -> Result<OpenOptions, LinuxError> {
    let acc = flags & O_ACCMODE;
    let mut opts = OpenOptions::new();
    match acc {
        O_RDONLY => opts.read(true),
        O_WRONLY => opts.write(true),
        O_RDWR => {
            opts.read(true);
            opts.write(true);
        }
        _ => return Err(LinuxError::EINVAL),
    }
    if flags & O_APPEND != 0 {
        opts.append(true);
    }
    if flags & O_TRUNC != 0 {
        opts.truncate(true);
    }
    if flags & O_CREAT != 0 {
        opts.create(true);
    }
    if flags & O_EXCL != 0 {
        opts.create_new(true);
    }
    Ok(opts)
}

fn fd_alloc(file: File) -> i32 {
    let mut t = FD_TABLE.lock();
    for (i, slot) in t.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(file);
            return i as i32;
        }
    }
    t.push(Some(file));
    (t.len() - 1) as i32
}

fn fd_take(fd: i32) -> Result<File, LinuxError> {
    if fd < 0 {
        return Err(LinuxError::EBADF);
    }
    let i = fd as usize;
    let mut t = FD_TABLE.lock();
    if i >= t.len() {
        return Err(LinuxError::EBADF);
    }
    t[i].take().ok_or(LinuxError::EBADF)
}

// Index-based access with a closure (holds `FD_TABLE` only for the duration of `f`).

fn with_file_fd<F, R>(fd: i32, f: F) -> Result<R, LinuxError>
where
    F: FnOnce(&mut File) -> Result<R, LinuxError>,
{
    if fd < 0 {
        return Err(LinuxError::EBADF);
    }
    let i = fd as usize;
    let mut t = FD_TABLE.lock();
    let slot = t.get_mut(i).ok_or(LinuxError::EBADF)?;
    let file = slot.as_mut().ok_or(LinuxError::EBADF)?;
    f(file)
}

fn sys_openat(dfd: c_int, fname: *const c_char, flags: c_int, mode: u32) -> isize {
    if dfd != AT_FDCWD {
        return neg_errno(LinuxError::EINVAL);
    }
    let path = match unsafe { c_str_to_string(fname) } {
        Ok(s) => normalize_path(&s),
        Err(e) => return neg_errno(e),
    };
    let flags = flags as u32;
    let opts = match linux_flags_to_open_options(flags, mode) {
        Ok(o) => o,
        Err(e) => return neg_errno(e),
    };
    match File::open(path.as_str(), &opts) {
        Ok(f) => {
            if f.get_attr().map(|a| a.is_dir()).unwrap_or(false) {
                neg_errno(LinuxError::EISDIR)
            } else {
                fd_alloc(f) as isize
            }
        }
        Err(e) => neg_errno(LinuxError::from(e)),
    }
}

fn sys_close(fd: i32) -> isize {
    match fd_take(fd) {
        Ok(_file) => 0,
        Err(e) => neg_errno(e),
    }
}

fn sys_read(fd: i32, buf: *mut c_void, count: usize) -> isize {
    if buf.is_null() {
        return neg_errno(LinuxError::EFAULT);
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, count) };
    match with_file_fd(fd, |file| match file.read(slice) {
        Ok(n) => Ok(n as isize),
        Err(e) => Err(LinuxError::from(e)),
    }) {
        Ok(n) => n,
        Err(e) => neg_errno(e),
    }
}

fn sys_write(fd: i32, buf: *const c_void, count: usize) -> isize {
    if fd == 1 || fd == 2 {
        if buf.is_null() {
            return neg_errno(LinuxError::EFAULT);
        }
        let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, count) };
        for &b in slice {
            ax_print!("{}", b as char);
        }
        return count as isize;
    }
    if buf.is_null() {
        return neg_errno(LinuxError::EFAULT);
    }
    let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, count) };
    match with_file_fd(fd, |file| match file.write(slice) {
        Ok(n) => Ok(n as isize),
        Err(e) => Err(LinuxError::from(e)),
    }) {
        Ok(n) => n,
        Err(e) => neg_errno(e),
    }
}

fn sys_writev(fd: i32, iov: *const IoVec, iovcnt: i32) -> isize {
    if fd != 1 && fd != 2 {
        return neg_errno(LinuxError::EBADF);
    }
    let mut total: isize = 0;
    for i in 0..iovcnt as usize {
        let entry = unsafe { &*iov.add(i) };
        if entry.iov_len == 0 || entry.iov_base == 0 {
            continue;
        }
        let slice =
            unsafe { core::slice::from_raw_parts(entry.iov_base as *const u8, entry.iov_len) };
        for &b in slice {
            ax_print!("{}", b as char);
        }
        total += entry.iov_len as isize;
    }
    total
}

fn sys_brk(addr: usize) -> isize {
    // Linux brk syscall returns the new program break on success.
    let cur = PROGRAM_BRK.load(Ordering::Relaxed);
    if addr == 0 {
        return cur as isize;
    }
    let mapped_end = PROGRAM_BRK_MAPPED.load(Ordering::Relaxed);
    let new_mapped_end = (addr + PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1);
    if new_mapped_end > mapped_end {
        let aspace_guard = crate::USER_ASPACE.lock();
        let Some(shared) = aspace_guard.as_ref() else {
            return neg_errno(LinuxError::EFAULT);
        };
        let mut aspace = shared.lock();
        if let Err(e) = aspace.map_alloc(
            VirtAddr::from(mapped_end),
            new_mapped_end - mapped_end,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER,
            true,
        ) {
            return neg_errno(LinuxError::from(e));
        }
        PROGRAM_BRK_MAPPED.store(new_mapped_end, Ordering::Relaxed);
    }
    PROGRAM_BRK.store(addr, Ordering::Relaxed);
    addr as isize
}

fn sys_mmap(
    addr: *mut c_void,
    length: usize,
    prot: i32,
    flags: i32,
    fd: i32,
    offset: isize,
) -> isize {
    if length == 0 || offset < 0 || offset as usize % PAGE_SIZE_4K != 0 {
        return neg_errno(LinuxError::EINVAL);
    }

    let prot = match MmapProt::from_bits(prot) {
        Some(bits) => bits,
        None => return neg_errno(LinuxError::EINVAL),
    };
    let flags = match MmapFlags::from_bits(flags) {
        Some(bits) => bits,
        None => return neg_errno(LinuxError::EINVAL),
    };
    if !flags.intersects(MmapFlags::MAP_SHARED | MmapFlags::MAP_PRIVATE) {
        return neg_errno(LinuxError::EINVAL);
    }

    let map_len = (length + PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1);
    let map_flags = MappingFlags::from(prot);

    let aspace_guard = crate::USER_ASPACE.lock();
    let Some(shared) = aspace_guard.as_ref() else {
        return neg_errno(LinuxError::EFAULT);
    };
    let mut aspace = shared.lock();

    let start = if flags.contains(MmapFlags::MAP_FIXED) {
        let requested = addr as usize;
        if requested == 0 || requested % PAGE_SIZE_4K != 0 {
            return neg_errno(LinuxError::EINVAL);
        }
        VirtAddr::from(requested)
    } else {
        let hint = if addr.is_null() {
            VirtAddr::from(NEXT_MMAP_ADDR.load(Ordering::Relaxed))
        } else {
            VirtAddr::from((addr as usize) & !(PAGE_SIZE_4K - 1))
        };
        let limit = VirtAddrRange::from_start_size(aspace.base(), aspace.size());
        match aspace.find_free_area(hint, map_len, limit) {
            Some(vaddr) => vaddr,
            None => return neg_errno(LinuxError::ENOMEM),
        }
    };

    if let Err(e) = aspace.map_alloc(start, map_len, map_flags, true) {
        return neg_errno(LinuxError::from(e));
    }

    if !flags.contains(MmapFlags::MAP_ANONYMOUS) {
        let mut file_buf = Vec::new();
        file_buf.resize(length, 0);
        let read_res = with_file_fd(fd, |file| {
            file.read_at(offset as u64, &mut file_buf)
                .map_err(LinuxError::from)
        });
        match read_res {
            Ok(_n) => {
                if let Err(e) = aspace.write(start, &file_buf) {
                    let _ = aspace.unmap(start, map_len);
                    return neg_errno(LinuxError::from(e));
                }
            }
            Err(e) => {
                let _ = aspace.unmap(start, map_len);
                return neg_errno(e);
            }
        }
    }

    NEXT_MMAP_ADDR.store(start.as_usize() + map_len, Ordering::Relaxed);
    start.as_usize() as isize
}

fn write_user_buf(addr: usize, buf: &[u8]) -> isize {
    let aspace_guard = crate::USER_ASPACE.lock();
    let Some(shared) = aspace_guard.as_ref() else {
        return neg_errno(LinuxError::EFAULT);
    };
    let aspace = shared.lock();
    match aspace.write(VirtAddr::from(addr), buf) {
        Ok(()) => 0,
        Err(e) => neg_errno(LinuxError::from(e)),
    }
}

fn write_c_field(dst: &mut [u8; 65], value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(64);
    dst[..len].copy_from_slice(&bytes[..len]);
    dst[len] = 0;
}

fn sys_uname(buf: *mut c_void) -> isize {
    if buf.is_null() {
        return neg_errno(LinuxError::EFAULT);
    }
    let mut uts = UtsName {
        sysname: [0; 65],
        nodename: [0; 65],
        release: [0; 65],
        version: [0; 65],
        machine: [0; 65],
        domainname: [0; 65],
    };
    write_c_field(&mut uts.sysname, "Linux");
    write_c_field(&mut uts.nodename, "arceos");
    write_c_field(&mut uts.release, "6.1.0");
    write_c_field(&mut uts.version, "ArceOS");
    write_c_field(&mut uts.domainname, "localdomain");
    #[cfg(target_arch = "riscv64")]
    write_c_field(&mut uts.machine, "riscv64");
    #[cfg(target_arch = "aarch64")]
    write_c_field(&mut uts.machine, "aarch64");
    #[cfg(target_arch = "loongarch64")]
    write_c_field(&mut uts.machine, "loongarch64");
    #[cfg(target_arch = "x86_64")]
    write_c_field(&mut uts.machine, "x86_64");

    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&uts as *const UtsName).cast::<u8>(),
            core::mem::size_of::<UtsName>(),
        )
    };
    write_user_buf(buf as usize, bytes)
}

fn sys_clock_gettime(_clockid: usize, tp: *mut c_void) -> isize {
    if tp.is_null() {
        return neg_errno(LinuxError::EFAULT);
    }
    let ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&ts as *const Timespec).cast::<u8>(),
            core::mem::size_of::<Timespec>(),
        )
    };
    write_user_buf(tp as usize, bytes)
}

fn sys_mprotect(addr: usize, len: usize, prot: i32) -> isize {
    if addr % PAGE_SIZE_4K != 0 || len == 0 {
        return neg_errno(LinuxError::EINVAL);
    }
    let prot = match MmapProt::from_bits(prot) {
        Some(bits) => bits,
        None => return neg_errno(LinuxError::EINVAL),
    };
    let len = (len + PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1);
    let aspace_guard = crate::USER_ASPACE.lock();
    let Some(shared) = aspace_guard.as_ref() else {
        return neg_errno(LinuxError::EFAULT);
    };
    let mut aspace = shared.lock();
    match aspace.protect(VirtAddr::from(addr), len, MappingFlags::from(prot)) {
        Ok(()) => 0,
        Err(e) => neg_errno(LinuxError::from(e)),
    }
}

fn sys_prlimit64(_pid: usize, _resource: usize, _new_limit: usize, old_limit: usize) -> isize {
    if old_limit != 0 {
        let lim = Rlimit {
            rlim_cur: u64::MAX,
            rlim_max: u64::MAX,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&lim as *const Rlimit).cast::<u8>(),
                core::mem::size_of::<Rlimit>(),
            )
        };
        let ret = write_user_buf(old_limit, bytes);
        if ret < 0 {
            return ret;
        }
    }
    0
}

fn sys_getrandom(buf: *mut c_void, len: usize, _flags: usize) -> isize {
    if buf.is_null() {
        return neg_errno(LinuxError::EFAULT);
    }
    let zeros = alloc::vec![0u8; len];
    let ret = write_user_buf(buf as usize, &zeros);
    if ret < 0 {
        ret
    } else {
        len as isize
    }
}

fn sys_readlinkat(_dirfd: i32, _path: *const c_char, _buf: *mut c_void, _bufsiz: usize) -> isize {
    neg_errno(LinuxError::ENOENT)
}

#[cfg(target_arch = "x86_64")]
fn sys_arch_prctl(uctx: &mut UserContext, op: usize, addr: usize) -> isize {
    match op {
        ARCH_SET_FS => {
            uctx.fs_base = addr as u64;
            0
        }
        _ => {
            ax_println!("Unimplemented arch_prctl op: {:#x}", op);
            neg_errno(LinuxError::EINVAL)
        }
    }
}

/// Handle a syscall from user space.
pub fn handle_syscall(uctx: &mut UserContext) -> Option<i32> {
    let syscall_num = get_syscall_num(uctx);
    let args = [
        uctx.arg0(),
        uctx.arg1(),
        uctx.arg2(),
        uctx.arg3(),
        uctx.arg4(),
        uctx.arg5(),
    ];
    ax_println!("handle_syscall [{}] ...", syscall_num);

    let ret: isize = match syscall_num {
        SYS_IOCTL => {
            ax_println!("Ignore SYS_IOCTL");
            0
        }
        #[cfg(not(target_arch = "x86_64"))]
        SYS_GETUID | SYS_GETEUID | SYS_GETGID | SYS_GETEGID => 0,
        #[cfg(not(target_arch = "x86_64"))]
        SYS_SET_ROBUST_LIST | SYS_RT_SIGACTION | SYS_RT_SIGPROCMASK | SYS_TGKILL => 0,
        #[cfg(not(target_arch = "x86_64"))]
        SYS_GETPID => 1,
        #[cfg(not(target_arch = "x86_64"))]
        SYS_GETTID => axtask::current().id().as_u64() as isize,
        #[cfg(not(target_arch = "x86_64"))]
        SYS_UNAME => sys_uname(args[0] as *mut c_void),
        #[cfg(not(target_arch = "x86_64"))]
        SYS_CLOCK_GETTIME => sys_clock_gettime(args[0], args[1] as *mut c_void),
        #[cfg(not(target_arch = "x86_64"))]
        SYS_MPROTECT => sys_mprotect(args[0], args[1], args[2] as i32),
        #[cfg(not(target_arch = "x86_64"))]
        SYS_PRLIMIT64 => sys_prlimit64(args[0], args[1], args[2], args[3]),
        #[cfg(not(target_arch = "x86_64"))]
        SYS_GETRANDOM => sys_getrandom(args[0] as *mut c_void, args[1], args[2]),
        #[cfg(not(target_arch = "x86_64"))]
        SYS_READLINKAT => sys_readlinkat(
            args[0] as i32,
            args[1] as *const c_char,
            args[2] as *mut c_void,
            args[3],
        ),
        SYS_SET_TID_ADDRESS => axtask::current().id().as_u64() as isize,
        SYS_BRK => sys_brk(args[0]),
        #[cfg(target_arch = "x86_64")]
        SYS_OPEN => sys_openat(
            AT_FDCWD,
            args[0] as *const c_char,
            args[1] as c_int,
            args[2] as u32,
        ),
        SYS_OPENAT => sys_openat(
            args[0] as c_int,
            args[1] as *const c_char,
            args[2] as c_int,
            args[3] as u32,
        ),
        SYS_CLOSE => sys_close(args[0] as i32),
        SYS_READ => sys_read(args[0] as i32, args[1] as *mut c_void, args[2]),
        SYS_WRITE => sys_write(args[0] as i32, args[1] as *const c_void, args[2]),
        SYS_WRITEV => sys_writev(args[0] as i32, args[1] as *const IoVec, args[2] as i32),
        SYS_EXIT_GROUP => {
            ax_println!("[SYS_EXIT_GROUP]: exiting ..");
            return Some(args[0] as i32);
        }
        SYS_EXIT => {
            ax_println!("[SYS_EXIT]: exiting ..");
            return Some(args[0] as i32);
        }
        SYS_MMAP => sys_mmap(
            args[0] as *mut c_void,
            args[1],
            args[2] as i32,
            args[3] as i32,
            args[4] as i32,
            args[5] as isize,
        ),
        #[cfg(target_arch = "x86_64")]
        SYS_ARCH_PRCTL => sys_arch_prctl(uctx, args[0], args[1]),
        _ => {
            ax_println!("Unimplemented syscall: {}", syscall_num);
            neg_errno(LinuxError::ENOSYS)
        }
    };

    set_syscall_ret(uctx, ret as usize);
    None
}
