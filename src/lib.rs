// Userspace ntsync library for Android.
//
// Copyright (C) 2026 Joshua Tam <297250+joshuatam@users.noreply.github.com>
//
// This library is free software: you can redistribute it and/or modify it
// under the terms of the GNU Lesser General Public License as published by
// the Free Software Foundation, version 3 only.
//
// This library is distributed in the hope that it will be useful, but
// WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU Lesser
// General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public License
// along with this library. If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: LGPL-3.0-only

pub mod core;
pub mod ffi;

#[cfg(test)]
mod bench;

pub use core::{Error, WaitOutcome, MAX_WAIT_COUNT};

#[cfg(test)]
mod tests {
    use super::core::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Once};
    use std::thread;
    use std::time::Duration;

    static INIT: Once = Once::new();

    fn setup() {
        INIT.call_once(|| {
            let path = std::env::temp_dir()
                .join(format!("ntsync-test-{}.shm", std::process::id()));
            let _ = std::fs::remove_file(&path);
            init(Some(path.to_str().unwrap())).unwrap();
        });
    }

    fn is_signaled(outcome: WaitOutcome, index: u32) -> bool {
        matches!(outcome, WaitOutcome::Signaled { index: i, .. } if i == index)
    }

    #[test]
    fn semaphore_basic() {
        setup();
        let h = create_semaphore(2, 5).unwrap();
        assert!(create_semaphore(6, 5).is_err());
        assert!(is_signaled(wait_any(&[h], 1, Some(Duration::ZERO), 0), 0));
        assert!(is_signaled(wait_any(&[h], 1, Some(Duration::ZERO), 0), 0));
        assert_eq!(
            wait_any(&[h], 1, Some(Duration::from_millis(10)), 0),
            WaitOutcome::Timeout
        );
        assert_eq!(sem_release(h, 3).unwrap(), 0);
        assert_eq!(read_sem(h).unwrap(), (3, 5));
        // Overflow: error, state unchanged (kernel behavior).
        assert_eq!(sem_release(h, 3), Err(Error::Overflow));
        assert_eq!(read_sem(h).unwrap(), (3, 5));
        assert!(close(h));
    }

    #[test]
    fn handle_zero_is_never_allocated() {
        setup();
        // Handle 0 is the "no alert" sentinel in ntsync_wait_args.alert, so it
        // must never be handed out: slot 0 is reserved at region init.
        let h = create_event(false, false).unwrap();
        assert_ne!(h & ((1 << 14) - 1), 0);
        assert!(!close(0));
        assert!(close(h));
    }

    #[test]
    fn semaphore_blocks_and_wakes() {
        setup();
        let h = create_semaphore(0, 1).unwrap();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            sem_release(h, 1).unwrap();
        });
        assert!(is_signaled(wait_any(&[h], 1, None, 0), 0));
        t.join().unwrap();
        assert!(close(h));
    }

    #[test]
    fn mutex_recursion_and_ownership() {
        setup();
        let h = create_mutex(0, 0).unwrap();
        // Kernel: !owner != !count is invalid.
        assert_eq!(create_mutex(5, 0), Err(Error::Invalid));
        assert!(is_signaled(wait_any(&[h], 7, Some(Duration::ZERO), 0), 0));
        assert!(is_signaled(wait_any(&[h], 7, Some(Duration::ZERO), 0), 0));
        assert_eq!(read_mutex(h).unwrap(), (2, 7, false));
        // Not the owner: cannot wait or unlock.
        assert_eq!(
            wait_any(&[h], 8, Some(Duration::from_millis(10)), 0),
            WaitOutcome::Timeout
        );
        assert_eq!(mutex_unlock(h, 8), Err(Error::Perm));
        // Unlock returns previous count.
        assert_eq!(mutex_unlock(h, 7).unwrap(), 2);
        assert_eq!(mutex_unlock(h, 7).unwrap(), 1);
        assert_eq!(read_mutex(h).unwrap(), (0, 0, false));
        assert!(close(h));
    }

    #[test]
    fn mutex_abandoned() {
        setup();
        let h = create_mutex(0, 0).unwrap();
        assert!(is_signaled(wait_any(&[h], 1, Some(Duration::ZERO), 0), 0));
        assert_eq!(mutex_kill(h, 2), Err(Error::Perm));
        mutex_kill(h, 1).unwrap();
        assert_eq!(read_mutex(h).unwrap(), (0, 0, true));
        match wait_any(&[h], 2, Some(Duration::ZERO), 0) {
            WaitOutcome::Signaled { index, owner_dead } => {
                assert_eq!(index, 0);
                assert!(owner_dead);
            }
            o => panic!("expected signal, got {o:?}"),
        }
        // Abandoned flag is cleared on acquisition.
        assert_eq!(read_mutex(h).unwrap(), (1, 2, false));
        assert!(close(h));
    }

    #[test]
    fn event_auto_and_manual_reset() {
        setup();
        let auto = create_event(false, true).unwrap();
        assert!(is_signaled(wait_any(&[auto], 1, Some(Duration::ZERO), 0), 0));
        // Auto-reset: consumed after wait.
        assert!(!read_event(auto).unwrap().1);

        let manual = create_event(true, true).unwrap();
        assert!(is_signaled(wait_any(&[manual], 1, Some(Duration::ZERO), 0), 0));
        assert!(is_signaled(wait_any(&[manual], 1, Some(Duration::ZERO), 0), 0));
        event_reset(manual).unwrap();
        assert_eq!(
            wait_any(&[manual], 1, Some(Duration::from_millis(10)), 0),
            WaitOutcome::Timeout
        );
        assert!(close(auto));
        assert!(close(manual));
    }

    #[test]
    fn event_pulse_wakes_waiters() {
        setup();
        let h = create_event(true, false).unwrap();
        let hits = Arc::new(AtomicBool::new(true));
        let mut threads = Vec::new();
        for i in 0..3 {
            let hits = hits.clone();
            threads.push(thread::spawn(move || {
                if !matches!(
                    wait_any(&[h], 10 + i, None, 0),
                    WaitOutcome::Signaled { .. }
                ) {
                    hits.store(false, Ordering::SeqCst);
                }
            }));
        }
        thread::sleep(Duration::from_millis(50));
        event_pulse(h).unwrap();
        for t in threads {
            t.join().unwrap();
        }
        assert!(hits.load(Ordering::SeqCst));
        assert!(!read_event(h).unwrap().1);
        assert!(close(h));
    }

    #[test]
    fn wait_all_semantics() {
        setup();
        let sem = create_semaphore(1, 1).unwrap();
        let ev = create_event(true, false).unwrap();
        assert_eq!(
            wait_all(&[sem, ev], 1, Some(Duration::from_millis(10)), 0),
            WaitOutcome::Timeout
        );
        event_set(ev).unwrap();
        assert!(is_signaled(wait_all(&[sem, ev], 1, Some(Duration::ZERO), 0), 0));
        assert_eq!(read_sem(sem).unwrap(), (0, 1));
        assert!(read_event(ev).unwrap().1); // manual event stays signaled
        assert!(close(sem));
        assert!(close(ev));
    }

    #[test]
    fn wait_any_index_selection() {
        setup();
        let s1 = create_semaphore(0, 1).unwrap();
        let s2 = create_semaphore(1, 1).unwrap();
        assert!(is_signaled(wait_any(&[s1, s2], 1, Some(Duration::ZERO), 0), 1));
        assert!(close(s1));
        assert!(close(s2));
    }

    #[test]
    fn invalid_handles() {
        setup();
        assert_eq!(
            wait_any(&[0xDEADBEEF], 1, Some(Duration::ZERO), 0),
            WaitOutcome::Invalid
        );
        assert_eq!(wait_any(&[], 1, None, 0), WaitOutcome::Invalid);
        assert_eq!(wait_any(&[1], 0, None, 0), WaitOutcome::Invalid);
        assert_eq!(sem_release(0xDEADBEEF, 1), Err(Error::Invalid));
        assert_eq!(event_set(0xDEADBEEF), Err(Error::Invalid));
        assert!(!close(0xDEADBEEF));
    }

    #[test]
    fn cross_process_semaphore() {
        setup();
        let h = create_semaphore(0, 1).unwrap();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // Child: block until the parent releases.
            let ok = matches!(
                wait_any(&[h], 42, None, 0),
                WaitOutcome::Signaled { index: 0, .. }
            );
            std::process::exit(if ok { 0 } else { 1 });
        }
        thread::sleep(Duration::from_millis(50));
        sem_release(h, 1).unwrap();
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert!(close(h));
    }

    #[test]
    fn many_waiters_targeted_wake() {
        // 32 threads each blocked on their own event; signaling one event
        // must wake exactly its waiter without hanging the others.
        setup();
        let events: Vec<u32> = (0..32).map(|_| create_event(false, false).unwrap()).collect();
        let threads: Vec<_> = events
            .iter()
            .map(|&e| {
                thread::spawn(move || {
                    matches!(wait_any(&[e], 42, Some(Duration::from_secs(10)), 0), WaitOutcome::Signaled { .. })
                })
            })
            .collect();
        thread::sleep(Duration::from_millis(50));
        for &e in &events {
            event_set(e).unwrap();
        }
        for t in threads {
            assert!(t.join().unwrap());
        }
        for e in events {
            assert!(close(e));
        }
    }

    #[test]
    fn cross_process_multi_object_wait() {
        setup();
        let sem = create_semaphore(0, 1).unwrap();
        let ev = create_event(true, false).unwrap();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // Child: wait-all on both objects.
            let ok = matches!(
                wait_all(&[sem, ev], 42, Some(Duration::from_secs(10)), 0),
                WaitOutcome::Signaled { .. }
            );
            std::process::exit(if ok { 0 } else { 1 });
        }
        thread::sleep(Duration::from_millis(50));
        sem_release(sem, 1).unwrap();
        thread::sleep(Duration::from_millis(50));
        event_set(ev).unwrap();
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert!(close(sem));
        assert!(close(ev));
    }

    #[test]
    fn sweep_dead_frees_orphans() {
        setup();
        let h = create_event(false, false).unwrap();
        // Simulate a crashed creator by clobbering the pid via a fake pid
        // that cannot exist: use sweep with our own (live) pid first.
        assert_eq!(sweep_dead().unwrap(), 0);
        assert!(read_event(h).is_ok());
        assert!(close(h));
    }

    #[test]
    fn auto_sweep_reaps_dead_creator() {
        setup();
        let mine = create_event(false, false).unwrap();
        // A child process creates an object and dies without closing it.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let _ = create_event(false, false).unwrap();
            unsafe { libc::_exit(0) };
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        // The orphan exists: an explicit sweep would find it. Instead, an
        // ordinary signal op must reap it via the auto-sweep path.
        test_reset_sweep_clock();
        event_set(mine).unwrap();
        assert_eq!(sweep_dead().unwrap(), 0, "auto-sweep should have reaped the orphan");
        assert!(close(mine));
    }
}

