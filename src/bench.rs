// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (C) 2026 Joshua Tam <297250+joshuatam@users.noreply.github.com>

//! Wine-workload simulation + CPU/memory profiling benchmarks.
//!
//! These are NOT correctness tests: they simulate the load shape of a real
//! game running under Wine/Proton (render thread, thread-pool workers,
//! contended mutexes, object churn, alertable and multi-object waits,
//! cross-process wineserver traffic) and record CPU time, context switches,
//! and shared-memory footprint. The point is to establish a *baseline* so
//! that later optimizations (lazy shm init, fewer syscalls on the lock
//! path, smaller region, ...) can be validated as real improvements rather
//! than regressions.
//!
//! Usage:
//!   # Run the suite and print the report (no baseline comparison unless
//!   # target/ntsync-bench-baseline.json already exists):
//!   cargo test --release -- --ignored --nocapture bench
//!
//!   # (Re)write the baseline from the current code:
//!   NTSYNC_BENCH_BASELINE=write cargo test --release -- --ignored --nocapture bench
//!
//!   # Fail the run if any metric regresses >20% vs the baseline:
//!   NTSYNC_BENCH_STRICT=1 cargo test --release -- --ignored --nocapture bench
//!
//! Every run also writes target/ntsync-bench-latest.json for external
//! tooling. All metrics are "lower is better".

use crate::core::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

static INIT: Once = Once::new();

/// rusage(RUSAGE_SELF) and proc RSS are process-wide, so benches must never
/// run concurrently: each one takes this lock for its whole duration. They
/// are also numbered so that, single-threaded, the footprint bench (most
/// sensitive to prior region state) runs first.
static BENCH_LOCK: Mutex<()> = Mutex::new(());

/// Unique stem for this bench process's shm file; also the substring used
/// to find the mapping in /proc/self/smaps (version-agnostic).
fn shm_stem() -> String {
    format!("ntsync-bench-{}", std::process::id())
}

fn setup() -> String {
    let stem = shm_stem();
    INIT.call_once(|| {
        let path = std::env::temp_dir().join(format!("{stem}.shm"));
        let _ = std::fs::remove_file(&path);
        init(Some(path.to_str().unwrap())).unwrap();
    });
    stem
}

// ---------------------------------------------------------------------------
// Metric collection
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Rusage {
    utime_us: i64,
    stime_us: i64,
    nvcsw: i64,   // voluntary context switches (futex sleeps, blocking IO)
    nivcsw: i64,  // involuntary context switches (preemption)
}

fn rusage(who: i32) -> Rusage {
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(who, &mut r) };
    let us = |tv: libc::timeval| tv.tv_sec as i64 * 1_000_000 + tv.tv_usec as i64;
    Rusage {
        utime_us: us(r.ru_utime),
        stime_us: us(r.ru_stime),
        nvcsw: r.ru_nvcsw as i64,
        nivcsw: r.ru_nivcsw as i64,
    }
}

fn proc_rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        }
    }
    0
}

/// (file size bytes, mapped Rss kB, Shared_Dirty kB, Private_Dirty kB) for
/// the shm region whose pathname contains `stem`.
fn shm_stats(stem: &str) -> (u64, u64, u64, u64) {
    let mut file_bytes = 0u64;
    let mut rss = 0u64;
    let mut shared_dirty = 0u64;
    let mut private_dirty = 0u64;
    // File size: find the actual file in temp_dir (name has .vN inserted).
    if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.contains(stem) {
                file_bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    let kb = |line: &str| -> u64 {
        line.split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    if let Ok(smaps) = std::fs::read_to_string("/proc/self/smaps") {
        let mut in_region = false;
        for line in smaps.lines() {
            // Region header lines contain the mapped pathname.
            if line.contains('-') && line.contains('/') {
                in_region = line.contains(stem);
                continue;
            }
            if !in_region {
                continue;
            }
            if line.starts_with("Rss:") {
                rss += kb(line);
            } else if line.starts_with("Shared_Dirty:") {
                shared_dirty += kb(line);
            } else if line.starts_with("Private_Dirty:") {
                private_dirty += kb(line);
            }
        }
    }
    (file_bytes, rss, shared_dirty, private_dirty)
}

// ---------------------------------------------------------------------------
// Baseline persistence + comparison (flat JSON: "bench.metric": number)
// ---------------------------------------------------------------------------

fn bench_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target")
}

fn baseline_path() -> std::path::PathBuf {
    bench_dir().join("ntsync-bench-baseline.json")
}

fn latest_path() -> std::path::PathBuf {
    bench_dir().join("ntsync-bench-latest.json")
}

fn to_json(map: &BTreeMap<String, u64>) -> String {
    let mut s = String::from("{\n");
    for (i, (k, v)) in map.iter().enumerate() {
        s.push_str(&format!(
            "  {k:?}: {v}{}\n",
            if i + 1 == map.len() { "" } else { "," }
        ));
    }
    s.push_str("}\n");
    s
}

/// Tolerant parser for the flat JSON we write: `  "key": 123,` per line.
fn from_json(text: &str) -> BTreeMap<String, u64> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim().trim_end_matches(',');
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().trim_matches('"');
            if let Ok(v) = v.trim().parse::<u64>() {
                map.insert(k.to_string(), v);
            }
        }
    }
    map
}

