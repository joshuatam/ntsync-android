// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (C) 2026 Joshua Tam <297250+joshuatam@users.noreply.github.com>

//! Userspace reimplementation of the Linux ntsync driver semantics
//! (drivers/misc/ntsync.c): NT semaphores, mutexes, and events.
//!
//! Cross-process design using Android/Linux NDK primitives:
//!
//! - All objects live in a fixed-size table in a file-backed shared mapping
//!   (opened by every process at the same path), mirroring the kernel's
//!   global object table.
//! - A process-shared *robust* pthread mutex in the shared page serializes
//!   all state changes and wait check/commit cycles, mirroring the kernel's
//!   dev->wait_all_lock. If a process dies holding it, the next locker gets
//!   EOWNERDEAD and marks it consistent.
//! - Waiters block with futex(FUTEX_WAIT) on a global generation counter
//!   that is bumped (and FUTEX_WAKE'd) by every state change, mirroring the
//!   kernel's wake-then-recheck loop in try_wake_any_obj/try_wake_all.
//!
//! Divergences from the kernel: closing an object that other threads are
//! waiting on fails those waits with EINVAL (the kernel keeps the object
//! alive via fd references); alertable waits are not supported; object
//! cleanup after a process crash requires ntsync_sweep_dead().

use std::ffi::CString;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

pub const MAX_WAIT_COUNT: usize = 64;

const MAGIC: u64 = 0x6E7473796E635F75; // "ntsyc_u"
const VERSION: u32 = 2;
const SLOT_COUNT: u32 = 16384;
const INDEX_BITS: u32 = 14; // log2(SLOT_COUNT)
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;
/// Resolve the region path: explicit argument, else $TMPDIR/ntsync_userspace.shm.
/// The caller is expected to export TMPDIR (Termux and app sandboxes do).
fn resolve_path(path: Option<&str>) -> Result<String, i32> {
    if let Some(p) = path {
        return Ok(p.into());
    }
    match std::env::var("TMPDIR") {
        Ok(tmp) if !tmp.is_empty() => Ok(format!("{tmp}/ntsync_userspace.shm")),
        _ => Err(libc::EINVAL),
    }
}

const SLOT_FREE: u32 = 0;
const SLOT_USED: u32 = 1;

const TYPE_SEM: u32 = 1;
const TYPE_MUTEX: u32 = 2;
const TYPE_EVENT: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Bad handle, wrong object type, or invalid argument (-EINVAL).
    Invalid,
    /// Caller is not the mutex owner (-EPERM).
    Perm,
    /// Semaphore count would exceed its maximum (-EOVERFLOW).
    Overflow,
    /// Region could not be opened/created/mapped (-errno).
    Init(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Object at `index` satisfied the wait; `owner_dead` is set when an
    /// abandoned mutex was acquired (kernel: -EOWNERDEAD).
    Signaled { index: u32, owner_dead: bool },
    /// Timed out (kernel: -ETIMEDOUT).
    Timeout,
    /// Bad arguments, or an object was closed mid-wait (kernel: -EINVAL).
    Invalid,
}

/// One object slot in the shared table. All fields are plain u32/u64 and
/// are only accessed under the region lock, except during futex waits.
#[repr(C)]
#[derive(Clone, Copy)]
struct Slot {
    state: u32,     // SLOT_FREE / SLOT_USED
    generation: u32,
    obj_type: u32,  // TYPE_*
    pid: u32,       // creator pid, for ntsync_sweep_dead
    a: u32,         // sem: count   | mutex: count   | event: signaled
    b: u32,         // sem: max     | mutex: owner   | event: manual
    c: u32,         // mutex: ownerdead
    d: u64,         // event: pulse_seq
}

const SLOT_SIZE: usize = std::mem::size_of::<Slot>();

