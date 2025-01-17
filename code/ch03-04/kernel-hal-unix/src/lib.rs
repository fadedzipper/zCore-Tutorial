#![feature(asm)]
#![feature(linkage)]
#![deny(warnings)]

extern crate alloc;
#[macro_use]
extern crate log;

use {
    alloc::boxed::Box,
    alloc::collections::VecDeque,
    core::time::Duration,
    core::{future::Future, pin::Pin},
    lazy_static::*,
    std::fmt::{Debug, Formatter},
    std::fs::{File, OpenOptions},
    std::io::Error,
    std::os::unix::io::AsRawFd,
    std::sync::Mutex,
    std::time::SystemTime,
    tempfile::tempdir,
};

pub use kernel_hal::{defs::*, *};

#[repr(C)]
pub struct Thread {
    thread: usize,
}

impl Thread {
    #[export_name = "hal_thread_spawn"]
    pub fn spawn(
        future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
        _vmtoken: usize,
    ) -> Self {
        async_std::task::spawn(future);
        Thread { thread: 0 }
    }
}

/// Get current time.
#[export_name = "hal_timer_now"]
pub fn timer_now() -> Duration {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
}

/// Initialize the HAL.
///
/// This function must be called at the beginning.
pub fn init() {
    #[cfg(target_os = "macos")]
    unimplemented!()
}

#[repr(C)]
pub struct PhysFrame {
    paddr: PhysAddr,
}

impl Debug for PhysFrame {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::result::Result<(), std::fmt::Error> {
        write!(f, "PhysFrame({:#x})", self.paddr)
    }
}

const PMEM_SIZE: usize = 0x4000_0000; // 1GiB
const PAGE_SIZE: usize = 0x1000;
fn page_aligned(x: VirtAddr) -> bool {
    x % PAGE_SIZE == 0
}

lazy_static! {
    static ref FRAME_FILE: File = create_pmem_file();
}

fn create_pmem_file() -> File {
    let dir = tempdir().expect("failed to create pmem dir");
    let path = dir.path().join("pmem");

    // workaround on macOS to avoid permission denied.
    // see https://jiege.ch/software/2020/02/07/macos-mmap-exec/ for analysis on this problem.
    #[cfg(target_os = "macos")]
    std::mem::forget(dir);

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .expect("failed to create pmem file");
    file.set_len(PMEM_SIZE as u64)
        .expect("failed to resize file");
    trace!("create pmem file: path={:?}, size={:#x}", path, PMEM_SIZE);
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    mmap(file.as_raw_fd(), 0, PMEM_SIZE, phys_to_virt(0), prot);
    file
}

/// Mmap frame file `fd` to `vaddr`.
fn mmap(fd: libc::c_int, offset: usize, len: usize, vaddr: VirtAddr, prot: libc::c_int) {
    // workaround on macOS to write text section.
    #[cfg(target_os = "macos")]
    let prot = if prot & libc::PROT_EXEC != 0 {
        prot | libc::PROT_WRITE
    } else {
        prot
    };

    let ret = unsafe {
        let flags = libc::MAP_SHARED | libc::MAP_FIXED;
        libc::mmap(vaddr as _, len, prot, flags, fd, offset as _)
    } as usize;
    trace!(
        "mmap file: fd={}, offset={:#x}, len={:#x}, vaddr={:#x}, prot={:#b}",
        fd,
        offset,
        len,
        vaddr,
        prot,
    );
    assert_eq!(ret, vaddr, "failed to mmap: {:?}", Error::last_os_error());
}

lazy_static! {
    static ref AVAILABLE_FRAMES: Mutex<VecDeque<usize>> =
        Mutex::new((PAGE_SIZE..PMEM_SIZE).step_by(PAGE_SIZE).collect());
}

impl PhysFrame {
    #[export_name = "hal_frame_alloc"]
    pub fn alloc() -> Option<Self> {
        let ret = AVAILABLE_FRAMES
            .lock()
            .unwrap()
            .pop_front()
            .map(|paddr| PhysFrame { paddr });
        trace!("frame alloc: {:?}", ret);
        ret
    }
    #[export_name = "hal_zero_frame_paddr"]
    pub fn zero_frame_addr() -> PhysAddr {
        0
    }
}

impl Drop for PhysFrame {
    #[export_name = "hal_frame_dealloc"]
    fn drop(&mut self) {
        trace!("frame dealloc: {:?}", self);
        AVAILABLE_FRAMES.lock().unwrap().push_back(self.paddr);
    }
}

fn phys_to_virt(paddr: PhysAddr) -> VirtAddr {
    /// Map physical memory from here.
    const PMEM_BASE: VirtAddr = 0x8_0000_0000;

    PMEM_BASE + paddr
}

/// Ensure physical memory are mmapped and accessible.
fn ensure_mmap_pmem() {
    FRAME_FILE.as_raw_fd();
}

/// Read physical memory from `paddr` to `buf`.
#[export_name = "hal_pmem_read"]
pub fn pmem_read(paddr: PhysAddr, buf: &mut [u8]) {
    trace!("pmem read: paddr={:#x}, len={:#x}", paddr, buf.len());
    assert!(paddr + buf.len() <= PMEM_SIZE);
    ensure_mmap_pmem();
    unsafe {
        (phys_to_virt(paddr) as *const u8).copy_to_nonoverlapping(buf.as_mut_ptr(), buf.len());
    }
}