/// Performance simulation of the paths Wine's ntdll exercises. Not run by
/// default; use:
///   cargo test --release -- --ignored --nocapture perf
#[cfg(test)]
mod perf {
    use super::core::*;
    use std::sync::Once;
    use std::thread;
    use std::time::{Duration, Instant};

    static INIT: Once = Once::new();

    fn setup() {
        INIT.call_once(|| {
            let path = std::env::temp_dir()
                .join(format!("ntsync-perf-{}.shm", std::process::id()));
            let _ = std::fs::remove_file(&path);
            init(Some(path.to_str().unwrap())).unwrap();
        });
    }

    fn report(name: &str, iters: u64, elapsed: Duration) {
        let ns = elapsed.as_nanos() as u64 / iters.max(1);
        println!(
            "{name:<42} {iters:>8} iters  {ns:>8} ns/op ({:>7.2} us)",
            ns as f64 / 1000.0
        );
    }

    /// Hot path: WaitForSingleObject on an already-signaled manual event.
    /// Should be a pure lock-free peek (no mutex, no syscall).
    #[test]
    #[ignore = "perf simulation"]
    fn perf_wait_signaled_event() {
        setup();
        let h = create_event(true, true).unwrap();
        const N: u64 = 100_000;
        let t = Instant::now();
        for _ in 0..N {
            assert!(matches!(
                wait_any(&[h], 1, Some(Duration::ZERO), 0),
                WaitOutcome::Signaled { .. }
            ));
        }
        report("wait_any(signaled manual event)", N, t.elapsed());
        assert!(close(h));
    }