#[repr(C)]
struct Header {
    magic: u64,
    version: u32,
    capacity: u32,
    /// Futex word: bumped on every state change.
    global_seq: AtomicU32,
    /// Allocation cursor.
    next_scan: u32,
    /// Debug: which thread currently holds `lock` (0 = none). Written after
    /// acquiring, cleared before releasing; only meaningful while locked.
    lock_owner_pid: u32,
    lock_owner_tid: u32,
    _pad: [u32; 1],
    lock: libc::pthread_mutex_t,
    // Slot array follows immediately.
}

const HEADER_SIZE: usize = std::mem::size_of::<Header>();

// Bionic exports the robust process-shared pthread mutex API (API 28+) but
// the libc crate does not expose it for Android, and the NDK stub libc.so
// omits both symbols entirely, which breaks consumers linking with
// --no-allow-shlib-undefined. Resolve them via dlsym at runtime instead.
#[cfg(target_os = "android")]
mod robust {
    use std::sync::OnceLock;

    type ConsistentFn = unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> i32;
    type SetrobustFn = unsafe extern "C" fn(*mut libc::pthread_mutexattr_t, i32) -> i32;

    // Fallbacks if the symbols are somehow missing: without robust mutexes the
    // kernel never reports EOWNERDEAD, so no-op consistent/setrobust is safe.
    unsafe extern "C" fn consistent_stub(_: *mut libc::pthread_mutex_t) -> i32 { 0 }
    unsafe extern "C" fn setrobust_stub(_: *mut libc::pthread_mutexattr_t, _: i32) -> i32 { 0 }

    static FUNCS: OnceLock<(ConsistentFn, SetrobustFn)> = OnceLock::new();

    fn funcs() -> (ConsistentFn, SetrobustFn) {
        *FUNCS.get_or_init(|| unsafe {
            let handle = libc::dlopen(b"libc.so\0".as_ptr().cast(), libc::RTLD_NOW);
            let lookup = |name: &[u8]| -> *mut libc::c_void {
                if handle.is_null() { return std::ptr::null_mut(); }
                libc::dlsym(handle, name.as_ptr().cast())
            };
            let consistent = lookup(b"pthread_mutex_consistent\0");
            let setrobust = lookup(b"pthread_mutexattr_setrobust\0");
            (
                if consistent.is_null() { consistent_stub } else { std::mem::transmute::<*mut libc::c_void, ConsistentFn>(consistent) },
                if setrobust.is_null() { setrobust_stub } else { std::mem::transmute::<*mut libc::c_void, SetrobustFn>(setrobust) },
            )
        })
    }

    pub unsafe fn pthread_mutex_consistent(lock: *mut libc::pthread_mutex_t) -> i32 {
        funcs().0(lock)
    }

    pub unsafe fn pthread_mutexattr_setrobust(attr: *mut libc::pthread_mutexattr_t, robust: i32) -> i32 {
        funcs().1(attr, robust)
    }
}
#[cfg(target_os = "android")]
use robust::{pthread_mutex_consistent, pthread_mutexattr_setrobust};

#[cfg(target_os = "android")]
const PTHREAD_PROCESS_SHARED: i32 = 1;
#[cfg(target_os = "android")]
const PTHREAD_MUTEX_ROBUST: i32 = 1;

#[cfg(not(target_os = "android"))]
use libc::{
    pthread_mutex_consistent, pthread_mutexattr_setrobust, PTHREAD_MUTEX_ROBUST,
    PTHREAD_PROCESS_SHARED,
};

fn errno() -> i32 {
    #[cfg(target_os = "android")]
    unsafe {
        *libc::__errno()
    }
    #[cfg(not(target_os = "android"))]
    unsafe {
        *libc::__errno_location()
    }
}

fn region_size() -> usize {
    HEADER_SIZE + SLOT_COUNT as usize * SLOT_SIZE
}

struct Region {
    base: *mut u8,
}

unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    fn header(&self) -> &Header {
        unsafe { &*(self.base as *const Header) }
    }

    fn slot_ptr(&self, index: u32) -> *mut Slot {
        unsafe { self.base.add(HEADER_SIZE + index as usize * SLOT_SIZE) as *mut Slot }
    }

    /// Get a used slot matching a handle's index+generation.
    fn slot(&self, handle: u32) -> Option<*mut Slot> {
        let index = handle & INDEX_MASK;
        let generation = handle >> INDEX_BITS;
        if index >= SLOT_COUNT {
            return None;
        }
        let slot = self.slot_ptr(index);
        let s = unsafe { &*slot };
        if s.state == SLOT_USED && s.generation == generation {
            Some(slot)
        } else {
            None
        }
    }

    fn lock_guard(&self) -> Result<RegionGuard<'_>, i32> {
        let lock = &self.header().lock as *const libc::pthread_mutex_t as *mut _;
        let debug = debug_enabled();
        let mut waited = std::time::Duration::ZERO;
        // Wine suspends threads with SIGUSR1; if it lands while we hold the
        // shared region mutex the holder stays parked and every process
        // (including wineserver) wedges on the lock. Defer it until release.
        let mut old_mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        let mut block = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        unsafe {
            libc::sigemptyset(&mut block);
            libc::sigaddset(&mut block, libc::SIGUSR1);
            libc::pthread_sigmask(libc::SIG_BLOCK, &block, &mut old_mask);
        }
        loop {
            let ret = if debug {
                // Poll with a timeout so a stuck holder can be logged.
                let mut ts = unsafe { std::mem::zeroed::<libc::timespec>() };
                unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
                ts.tv_sec += 5;
                unsafe { libc::pthread_mutex_timedlock(lock, &ts) }
            } else {
                unsafe { libc::pthread_mutex_lock(lock) }
            };
            if ret == 0 || ret == libc::EOWNERDEAD {
                if ret == libc::EOWNERDEAD {
                    // Previous holder died mid-operation; the object states are
                    // still structurally valid because every op is a small,
                    // single-pass mutation.
                    unsafe { pthread_mutex_consistent(lock) };
                }
                if debug {
                    let h = self.header() as *const Header as *mut Header;
                    unsafe {
                        (*h).lock_owner_pid = std::process::id();
                        (*h).lock_owner_tid = current_tid();
                    }
                }
                return Ok(RegionGuard { region: self, old_mask });
            }
            if ret == libc::ETIMEDOUT {
                waited += std::time::Duration::from_secs(5);
                let h = self.header();
                debug_log(&format!(
                    "region lock blocked {:?} (pid {} tid {}): held by pid {} tid {}",
                    waited,
                    std::process::id(),
                    current_tid(),
                    h.lock_owner_pid,
                    h.lock_owner_tid,
                ));
                continue;
            }
            if ret != libc::EINTR {
                unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &old_mask, std::ptr::null_mut()) };
                return Err(-ret);
            }
        }
    }

    fn bump_and_wake(&self) {
        // Caller holds the lock.
        self.header().global_seq.fetch_add(1, Ordering::Release);
        let addr = &self.header().global_seq as *const AtomicU32 as *mut u32;
        unsafe {
            libc::syscall(libc::SYS_futex, addr, libc::FUTEX_WAKE, i32::MAX, 0, 0, 0);
        }
    }

    fn seq(&self) -> u32 {
        self.header().global_seq.load(Ordering::Acquire)
    }

    /// FUTEX_WAIT on the global sequence counter.
    fn wait_seq(&self, expected: u32, timeout: Option<Duration>) -> i32 {
        let addr = &self.header().global_seq as *const AtomicU32 as *mut u32;
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let ts_ptr = match timeout {
            Some(d) => {
                ts.tv_sec = d.as_secs() as _;
                ts.tv_nsec = d.subsec_nanos() as _;
                &mut ts as *mut _
            }
            None => std::ptr::null_mut(),
        };
        let ret = unsafe { libc::syscall(libc::SYS_futex, addr, libc::FUTEX_WAIT, expected, ts_ptr, 0, 0) };
        if ret == 0 {
            0
        } else {
            -errno()
        }
    }
}