/// Write physical memory to `paddr` from `buf`.
#[export_name = "hal_pmem_write"]
pub fn pmem_write(paddr: PhysAddr, buf: &[u8]) {
    trace!("pmem write: paddr={:#x}, len={:#x}", paddr, buf.len());
    assert!(paddr + buf.len() <= PMEM_SIZE);
    ensure_mmap_pmem();
    unsafe {
        buf.as_ptr()
            .copy_to_nonoverlapping(phys_to_virt(paddr) as _, buf.len());
    }
}

/// Zero physical memory at `[paddr, paddr + len)`
#[export_name = "hal_pmem_zero"]
pub fn pmem_zero(paddr: PhysAddr, len: usize) {
    trace!("pmem_zero: addr={:#x}, len={:#x}", paddr, len);
    assert!(paddr + len <= PMEM_SIZE);
    ensure_mmap_pmem();
    unsafe {
        core::ptr::write_bytes(phys_to_virt(paddr) as *mut u8, 0, len);
    }
}

/// Copy content of `src` frame to `target` frame
#[export_name = "hal_frame_copy"]
pub fn frame_copy(src: PhysAddr, target: PhysAddr) {
    trace!("frame_copy: {:#x} <- {:#x}", target, src);
    assert!(src + PAGE_SIZE <= PMEM_SIZE && target + PAGE_SIZE <= PMEM_SIZE);
    ensure_mmap_pmem();
    unsafe {
        let buf = phys_to_virt(src) as *const u8;
        buf.copy_to_nonoverlapping(phys_to_virt(target) as _, PAGE_SIZE);
    }
}

/// Flush the physical frame.
#[export_name = "hal_frame_flush"]
pub fn frame_flush(_target: PhysAddr) {
    // do nothing
}

/// Page Table
#[repr(C)]
pub struct PageTable {
    table_phys: PhysAddr,
}

impl PageTable {
    /// Create a new `PageTable`.
    #[allow(clippy::new_without_default)]
    #[export_name = "hal_pt_new"]
    pub fn new() -> Self {
        PageTable { table_phys: 0 }
    }
}

impl PageTableTrait for PageTable {
    /// Map the page of `vaddr` to the frame of `paddr` with `flags`.
    #[export_name = "hal_pt_map"]
    fn map(&mut self, vaddr: VirtAddr, paddr: PhysAddr, flags: MMUFlags) -> Result<()> {
        debug_assert!(page_aligned(vaddr));
        debug_assert!(page_aligned(paddr));
        let prot = flags.to_mmap_prot();
        mmap(FRAME_FILE.as_raw_fd(), paddr, PAGE_SIZE, vaddr, prot);
        Ok(())
    }

    /// Unmap the page of `vaddr`.
    #[export_name = "hal_pt_unmap"]
    fn unmap(&mut self, vaddr: VirtAddr) -> Result<()> {
        self.unmap_cont(vaddr, 1)
    }

    /// Change the `flags` of the page of `vaddr`.
    #[export_name = "hal_pt_protect"]
    fn protect(&mut self, vaddr: VirtAddr, flags: MMUFlags) -> Result<()> {
        debug_assert!(page_aligned(vaddr));
        let prot = flags.to_mmap_prot();
        let ret = unsafe { libc::mprotect(vaddr as _, PAGE_SIZE, prot) };
        assert_eq!(ret, 0, "failed to mprotect: {:?}", Error::last_os_error());
        Ok(())
    }

    /// Query the physical address which the page of `vaddr` maps to.
    #[export_name = "hal_pt_query"]
    fn query(&mut self, vaddr: VirtAddr) -> Result<PhysAddr> {
        debug_assert!(page_aligned(vaddr));
        unimplemented!()
    }

    /// Get the physical address of root page table.
    #[export_name = "hal_pt_table_phys"]
    fn table_phys(&self) -> PhysAddr {
        self.table_phys
    }

    #[export_name = "hal_pt_unmap_cont"]
    fn unmap_cont(&mut self, vaddr: VirtAddr, pages: usize) -> Result<()> {
        if pages == 0 {
            return Ok(());
        }
        debug_assert!(page_aligned(vaddr));
        let ret = unsafe { libc::munmap(vaddr as _, PAGE_SIZE * pages) };
        assert_eq!(ret, 0, "failed to munmap: {:?}", Error::last_os_error());
        Ok(())
    }
}

trait FlagsExt {
    fn to_mmap_prot(&self) -> libc::c_int;
}

impl FlagsExt for MMUFlags {
    fn to_mmap_prot(&self) -> libc::c_int {
        let mut flags = 0;
        if self.contains(MMUFlags::READ) {
            flags |= libc::PROT_READ;
        }
        if self.contains(MMUFlags::WRITE) {
            flags |= libc::PROT_WRITE;
        }
        if self.contains(MMUFlags::EXECUTE) {
            flags |= libc::PROT_EXEC;
        }
        flags
    }
}