static RESULTS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

/// Record one sample's metrics under `bench_name` and update the
/// baseline/latest files. Prints a comparison table against the baseline.
fn record(bench_name: &str, metrics: &[(&str, u64)]) {
    {
        let mut g = RESULTS.lock().unwrap();
        for (k, v) in metrics {
            g.insert(format!("{bench_name}.{k}"), *v);
        }
        let write_baseline = std::env::var("NTSYNC_BENCH_BASELINE").as_deref() == Ok("write");
        let target = if write_baseline {
            baseline_path()
        } else {
            latest_path()
        };
        let _ = std::fs::create_dir_all(bench_dir());
        if let Err(e) = std::fs::write(&target, to_json(&g)) {
            eprintln!("bench: cannot write {}: {e}", target.display());
        }
    }
    compare(bench_name, metrics);
}

fn compare(bench_name: &str, metrics: &[(&str, u64)]) {
    if std::env::var("NTSYNC_BENCH_BASELINE").as_deref() == Ok("write") {
        println!("bench: baseline updated ({})", baseline_path().display());
        return;
    }
    let Ok(text) = std::fs::read_to_string(baseline_path()) else {
        println!(
            "bench: no baseline yet; create one with \
             NTSYNC_BENCH_BASELINE=write cargo test --release -- --ignored --nocapture bench"
        );
        return;
    };
    let base = from_json(&text);
    let strict = std::env::var("NTSYNC_BENCH_STRICT").as_deref() == Ok("1");
    println!("--- {bench_name}: vs baseline (lower is better) ---");
    let mut regressions = Vec::new();
    for (k, v) in metrics {
        let key = format!("{bench_name}.{k}");
        match base.get(&key) {
            Some(&b) => {
                let delta = if b == 0 {
                    if *v == 0 { 0.0 } else { f64::INFINITY }
                } else {
                    (*v as f64 - b as f64) / b as f64 * 100.0
                };
                // Percent deltas on tiny absolute values are pure noise
                // (e.g. involuntary ctxsw 4 -> 5 from scheduler jitter), so
                // require a minimum absolute change before flagging.
                let meaningful = v.abs_diff(b) >= 50;
                let mark = if meaningful && delta > 20.0 {
                    regressions.push(key.clone());
                    " REGRESSED"
                } else if meaningful && delta < -5.0 {
                    " improved"
                } else {
                    ""
                };
                println!("  {k:<24} base {b:>12}  now {v:>12}  {delta:+7.1}%{mark}");
            }
            _ => println!("  {k:<24} base          -  now {v:>12}  (new)"),
        }
    }
    if strict && !regressions.is_empty() {
        panic!("bench regressions >20% vs baseline: {regressions:?}");
    }
}

// ---------------------------------------------------------------------------
// Workload samples
// ---------------------------------------------------------------------------

/// Take a snapshot around a workload closure; returns per-op CPU cost and
/// contention indicators.
struct Sample {
    wall_us: u64,
    cpu_us: u64,
    ops: u64,
    nvcsw: u64,
    nivcsw: u64,
}