struct RegionGuard<'a> {
    region: &'a Region,
    old_mask: libc::sigset_t,
}

impl Drop for RegionGuard<'_> {
    fn drop(&mut self) {
        let h = self.region.header() as *const Header as *mut Header;
        unsafe {
            (*h).lock_owner_pid = 0;
            (*h).lock_owner_tid = 0;
            let lock = &(*h).lock as *const libc::pthread_mutex_t as *mut _;
            libc::pthread_mutex_unlock(lock);
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.old_mask, std::ptr::null_mut());
        }
    }
}

fn current_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

fn open_and_map(path: &str) -> Result<Region, i32> {
    let c_path = CString::new(path).map_err(|_| libc::EINVAL)?;
    unsafe {
        let fd = libc::open(
            c_path.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC,
            0o600,
        );
        if fd < 0 {
            return Err(errno());
        }
        // Serialize initialization across processes.
        if libc::flock(fd, libc::LOCK_EX) != 0 {
            let e = errno();
            libc::close(fd);
            return Err(e);
        }
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            let e = errno();
            libc::close(fd);
            return Err(e);
        }
        let need_init = st.st_size == 0;
        if need_init && libc::ftruncate(fd, region_size() as _) != 0 {
            let e = errno();
            libc::close(fd);
            return Err(e);
        }
        let base = libc::mmap(
            std::ptr::null_mut(),
            region_size(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        libc::flock(fd, libc::LOCK_UN);
        libc::close(fd);
        if base == libc::MAP_FAILED {
            return Err(errno());
        }
        let region = Region { base: base as *mut u8 };
        if need_init {
            let header = &mut *(base as *mut Header);
            std::ptr::write_bytes(base, 0, region_size());
            let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
            libc::pthread_mutexattr_init(&mut attr);
            libc::pthread_mutexattr_setpshared(&mut attr, PTHREAD_PROCESS_SHARED);
            pthread_mutexattr_setrobust(&mut attr, PTHREAD_MUTEX_ROBUST);
            libc::pthread_mutex_init(&mut header.lock, &attr);
            libc::pthread_mutexattr_destroy(&mut attr);
            header.magic = MAGIC;
            header.version = VERSION;
            header.capacity = SLOT_COUNT;
            header.global_seq = AtomicU32::new(0);
            header.next_scan = 0;
        } else if region.header().magic != MAGIC
            || region.header().version != VERSION
            || region.header().capacity != SLOT_COUNT
        {
            libc::munmap(base, region_size());
            return Err(libc::EINVAL);
        }
        Ok(region)
    }
}

static REGION: OnceLock<Result<Region, i32>> = OnceLock::new();

/// Initialize (idempotently) with the shared-memory file at `path`.
/// Called automatically with $TMPDIR/ntsync_userspace.shm if the library
/// is used without an explicit init.
pub fn init(path: Option<&str>) -> Result<(), Error> {
    let result = REGION.get_or_init(|| resolve_path(path).and_then(|p| open_and_map(&p)));
    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(Error::Init(*e)),
    }
}

fn region() -> Result<&'static Region, Error> {
    init(None)?;
    match REGION.get() {
        Some(Ok(r)) => Ok(r),
        Some(Err(e)) => Err(Error::Init(*e)),
        None => unreachable!(),
    }
}

fn alloc_slot(r: &Region) -> Option<u32> {
    // Caller holds the lock.
    let header = r.header();
    for i in 0..SLOT_COUNT {
        let index = (header.next_scan + i) % SLOT_COUNT;
        let slot = r.slot_ptr(index);
        unsafe {
            if (*slot).state == SLOT_FREE {
                (*slot).state = SLOT_USED;
                (*(r.base as *mut Header)).next_scan = (index + 1) % SLOT_COUNT;
                return Some(((*slot).generation << INDEX_BITS) | index);
            }
        }
    }
    None
}

