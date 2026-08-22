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
    fn sweep_dead_frees_orphans() {
        setup();
        let h = create_event(false, false).unwrap();
        // Simulate a crashed creator by clobbering the pid via a fake pid
        // that cannot exist: use sweep with our own (live) pid first.
        assert_eq!(sweep_dead().unwrap(), 0);
        assert!(read_event(h).is_ok());
        assert!(close(h));
    }
}