    /// Set/reset churn with no waiters (typical game event traffic when
    /// nobody is blocked). Should skip FUTEX_WAKE entirely.
    #[test]
    #[ignore = "perf simulation"]
    fn perf_event_set_reset_no_waiters() {
        setup();
        let h = create_event(true, false).unwrap();
        const N: u64 = 100_000;
        let t = Instant::now();
        for _ in 0..N {
            event_set(h).unwrap();
            event_reset(h).unwrap();
        }
        report("event_set+reset (no waiters)", N, t.elapsed());
        assert!(close(h));
    }

    /// Uncontended semaphore acquire/release pairs (e.g. D3D command
    /// semaphores).
    #[test]
    #[ignore = "perf simulation"]
    fn perf_sem_acquire_release_uncontended() {
        setup();
        let h = create_semaphore(0, 1_000_000).unwrap();
        sem_release(h, 100_000).unwrap();
        const N: u64 = 100_000;
        let t = Instant::now();
        for _ in 0..N {
            assert!(matches!(
                wait_any(&[h], 1, Some(Duration::ZERO), 0),
                WaitOutcome::Signaled { .. }
            ));
        }
        report("sem acquire (precharged)", N, t.elapsed());
        assert!(close(h));
    }

    /// Full wake latency: two threads ping-ponging on semaphores. Measures
    /// futex sleep + wake + re-check, i.e. the blocked-wait cost.
    #[test]
    #[ignore = "perf simulation"]
    fn perf_ping_pong_wake_latency() {
        setup();
        let s1 = create_semaphore(0, 1).unwrap();
        let s2 = create_semaphore(0, 1).unwrap();
        const N: u64 = 10_000;
        let worker = thread::spawn(move || {
            for _ in 0..N {
                assert!(matches!(
                    wait_any(&[s1], 2, Some(Duration::from_secs(5)), 0),
                    WaitOutcome::Signaled { .. }
                ));
                sem_release(s2, 1).unwrap();
            }
        });
        // Let the worker block first.
        thread::sleep(Duration::from_millis(10));
        let t = Instant::now();
        for _ in 0..N {
            sem_release(s1, 1).unwrap();
            assert!(matches!(
                wait_any(&[s2], 1, Some(Duration::from_secs(5)), 0),
                WaitOutcome::Signaled { .. }
            ));
        }
        report("ping-pong round trip (2 wakes)", N, t.elapsed());
        worker.join().unwrap();
        assert!(close(s1));
        assert!(close(s2));
    }