pub fn create_semaphore(count: u32, max: u32) -> Result<u32, Error> {
    if count > max {
        return Err(Error::Invalid);
    }
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let handle = alloc_slot(r).ok_or(Error::Init(libc::ENOMEM))?;
    let slot = r.slot_ptr(handle & INDEX_MASK);
    unsafe {
        (*slot).obj_type = TYPE_SEM;
        (*slot).pid = libc::getpid() as u32;
        (*slot).a = count;
        (*slot).b = max;
    }
    Ok(handle)
}

pub fn create_mutex(owner: u32, count: u32) -> Result<u32, Error> {
    if (owner == 0) != (count == 0) {
        return Err(Error::Invalid);
    }
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let handle = alloc_slot(r).ok_or(Error::Init(libc::ENOMEM))?;
    let slot = r.slot_ptr(handle & INDEX_MASK);
    unsafe {
        (*slot).obj_type = TYPE_MUTEX;
        (*slot).pid = libc::getpid() as u32;
        (*slot).a = count;
        (*slot).b = owner;
        (*slot).c = 0;
    }
    Ok(handle)
}

pub fn create_event(manual: bool, signaled: bool) -> Result<u32, Error> {
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let handle = alloc_slot(r).ok_or(Error::Init(libc::ENOMEM))?;
    let slot = r.slot_ptr(handle & INDEX_MASK);
    unsafe {
        (*slot).obj_type = TYPE_EVENT;
        (*slot).pid = libc::getpid() as u32;
        (*slot).a = signaled as u32;
        (*slot).b = manual as u32;
        (*slot).d = 0;
    }
    Ok(handle)
}

pub fn close(handle: u32) -> bool {
    let Ok(r) = region() else { return false };
    let Ok(_g) = r.lock_guard() else { return false };
    let Some(slot) = r.slot(handle) else { return false };
    unsafe {
        (*slot).state = SLOT_FREE;
        (*slot).generation = (*slot).generation.wrapping_add(1);
    }
    r.bump_and_wake();
    true
}

/// Release `count` from a semaphore. Returns the previous count.
/// On overflow the state is left unchanged (kernel: -EOVERFLOW).
pub fn sem_release(handle: u32, count: u32) -> Result<u32, Error> {
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let slot = r.slot(handle).filter(|s| unsafe { (**s).obj_type } == TYPE_SEM).ok_or(Error::Invalid)?;
    unsafe {
        let sum = (*slot)
            .a
            .checked_add(count)
            .filter(|&sum| sum <= (*slot).b)
            .ok_or(Error::Overflow)?;
        let prev = (*slot).a;
        (*slot).a = sum;
        r.bump_and_wake();
        Ok(prev)
    }
}

pub fn event_set(handle: u32) -> Result<u32, Error> {
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let slot = r.slot(handle).filter(|s| unsafe { (**s).obj_type } == TYPE_EVENT).ok_or(Error::Invalid)?;
    let prev = unsafe {
        let prev = (*slot).a;
        (*slot).a = 1;
        prev
    };
    r.bump_and_wake();
    Ok(prev)
}

pub fn event_reset(handle: u32) -> Result<u32, Error> {
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let slot = r.slot(handle).filter(|s| unsafe { (**s).obj_type } == TYPE_EVENT).ok_or(Error::Invalid)?;
    let prev = unsafe {
        let prev = (*slot).a;
        (*slot).a = 0;
        prev
    };
    r.bump_and_wake();
    Ok(prev)
}

pub fn event_pulse(handle: u32) -> Result<u32, Error> {
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let slot = r.slot(handle).filter(|s| unsafe { (**s).obj_type } == TYPE_EVENT).ok_or(Error::Invalid)?;
    let prev = unsafe {
        let prev = (*slot).a;
        // Wake all current waiters, then return to unsignaled.
        (*slot).d = (*slot).d.wrapping_add(1);
        (*slot).a = 0;
        prev
    };
    r.bump_and_wake();
    Ok(prev)
}

