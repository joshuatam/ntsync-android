// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (C) 2026 Joshua Tam <297250+joshuatam@users.noreply.github.com>

//! Soak tests for the dead-owner sweep, run with the interval cranked down so
//! passes happen constantly instead of once per 30 s.
//!
//! ```sh
//! NTSYNC_SWEEP_INTERVAL_SEC=1 cargo test --release -- --ignored --nocapture soak --test-threads=1
//! ```
//!
//! What is being hunted:
//!
//! - a stale reap corrupting a live object's waiter list (freed slot reused
//!   while the sweep was unlocked, then detached against it) — shows up as a
//!   canary object that stops signalling or a wait that never returns;
//! - double-insertion into the node free list (a node freed twice, or freed
//!   while still on a waiter's own chain) — shows up as two waiters handed the
//!   same node, i.e. a wait that hangs or a wake that never lands;
//! - object lifetime after the creator dies: a handle relayed to another
//!   process is reaped out from under the holder, which the kernel driver does
//!   not do. Recorded, not asserted: it is a design difference, and knowing
//!   whether it happens tells us what a relayed handle can be trusted for.
//!
//! The parent stays single-threaded across every `fork`, so a child never
//! inherits a region mutex held by a thread that does not exist in it (the
//! hazard that makes the ordinary suite serial).

use crate::core::*;
use std::sync::Once;
use std::time::{Duration, Instant};

static INIT: Once = Once::new();

fn setup() {
    INIT.call_once(|| {
        let path = std::env::temp_dir().join(format!("ntsync-soak-{}.shm", std::process::id()));
        let _ = std::fs::remove_file(&path);
        init(Some(path.to_str().unwrap())).unwrap();
    });
}

fn me() -> u32 {
    std::process::id()
}

/// Drive enough waits to pass the 256-call sweep gate, with timeouts so each
/// one takes the ETIMEDOUT path where the opportunistic sweep lives.
fn trigger_sweeps(ev: u32, calls: usize) {
    for _ in 0..calls {
        let _ = wait_any(&[ev], me(), Some(Duration::from_millis(1)), 0);
    }
}

/// Canary: a parent-owned set of primitives that must survive every sweep
/// untouched, because their creator (this process) is alive.
fn check_canaries(tag: &str) {
    let ev = create_event(false, false).unwrap();
    let sem = create_semaphore(1, 8).unwrap();
    let mtx = create_mutex(0, 0).unwrap();

    // read_event returns (manual, signaled).
    assert!(event_set(ev).is_ok(), "{tag}: event_set failed");
    assert!(read_event(ev).unwrap().1, "{tag}: event not signaled");
    assert!(event_reset(ev).is_ok(), "{tag}: event_reset failed");
    assert!(!read_event(ev).unwrap().1, "{tag}: event still signaled");

    assert_eq!(sem_release(sem, 2).unwrap(), 1, "{tag}: sem count wrong");
    assert_eq!(read_sem(sem).unwrap().0, 3, "{tag}: sem value wrong");

    assert!(
        matches!(
            wait_any(&[mtx], me(), Some(Duration::ZERO), 0),
            WaitOutcome::Signaled { index: 0, .. }
        ),
        "{tag}: mutex acquire failed"
    );
    assert!(mutex_unlock(mtx, me()).is_ok(), "{tag}: mutex release failed");

    assert!(close(ev) && close(sem) && close(mtx), "{tag}: close failed");
}

/// Repeated process death under a fast sweep: the sweep must never damage a
/// live owner's objects, hang, or hand out a corrupted node.
#[test]
#[ignore]
fn soak_process_death_under_sweep() {
    setup();
    const CYCLES: usize = 40;
    const ORPHANS: usize = 64;

    let trigger = create_event(false, false).unwrap();
    let started = Instant::now();
    for cycle in 0..CYCLES {
        check_canaries(&format!("cycle {cycle} pre"));

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Child: leave objects and registered waiter nodes behind, with no
            // cleanup at all, and with threads still blocked at exit.
            let mut orphans = Vec::with_capacity(ORPHANS);
            for _ in 0..ORPHANS {
                match create_event(false, false) {
                    Ok(h) => orphans.push(h),
                    Err(_) => break,
                }
            }
            let blocker = match create_event(false, false) {
                Ok(h) => h,
                Err(_) => unsafe { libc::_exit(0) },
            };
            for _ in 0..2 {
                std::thread::spawn(move || {
                    let _ = wait_any(&[blocker], unsafe { libc::getpid() } as u32, None, 0);
                });
            }
            // Let the waiters register their nodes, then die without unwinding.
            std::thread::sleep(Duration::from_millis(5));
            unsafe { libc::_exit(0) }
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };

        // Reap them via the opportunistic path, several passes per cycle.
        trigger_sweeps(trigger, 768);

        check_canaries(&format!("cycle {cycle} post"));
        eprintln!("soak: cycle {cycle}/{CYCLES} ok ({:?})", started.elapsed());
    }
    let _ = close(trigger);
}

/// Handle relay across process death: what a relayed handle is worth after the
/// creator exits. Recorded per phase rather than asserted, because it is a
/// design difference from the kernel driver (fd keeps the object alive).
#[test]
#[ignore]
fn soak_relayed_handle_after_creator_death() {
    setup();
    let trigger = create_event(false, false).unwrap();

    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (rd, wr) = (fds[0], fds[1]);

    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // Child: create an object and hand the handle to the parent.
        let h = create_event(false, false).unwrap();
        let bytes = h.to_ne_bytes();
        unsafe {
            libc::write(wr, bytes.as_ptr() as *const _, 4);
            libc::_exit(0)
        }
    }
    unsafe { libc::close(wr) };
    let mut bytes = [0u8; 4];
    let n = unsafe { libc::read(rd, bytes.as_mut_ptr() as *mut _, 4) };
    assert_eq!(n, 4);
    unsafe { libc::close(rd) };
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let relayed = u32::from_ne_bytes(bytes);

    println!(
        "soak: relayed handle {relayed:#x} from dead pid {}, creator-death semantics:",
        pid
    );
    println!("  before any sweep:  usable = {}", read_event(relayed).is_ok());
    println!("  signal it:         ok     = {}", event_set(relayed).is_ok());

    // Force an opportunistic sweep: still alive, so reaping must not happen.
    trigger_sweeps(trigger, 512);
    println!("  after one sweep:   usable = {}", read_event(relayed).is_ok());

    let _ = close(trigger);
}