impl Sample {
    fn run(ops: &AtomicU64, f: impl FnOnce()) -> Sample {
        let r0 = rusage(libc::RUSAGE_SELF);
        let t = Instant::now();
        f();
        let wall = t.elapsed();
        let r1 = rusage(libc::RUSAGE_SELF);
        Sample {
            wall_us: wall.as_micros() as u64,
            cpu_us: (r1.utime_us - r0.utime_us + r1.stime_us - r0.stime_us).max(0) as u64,
            ops: ops.load(Ordering::Relaxed),
            nvcsw: (r1.nvcsw - r0.nvcsw).max(0) as u64,
            nivcsw: (r1.nivcsw - r0.nivcsw).max(0) as u64,
        }
    }

    fn metrics(&self) -> Vec<(&'static str, u64)> {
        let ops = self.ops.max(1);
        vec![
            ("ops", self.ops),
            ("wall_us", self.wall_us),
            ("cpu_us", self.cpu_us),
            ("cpu_ns_per_op", self.cpu_us * 1000 / ops),
            // >100% means the workload burned more than one core.
            ("cpu_ratio_pct", self.cpu_us * 100 / self.wall_us.max(1)),
            ("voluntary_ctxsw", self.nvcsw),
            ("involuntary_ctxsw", self.nivcsw),
            ("ctxsw_per_kop", self.nvcsw * 1000 / ops),
        ]
    }
}