/// Unlock a mutex held by `owner`. Returns the previous recursion count.
pub fn mutex_unlock(handle: u32, owner: u32) -> Result<u32, Error> {
    if owner == 0 {
        return Err(Error::Invalid);
    }
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let slot = r.slot(handle).filter(|s| unsafe { (**s).obj_type } == TYPE_MUTEX).ok_or(Error::Invalid)?;
    unsafe {
        if (*slot).b != owner {
            return Err(Error::Perm);
        }
        let prev = (*slot).a;
        (*slot).a -= 1;
        if (*slot).a == 0 {
            (*slot).b = 0;
        }
        r.bump_and_wake();
        Ok(prev)
    }
}

/// Mark a mutex abandoned and release ownership (kernel: MUTEX_KILL).
pub fn mutex_kill(handle: u32, owner: u32) -> Result<(), Error> {
    if owner == 0 {
        return Err(Error::Invalid);
    }
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let slot = r.slot(handle).filter(|s| unsafe { (**s).obj_type } == TYPE_MUTEX).ok_or(Error::Invalid)?;
    unsafe {
        if (*slot).b != owner {
            return Err(Error::Perm);
        }
        (*slot).c = 1;
        (*slot).b = 0;
        (*slot).a = 0;
        r.bump_and_wake();
        Ok(())
    }
}

/// Returns (count, max).
pub fn read_sem(handle: u32) -> Result<(u32, u32), Error> {
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let slot = r.slot(handle).filter(|s| unsafe { (**s).obj_type } == TYPE_SEM).ok_or(Error::Invalid)?;
    Ok(unsafe { ((*slot).a, (*slot).b) })
}

/// Returns (count, owner, owner_dead).
pub fn read_mutex(handle: u32) -> Result<(u32, u32, bool), Error> {
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let slot = r.slot(handle).filter(|s| unsafe { (**s).obj_type } == TYPE_MUTEX).ok_or(Error::Invalid)?;
    Ok(unsafe { ((*slot).a, (*slot).b, (*slot).c != 0) })
}

/// Returns (manual, signaled).
pub fn read_event(handle: u32) -> Result<(bool, bool), Error> {
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let slot = r.slot(handle).filter(|s| unsafe { (**s).obj_type } == TYPE_EVENT).ok_or(Error::Invalid)?;
    Ok(unsafe { ((*slot).b != 0, (*slot).a != 0) })
}

/// Check whether an object is acquirable ("locked" in kernel terms) without
/// mutating it.
fn is_locked(slot: &Slot, owner: u32, entry_seq: u64) -> bool {
    match slot.obj_type {
        TYPE_SEM => slot.a > 0,
        TYPE_MUTEX => (slot.b == 0 || slot.b == owner) && slot.a < u32::MAX,
        TYPE_EVENT => slot.a != 0 || slot.d != entry_seq,
        _ => false,
    }
}