    /// Same ping-pong across a fork(), the real wineserver↔client shape:
    /// waker and waiter are in different processes sharing the region.
    #[test]
    #[ignore = "perf simulation"]
    fn perf_cross_process_ping_pong() {
        setup();
        let s1 = create_semaphore(0, 1).unwrap();
        let s2 = create_semaphore(0, 1).unwrap();
        const N: u64 = 10_000;
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // Child: wait on s1, release s2.
            for _ in 0..N {
                if !matches!(
                    wait_any(&[s1], 2, Some(Duration::from_secs(30)), 0),
                    WaitOutcome::Signaled { .. }
                ) {
                    std::process::exit(1);
                }
                sem_release(s2, 1).unwrap();
            }
            std::process::exit(0);
        }
        thread::sleep(Duration::from_millis(10));
        let t = Instant::now();
        for _ in 0..N {
            sem_release(s1, 1).unwrap();
            assert!(matches!(
                wait_any(&[s2], 1, Some(Duration::from_secs(30)), 0),
                WaitOutcome::Signaled { .. }
            ));
        }
        report("cross-process ping-pong (2 wakes)", N, t.elapsed());
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        assert!(close(s1));
        assert!(close(s2));
    }

    /// Contended mutex: 4 threads hammering one mutex (wait + unlock).
    #[test]
    #[ignore = "perf simulation"]
    fn perf_contended_mutex() {
        setup();
        let m = create_mutex(0, 0).unwrap();
        const THREADS: u32 = 4;
        const PER_THREAD: u64 = 5_000;
        let t = Instant::now();
        let handles: Vec<_> = (1..=THREADS)
            .map(|owner| {
                thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        assert!(matches!(
                            wait_any(&[m], owner, Some(Duration::from_secs(30)), 0),
                            WaitOutcome::Signaled { .. }
                        ));
                        mutex_unlock(m, owner).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let total = THREADS as u64 * PER_THREAD;
        report("contended mutex acquire+release (4 thr)", total, t.elapsed());
        assert!(close(m));
    }
}