fn report_footprint(stem: &str, phase: &str, bench: &str) {
    let (bytes, rss, sdirty, pdirty) = shm_stats(stem);
    record(
        bench,
        &[
            (&format!("shm_file_bytes_{phase}").leak(), bytes),
            (&format!("shm_rss_kb_{phase}").leak(), rss),
            (&format!("shm_shared_dirty_kb_{phase}").leak(), sdirty),
            (&format!("shm_private_dirty_kb_{phase}").leak(), pdirty),
            (&format!("proc_rss_kb_{phase}").leak(), proc_rss_kb()),
        ],
    );
    println!(
        "  [{phase}] shm file={bytes} B ({:.0} KiB), mapped RSS={rss} kB \
         (shared-dirty={sdirty} kB, private-dirty={pdirty} kB), proc RSS={} kB",
        bytes as f64 / 1024.0,
        proc_rss_kb()
    );
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Memory footprint of the shared region across its lifecycle. This is the
/// regression test for the 960 KiB region size and for any future lazy-init
/// work: today, init eagerly zeroes and node-links the whole file, so the
/// mapped RSS jumps to the full size immediately; a lazy implementation
/// should show a much smaller `shm_rss_kb_init`.
#[test]
#[ignore = "bench simulation"]
fn bench_1_shm_footprint() {
    let stem = setup();
    let _lock = BENCH_LOCK.lock().unwrap();
    let bench = "shm_footprint";
    report_footprint(&stem, "init", bench);

    // Phase 1: create a Wine-session-sized object set (a few thousand
    // events/semaphores/mutexes is typical for a busy game).
    let mut handles = Vec::new();
    for i in 0..2048 {
        handles.push(match i % 3 {
            0 => create_event(i % 6 == 0, false),
            1 => create_semaphore(0, 64),
            _ => create_mutex(0, 0),
        });
    }
    let handles: Vec<u32> = handles.into_iter().collect::<Result<_, _>>().unwrap();
    report_footprint(&stem, "objects_2048", bench);

    // Phase 2: park 256 waiters (registered nodes touch the node pool).
    let events: Vec<u32> = (0..256).map(|_| create_event(false, false).unwrap()).collect();
    let parked: Vec<_> = events
        .iter()
        .map(|&e| {
            thread::spawn(move || {
                let _ = wait_any(&[e], 7, Some(Duration::from_secs(30)), 0);
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(200));
    report_footprint(&stem, "waiters_256", bench);
    for &e in &events {
        event_set(e).unwrap();
    }
    for t in parked {
        t.join().unwrap();
    }
    for h in handles.iter().chain(events.iter()) {
        close(*h);
    }
}

/// CPU cost of the hottest Wine paths: signaled waits and signal churn.
/// These must stay syscall-free (near-zero voluntary ctxsw, low cpu/op).
#[test]
#[ignore = "bench simulation"]
fn bench_2_signaled_fastpath() {
    setup();
    let _lock = BENCH_LOCK.lock().unwrap();
    let bench = "signaled_fastpath";
    let ops = AtomicU64::new(0);
    let s = Sample::run(&ops, || {
        let ev = create_event(true, true).unwrap();
        let churn = create_event(true, false).unwrap();
        let sem = create_semaphore(0, 1_000_000).unwrap();
        sem_release(sem, 300_000).unwrap();
        for _ in 0..100_000u64 {
            assert!(matches!(
                wait_any(&[ev], 1, Some(Duration::ZERO), 0),
                WaitOutcome::Signaled { .. }
            ));
            event_set(churn).unwrap();
            event_reset(churn).unwrap();
            assert!(matches!(
                wait_any(&[sem], 1, Some(Duration::ZERO), 0),
                WaitOutcome::Signaled { .. }
            ));
            ops.fetch_add(4, Ordering::Relaxed);
        }
        close(ev);
        close(churn);
        close(sem);
    });
    for (k, v) in s.metrics() {
        println!("  {k:<24} {v}");
    }
    record(bench, &s.metrics());
    // Hard sanity gate: the fast path must never block. A regression that
    // adds a syscall/sleep here will explode this number.
    assert!(
        s.nvcsw < 1_000,
        "fast path caused {} voluntary context switches (expected ~0)",
        s.nvcsw
    );
}

/// The main "real Wine running" simulation: a sustained mixed load with the
/// thread roles a game actually has. Runs for a fixed wall duration and
/// measures total CPU burned and context-switch rate.
#[test]
#[ignore = "bench simulation"]
fn bench_4_wine_mixed_load() {
    setup();
    let _lock = BENCH_LOCK.lock().unwrap();
    let bench = "wine_mixed_load";
    let stop = Arc::new(AtomicBool::new(false));
    let ops = Arc::new(AtomicU64::new(0));

    // Shared objects, one per "subsystem".
    let job_sem = create_semaphore(0, 1 << 20).unwrap(); // thread-pool job queue
    let done_sem = create_semaphore(0, 1 << 20).unwrap();
    let frame_ev = create_event(true, false).unwrap(); // manual-reset frame barrier
    let vsync_ev = create_event(false, false).unwrap(); // auto-reset pacing
    let mtx = create_mutex(0, 0).unwrap(); // e.g. d3d device lock
    let alert_ev = create_event(false, false).unwrap(); // APC alert event
    let multi: Vec<u32> = (0..4).map(|_| create_event(false, false).unwrap()).collect();

    let mut threads = Vec::new();

    // 8 thread-pool workers: block on the job queue, "execute", signal done.
    for i in 0..8u32 {
        let (stop, ops) = (stop.clone(), ops.clone());
        threads.push(thread::spawn(move || {
            let owner = 100 + i;
            while !stop.load(Ordering::Relaxed) {
                match wait_any(&[job_sem], owner, Some(Duration::from_millis(20)), 0) {
                    WaitOutcome::Signaled { .. } => {
                        sem_release(done_sem, 1).unwrap();
                        ops.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }));
    }

    // Render thread @ ~120 fps: post a burst of jobs, pace with a timed
    // wait, flip the frame event, wait for workers to drain.
    {
        let (stop, ops) = (stop.clone(), ops.clone());
        threads.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = sem_release(job_sem, 8);
                event_set(frame_ev).unwrap();
                // Frame pacing: short timed wait on an unsignaled event
                // (the WaitForSingleObject(..., 1ms) pattern).
                let _ = wait_any(&[vsync_ev], 1, Some(Duration::from_millis(1)), 0);
                event_reset(frame_ev).unwrap();
                // Drain a few completions.
                let _ = wait_any(&[done_sem], 1, Some(Duration::from_millis(2)), 0);
                ops.fetch_add(5, Ordering::Relaxed);
            }
        }));
    }

    // 4 threads hammering one mutex (d3d/vulkan device lock contention).
    for i in 0..4u32 {
        let (stop, ops) = (stop.clone(), ops.clone());
        threads.push(thread::spawn(move || {
            let owner = 200 + i;
            while !stop.load(Ordering::Relaxed) {
                if matches!(
                    wait_any(&[mtx], owner, Some(Duration::from_millis(50)), 0),
                    WaitOutcome::Signaled { .. }
                ) {
                    mutex_unlock(mtx, owner).unwrap();
                    ops.fetch_add(2, Ordering::Relaxed);
                }
            }
        }));
    }

    // Object churn: games constantly create/destroy sync objects.
    {
        let (stop, ops) = (stop.clone(), ops.clone());
        threads.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut hs = Vec::with_capacity(64);
                for i in 0..64 {
                    let h = match i % 3 {
                        0 => create_event(false, false),
                        1 => create_semaphore(0, 4),
                        _ => create_mutex(0, 0),
                    };
                    if let Ok(h) = h {
                        hs.push(h);
                    }
                }
                for h in hs {
                    close(h);
                    ops.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }

    // Alertable waiter (ntdll APC pattern): waits with an alert event that
    // another thread pulses periodically.
    {
        let (stop, ops) = (stop.clone(), ops.clone());
        let alerter_stop = stop.clone();
        let alerter = thread::spawn(move || {
            while !alerter_stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(3));
                let _ = event_set(alert_ev);
                thread::sleep(Duration::from_millis(3));
                let _ = event_reset(alert_ev);
            }
        });
        threads.push(alerter);
        threads.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = wait_any(&[vsync_ev], 9, Some(Duration::from_millis(10)), alert_ev);
                ops.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // MsgWaitForMultipleObjects-style multi-wait with timeouts.
    {
        let (stop, ops) = (stop.clone(), ops.clone());
        let multi = multi.clone();
        threads.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = wait_any(&multi, 1, Some(Duration::from_millis(2)), 0);
                ops.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    let s = Sample::run(&ops, || {
        thread::sleep(Duration::from_secs(3));
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            t.join().unwrap();
        }
    });
    for (k, v) in s.metrics() {
        println!("  {k:<24} {v}");
    }
    record(bench, &s.metrics());

    close(job_sem);
    close(done_sem);
    close(frame_ev);
    close(vsync_ev);
    close(mtx);
    close(alert_ev);
    for e in multi {
        close(e);
    }
}

/// Cross-process wineserver↔client traffic: semaphore ping-pong across a
/// fork(), measuring CPU on both sides (parent + RUSAGE_CHILDREN).
#[test]
#[ignore = "bench simulation"]
fn bench_3_cross_process_rpc() {
    setup();
    let _lock = BENCH_LOCK.lock().unwrap();
    let bench = "cross_process_rpc";
    let s1 = create_semaphore(0, 1).unwrap();
    let s2 = create_semaphore(0, 1).unwrap();
    const N: u64 = 10_000;

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // Child (the "client"): wait on s1, answer on s2.
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

    thread::sleep(Duration::from_millis(20));
    let r0 = rusage(libc::RUSAGE_SELF);
    let c0 = rusage(libc::RUSAGE_CHILDREN);
    let t = Instant::now();
    for _ in 0..N {
        sem_release(s1, 1).unwrap();
        assert!(matches!(
            wait_any(&[s2], 1, Some(Duration::from_secs(30)), 0),
            WaitOutcome::Signaled { .. }
        ));
    }
    let wall = t.elapsed();
    let r1 = rusage(libc::RUSAGE_SELF);
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let c1 = rusage(libc::RUSAGE_CHILDREN);
    assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);

    let cpu_us = (r1.utime_us - r0.utime_us + r1.stime_us - r0.stime_us
        + c1.utime_us - c0.utime_us + c1.stime_us - c0.stime_us)
        .max(0) as u64;
    let nvcsw = (r1.nvcsw - r0.nvcsw + c1.nvcsw - c0.nvcsw).max(0) as u64;
    let metrics = vec![
        ("ops", N),
        ("wall_us", wall.as_micros() as u64),
        ("cpu_us", cpu_us),
        ("cpu_ns_per_op", cpu_us * 1000 / N),
        ("voluntary_ctxsw", nvcsw),
        ("ctxsw_per_kop", nvcsw * 1000 / N),
    ];
    for (k, v) in &metrics {
        println!("  {k:<24} {v}");
    }
    record(bench, &metrics);
    close(s1);
    close(s2);
}

/// The real Wine topology: MANY processes sharing one region. A busy game
/// session is wineserver + the game + a few helpers, each with worker
/// threads, all hammering the same objects. This forks PROCS children
/// (they inherit the parent's mapping, like exec'd Wine processes sharing
/// $TMPDIR would see the same file), releases them simultaneously on a
/// manual-reset start event, and has every process run a mixed loop of
/// contended-mutex acquire/release, sem ping-pong, and event set/reset.
///
/// This is the bench that exercises the costs invisible in a single
/// process: robust-region-mutex contention across processes, the per-lock
/// sigmask syscalls, and cross-process futex wake storms. Optimizations
/// that remove syscalls from the lock path should show up here first.
#[test]
#[ignore = "bench simulation"]
fn bench_5_multi_process_contention() {
    setup();
    let _lock = BENCH_LOCK.lock().unwrap();
    let bench = "multi_process_contention";
    const PROCS: u32 = 4;
    const ITERS: u64 = 2_000;
    const OPS_PER_ITER: u64 = 6; // mtx wait+unlock, sem wait+release, event set+reset

    let start = create_event(true, false).unwrap(); // manual-reset starter gun
    let mtx = create_mutex(0, 0).unwrap(); // one global "device lock"
    let ping: Vec<u32> = (0..PROCS).map(|_| create_semaphore(0, 1).unwrap()).collect();
    let pong: Vec<u32> = (0..PROCS).map(|_| create_semaphore(0, 1).unwrap()).collect();
    let ev = create_event(false, false).unwrap();

    let c0 = rusage(libc::RUSAGE_CHILDREN);
    let r0 = rusage(libc::RUSAGE_SELF);
    let t = Instant::now();

    let mut pids = Vec::new();
    for p in 0..PROCS {
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // Child: wait for the starter gun, then run the mixed loop.
            let _ = wait_any(&[start], 1, Some(Duration::from_secs(30)), 0);
            let owner = 1000 + p;
            let (mut a, mut b) = (ping[p as usize], pong[p as usize]);
            if p % 2 == 1 {
                std::mem::swap(&mut a, &mut b);
            }
            for _ in 0..ITERS {
                // Contended device lock across all processes.
                if matches!(
                    wait_any(&[mtx], owner, Some(Duration::from_secs(30)), 0),
                    WaitOutcome::Signaled { .. }
                ) {
                    mutex_unlock(mtx, owner).unwrap();
                }
                // RPC-ish exchange with the parent on a private channel.
                sem_release(a, 1).unwrap();
                let _ = wait_any(&[b], owner, Some(Duration::from_secs(30)), 0);
                // Event churn.
                event_set(ev).unwrap();
                event_reset(ev).unwrap();
            }
            std::process::exit(0);
        }
        pids.push(pid);
    }

    // Parent: answer all children's pings until they finish.
    event_set(start).unwrap();
    let ops_total = PROCS as u64 * ITERS * OPS_PER_ITER;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut answered = 0u64;
    while answered < PROCS as u64 * ITERS {
        let mut progressed = false;
        for p in 0..PROCS {
            let (req, resp) = if p % 2 == 1 {
                (pong[p as usize], ping[p as usize])
            } else {
                (ping[p as usize], pong[p as usize])
            };
            if matches!(
                wait_any(&[req], 1, Some(Duration::from_millis(1)), 0),
                WaitOutcome::Signaled { .. }
            ) {
                sem_release(resp, 1).unwrap();
                answered += 1;
                progressed = true;
            }
        }
        if !progressed && Instant::now() > deadline {
            break;
        }
    }

    let mut status = 0;
    for pid in &pids {
        unsafe { libc::waitpid(*pid, &mut status, 0) };
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    }
    let wall = t.elapsed();
    let c1 = rusage(libc::RUSAGE_CHILDREN);
    assert_eq!(answered, PROCS as u64 * ITERS, "children stalled");

    // Parent CPU is included too: it acts as the wineserver.
    let r1 = rusage(libc::RUSAGE_SELF);
    let cpu_us = (c1.utime_us - c0.utime_us + c1.stime_us - c0.stime_us + r1.utime_us - r0.utime_us
        + r1.stime_us
        - r0.stime_us)
        .max(0) as u64;
    let nvcsw = (c1.nvcsw - c0.nvcsw + r1.nvcsw - r0.nvcsw).max(0) as u64;
    let metrics = vec![
        ("ops", ops_total),
        ("wall_us", wall.as_micros() as u64),
        ("cpu_us", cpu_us),
        ("cpu_ns_per_op", cpu_us * 1000 / ops_total),
        ("cpu_ratio_pct", cpu_us * 100 / (wall.as_micros() as u64).max(1)),
        ("voluntary_ctxsw", nvcsw),
        ("ctxsw_per_kop", nvcsw * 1000 / ops_total),
    ];
    for (k, v) in &metrics {
        println!("  {k:<24} {v}");
    }
    record(bench, &metrics);
    close(start);
    close(mtx);
    close(ev);
    for h in ping.iter().chain(pong.iter()) {
        close(*h);
    }
}

// ---------------------------------------------------------------------------
// exec()-based multi-process bench: children are real, independent processes
// ---------------------------------------------------------------------------

/// Worker loop shared by the fork bench and the exec'd worker below.
/// `a`/`b` are the private RPC channel with the parent: release on `a`,
/// wait on `b`.
fn mp_worker_loop(start: u32, mtx: u32, a: u32, b: u32, ev: u32, owner: u32, iters: u64) {
    let _ = wait_any(&[start], 1, Some(Duration::from_secs(30)), 0);
    for _ in 0..iters {
        if matches!(
            wait_any(&[mtx], owner, Some(Duration::from_secs(30)), 0),
            WaitOutcome::Signaled { .. }
        ) {
            mutex_unlock(mtx, owner).unwrap();
        }
        sem_release(a, 1).unwrap();
        let _ = wait_any(&[b], owner, Some(Duration::from_secs(30)), 0);
        event_set(ev).unwrap();
        event_reset(ev).unwrap();
    }
}

/// Env-var protocol between bench_6 and the exec'd worker process.
const W_ENV: &str = "NTSYNC_BENCH_WORKER";
const W_PATH: &str = "NTSYNC_BENCH_WORKER_PATH";
const W_IDX: &str = "NTSYNC_BENCH_WORKER_IDX";
const W_ITERS: &str = "NTSYNC_BENCH_WORKER_ITERS";
/// Comma-separated: start,mtx,ev,ping0,ping1,...,pong0,pong1,...
const W_HANDLES: &str = "NTSYNC_BENCH_WORKER_HANDLES";

/// Same scenario as bench_5, but children are spawned with
/// exec(current_exe()) instead of fork(): each one independently opens,
/// flocks and mmaps the region file and calls ntsync_init, exactly like a
/// fresh Wine process attaching to $TMPDIR/ntsync_userspace.shm. On top of
/// the contention metrics this measures the real per-process attach cost,
/// and each worker reports its own shm RSS (parsed back from its stdout),
/// which is the number lazy-init optimizations must shrink.
#[test]
#[ignore = "bench simulation"]
fn bench_6_multi_process_exec() {
    if std::env::var(W_ENV).is_ok() {
        return; // we are the worker; bench_7_worker_exec does the work
    }
    setup();
    let _lock = BENCH_LOCK.lock().unwrap();
    let bench = "multi_process_exec";
    const PROCS: u32 = 4;
    // Longer than the fork bench: amortizes exec/dynamic-linker startup
    // noise, which otherwise dominates the per-op numbers.
    const ITERS: u64 = 6_000;
    const OPS_PER_ITER: u64 = 6;

    let start = create_event(true, false).unwrap();
    let mtx = create_mutex(0, 0).unwrap();
    let ping: Vec<u32> = (0..PROCS).map(|_| create_semaphore(0, 1).unwrap()).collect();
    let pong: Vec<u32> = (0..PROCS).map(|_| create_semaphore(0, 1).unwrap()).collect();
    let ev = create_event(false, false).unwrap();

    let handles = {
        let mut v = vec![start, mtx, ev];
        v.extend(ping.iter().chain(pong.iter()));
        v.iter().map(|h| h.to_string()).collect::<Vec<_>>().join(",")
    };
    let shm_path = std::env::temp_dir().join(format!("{}.shm", shm_stem()));
    let exe = std::env::current_exe().unwrap();

    let c0 = rusage(libc::RUSAGE_CHILDREN);
    let r0 = rusage(libc::RUSAGE_SELF);
    let t = Instant::now();

    let children: Vec<_> = (0..PROCS)
        .map(|p| {
            std::process::Command::new(&exe)
                .args([
                    "bench::bench_7_worker_exec",
                    "--exact",
                    "--ignored",
                    "--nocapture",
                ])
                .env(W_ENV, "1")
                .env(W_PATH, &shm_path)
                .env(W_IDX, p.to_string())
                .env(W_ITERS, ITERS.to_string())
                .env(W_HANDLES, &handles)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();

    event_set(start).unwrap();
    let ops_total = PROCS as u64 * ITERS * OPS_PER_ITER;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut answered = 0u64;
    while answered < PROCS as u64 * ITERS {
        let mut progressed = false;
        for p in 0..PROCS {
            let (req, resp) = if p % 2 == 1 {
                (pong[p as usize], ping[p as usize])
            } else {
                (ping[p as usize], pong[p as usize])
            };
            if matches!(
                wait_any(&[req], 1, Some(Duration::from_millis(1)), 0),
                WaitOutcome::Signaled { .. }
            ) {
                sem_release(resp, 1).unwrap();
                answered += 1;
                progressed = true;
            }
        }
        if !progressed && Instant::now() > deadline {
            break;
        }
    }

    // Reap and collect each worker's reported shm RSS.
    let mut worker_rss = Vec::new();
    let mut worker_logs = String::new();
    for ch in children {
        let out = ch.wait_with_output().unwrap();
        worker_logs.push_str(&String::from_utf8_lossy(&out.stdout));
        assert!(out.status.success(), "worker failed: {out:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("NTSYNC_WORKER_RSS_KB=") {
                worker_rss.push(v.trim().parse::<u64>().unwrap_or(0));
            }
        }
    }
    assert_eq!(
        answered,
        PROCS as u64 * ITERS,
        "children stalled after {answered} answers; worker output:\n{worker_logs}"
    );
    assert_eq!(worker_rss.len(), PROCS as usize, "missing worker RSS");
    let wall = t.elapsed();
    let c1 = rusage(libc::RUSAGE_CHILDREN);
    let r1 = rusage(libc::RUSAGE_SELF);

    let cpu_us = (c1.utime_us - c0.utime_us + c1.stime_us - c0.stime_us + r1.utime_us - r0.utime_us
        + r1.stime_us
        - r0.stime_us)
        .max(0) as u64;
    let nvcsw = (c1.nvcsw - c0.nvcsw + r1.nvcsw - r0.nvcsw).max(0) as u64;
    let metrics = vec![
        ("ops", ops_total),
        ("wall_us", wall.as_micros() as u64),
        ("cpu_us", cpu_us),
        ("cpu_ns_per_op", cpu_us * 1000 / ops_total),
        ("cpu_ratio_pct", cpu_us * 100 / (wall.as_micros() as u64).max(1)),
        ("voluntary_ctxsw", nvcsw),
        ("ctxsw_per_kop", nvcsw * 1000 / ops_total),
        ("worker_shm_rss_kb_max", worker_rss.iter().copied().max().unwrap_or(0)),
        ("worker_shm_rss_kb_sum", worker_rss.iter().sum()),
    ];
    for (k, v) in &metrics {
        println!("  {k:<24} {v}");
    }
    record(bench, &metrics);
    close(start);
    close(mtx);
    close(ev);
    for h in ping.iter().chain(pong.iter()) {
        close(*h);
    }
}

/// The exec'd worker. Runs only when NTSYNC_BENCH_WORKER=1 is set (by
/// bench_6); in a normal bench run it returns immediately.
#[test]
#[ignore = "bench worker"]
fn bench_7_worker_exec() {
    if std::env::var(W_ENV).is_err() {
        return;
    }
    let path = std::env::var(W_PATH).unwrap();
    let idx: u32 = std::env::var(W_IDX).unwrap().parse().unwrap();
    let iters: u64 = std::env::var(W_ITERS).unwrap().parse().unwrap();
    let h: Vec<u32> = std::env::var(W_HANDLES)
        .unwrap()
        .split(',')
        .map(|s| s.parse().unwrap())
        .collect();
    let (start, mtx, ev) = (h[0], h[1], h[2]);
    let procs = (h.len() - 3) / 2;
    let ping = &h[3..3 + procs];
    let pong = &h[3 + procs..];

    // Independent attach: open + flock + mmap + init, like a fresh process.
    init(Some(&path)).unwrap();

    let (a, b) = if idx % 2 == 1 {
        (pong[idx as usize], ping[idx as usize])
    } else {
        (ping[idx as usize], pong[idx as usize])
    };
    mp_worker_loop(start, mtx, a, b, ev, 1000 + idx, iters);

    // Report our own shm RSS after the run (how much of the region this
    // process actually had to touch).
    let (_, rss, _, _) = shm_stats(&shm_stem_from_path(&path));
    println!("NTSYNC_WORKER_RSS_KB={rss}");
    std::process::exit(0);
}

/// The smaps stem for a worker: the shm filename without the version
/// suffix, so any region file version matches.
fn shm_stem_from_path(path: &str) -> String {
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.split('.').next().unwrap_or("ntsync-bench").to_string()
}