/// Attempt to acquire an object. Returns Some(owner_dead) on success, None if
/// not acquirable.
fn try_acquire(slot: &mut Slot, owner: u32, entry_seq: u64) -> Option<bool> {
    match slot.obj_type {
        TYPE_SEM => {
            if slot.a > 0 {
                slot.a -= 1;
                Some(false)
            } else {
                None
            }
        }
        TYPE_MUTEX => {
            if (slot.b == 0 || slot.b == owner) && slot.a < u32::MAX {
                let owner_dead = if slot.b == 0 {
                    let od = slot.c != 0;
                    slot.c = 0;
                    od
                } else {
                    false
                };
                slot.a += 1;
                slot.b = owner;
                Some(owner_dead)
            } else {
                None
            }
        }
        TYPE_EVENT => {
            if slot.a != 0 {
                if slot.b == 0 {
                    // auto-reset
                    slot.a = 0;
                }
                Some(false)
            } else if slot.d != entry_seq {
                // Pulsed during the wait; considered satisfied (pulse leaves
                // the event unsignaled, so nothing to reset).
                Some(false)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `alert` is the optional alertable-wait event handle (0 = none), matching
/// the kernel's ntsync_wait_args.alert: if it is (or becomes) signaled, the
/// wait completes with index == handles.len(). The alert event is only
/// tested, never acquired (wineserver resets it when the APC queue empties).
/// Log to stderr (kept dependency-free so ntdll.so/wineserver do not need liblog).
fn debug_log(msg: &str) {
    eprintln!("{msg}");
}

fn debug_enabled() -> bool {
    static ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| {
        let on = std::env::var_os("NTSYNC_DEBUG")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);
        if on {
            debug_log(&format!("watchdog armed (pid {})", std::process::id()));
        }
        on
    })
}

/// Dump the state of every object involved in a stuck wait, to distinguish
/// a lost wakeup (object signaled while we sleep) from a never-coming signal.
fn debug_dump_stuck_wait(
    r: &Region,
    handles: &[u32],
    owner: u32,
    alert: u32,
    all: bool,
    start: std::time::Instant,
) {
    let type_name = |t: u32| match t {
        TYPE_EVENT => "event",
        TYPE_MUTEX => "mutex",
        TYPE_SEM => "sem",
        _ => "?",
    };
    debug_log(&format!(
        "stuck wait: {:?} elapsed, {} owner={} seq={} alert={:#x}",
        start.elapsed(),
        if all { "all" } else { "any" },
        owner,
        r.seq(),
        alert,
    ));
    if let Ok(_g) = r.lock_guard() {
        for h in handles.iter().copied().chain((alert != 0).then_some(alert)) {
            match r.slot(h) {
                Some(slot) => unsafe {
                    let s = &*slot;
                    let locked = is_locked(s, owner, s.d);
                    debug_log(&format!(
                        "  {:#010x} {} state={} a={} b={} c={} d={} pid={} gen={} now_signaled={}",
                        h, type_name(s.obj_type), s.state, s.a, s.b, s.c, s.d, s.pid, s.generation, locked,
                    ));
                },
                None => debug_log(&format!("  {:#010x} <closed/invalid>", h)),
            }
        }
    }
}

pub fn wait_any(handles: &[u32], owner: u32, timeout: Option<Duration>, alert: u32) -> WaitOutcome {
    wait(handles, owner, timeout, false, alert)
}

pub fn wait_all(handles: &[u32], owner: u32, timeout: Option<Duration>, alert: u32) -> WaitOutcome {
    wait(handles, owner, timeout, true, alert)
}

fn wait(handles: &[u32], owner: u32, timeout: Option<Duration>, all: bool, alert: u32) -> WaitOutcome {
    if owner == 0
        || handles.is_empty()
        || handles.len() > MAX_WAIT_COUNT
        || handles.iter().collect::<std::collections::HashSet<_>>().len() != handles.len()
    {
        return WaitOutcome::Invalid;
    }
    let Ok(r) = region() else {
        return WaitOutcome::Invalid;
    };

    // Validate the alert event handle up front.
    if alert != 0 {
        let Ok(_g) = r.lock_guard() else {
            return WaitOutcome::Invalid;
        };
        let valid = match r.slot(alert) {
            Some(slot) => unsafe { (*slot).obj_type == TYPE_EVENT },
            None => false,
        };
        if !valid {
            return WaitOutcome::Invalid;
        }
    }

    let deadline = timeout.map(|d| std::time::Instant::now() + d);
    let debug = debug_enabled();
    let start = std::time::Instant::now();
    let mut last_dump = start;

    // Validate handles and snapshot event pulse sequences at wait entry.
    let mut entry_seqs = [0u64; MAX_WAIT_COUNT];
    {
        let Ok(_g) = r.lock_guard() else {
            return WaitOutcome::Invalid;
        };
        for (i, h) in handles.iter().enumerate() {
            let Some(slot) = r.slot(*h) else {
                return WaitOutcome::Invalid;
            };
            unsafe {
                if (*slot).obj_type == TYPE_EVENT {
                    entry_seqs[i] = (*slot).d;
                }
            }
        }
    }

    loop {
        let seq;
        {
            let Ok(_g) = r.lock_guard() else {
                return WaitOutcome::Invalid;
            };

            // Re-validate: objects may have been closed while waiting.
            if handles.iter().any(|h| r.slot(*h).is_none()) {
                return WaitOutcome::Invalid;
            }

            // Alertable wait: the alert event wins immediately if signaled.
            if alert != 0 {
                let Some(slot) = r.slot(alert) else {
                    return WaitOutcome::Invalid;
                };
                if unsafe { (*slot).a != 0 } {
                    return WaitOutcome::Signaled {
                        index: handles.len() as u32,
                        owner_dead: false,
                    };
                }
            }

            if !all {
                for (i, h) in handles.iter().enumerate() {
                    let slot = r.slot_ptr(h & INDEX_MASK);
                    if let Some(owner_dead) =
                        try_acquire(unsafe { &mut *slot }, owner, entry_seqs[i])
                    {
                        return WaitOutcome::Signaled {
                            index: i as u32,
                            owner_dead,
                        };
                    }
                }
            } else {
                let satisfied = handles.iter().enumerate().all(|(i, h)| {
                    is_locked(unsafe { &*r.slot_ptr(h & INDEX_MASK) }, owner, entry_seqs[i])
                });
                if satisfied {
                    let mut owner_dead = false;
                    for (i, h) in handles.iter().enumerate() {
                        if let Some(od) =
                            try_acquire(unsafe { &mut *r.slot_ptr(h & INDEX_MASK) }, owner, entry_seqs[i])
                        {
                            owner_dead |= od;
                        }
                    }
                    return WaitOutcome::Signaled {
                        index: 0,
                        owner_dead,
                    };
                }
            }
            seq = r.seq();
        } // unlock

        let remaining = match deadline {
            Some(dl) => {
                let now = std::time::Instant::now();
                if now >= dl {
                    return WaitOutcome::Timeout;
                }
                Some(dl - now)
            }
            None => None,
        };

        // With NTSYNC_DEBUG=1, cap each futex wait at 5s so a stuck wait
        // periodically dumps the object states to stderr.
        let slice = if debug {
            Some(remaining.map_or(Duration::from_secs(5), |r| r.min(Duration::from_secs(5))))
        } else {
            remaining
        };

        match r.wait_seq(seq, slice) {
            0 | libc::EAGAIN => {}
            x if x == libc::ETIMEDOUT => {
                if deadline.is_some_and(|dl| std::time::Instant::now() >= dl) {
                    return WaitOutcome::Timeout;
                }
                if debug && last_dump.elapsed() >= Duration::from_secs(5) {
                    debug_dump_stuck_wait(r, handles, owner, alert, all, start);
                    last_dump = std::time::Instant::now();
                }
            }
            _ => {} // EINTR and friends: loop and re-check
        }
    }
}

/// Free all objects created by processes that no longer exist.
/// Userspace has no fd-close-on-death hook, so callers (e.g. a launcher or
/// wineserver replacement) should run this after a process exits.
pub fn sweep_dead() -> Result<u32, Error> {
    let r = region()?;
    let _g = r.lock_guard().map_err(Error::Init)?;
    let mut freed = 0;
    for index in 0..SLOT_COUNT {
        let slot = r.slot_ptr(index);
        unsafe {
            if (*slot).state == SLOT_USED && (*slot).pid != 0 {
                let ret = libc::kill((*slot).pid as i32, 0);
                if ret != 0 && errno() == libc::ESRCH {
                    (*slot).state = SLOT_FREE;
                    (*slot).generation = (*slot).generation.wrapping_add(1);
                    freed += 1;
                }
            }
        }
    }
    if freed > 0 {
        r.bump_and_wake();
    }
    Ok(freed)
}
