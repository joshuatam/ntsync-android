# ntsync-android

Full userspace replacement of the Linux `ntsync` kernel driver
(`drivers/misc/ntsync.c`) for Android — NT semaphores, recursive/abandoned
mutexes, and auto/manual-reset events with `WAIT_ANY` / `WAIT_ALL` waits and
timeouts. Cross-process: objects live in shared memory, waits use futexes.
Intended to back the ntsync fast path of Proton/Wine's ntdll
(`dlls/ntdll/unix/sync.c`) on Android, where no `/dev/ntsync` device exists.

## Architecture (kernel concepts → NDK primitives)

| Kernel ntsync                        | This library                                    |
|--------------------------------------|-------------------------------------------------|
| global object table (`ntsync_device`)| slot table in a file-backed shared mapping      |
| `dev->wait_all_lock`                 | process-shared **robust** pthread mutex in shm (waiter registration/walks only; object state is lock-free CAS) |
| per-object wait queues + `wake_up_process` | per-object intrusive waiter lists; each waiter sleeps on its own node futex word |
| wake-then-recheck (`try_wake_any/all`) | waiters wake, re-evaluate conditions; wake walk is skipped entirely when the list is empty |
| fd lifetime / close-on-death         | creator-pid tracking + `ntsync_sweep_dead()`    |
| `get_task` owner-death check         | explicit `ntsync_mutex_kill` (same as kernel)   |

The region is initialized on first use; the path is the `ntsync_init(path)`
argument if given, else `$NTSYNC_SHM`, else `$TMPDIR/ntsync_userspace.shm`
(the caller is expected to export `TMPDIR`; Termux does this for every
process, so all Wine processes automatically share the region). First opener
creates and initializes the file under `flock`; later processes just `mmap`
it. Capacity is 16384 objects plus an 8192-entry shared waiter-node pool.

`NTSYNC_SHM` exists for containerized setups (e.g. GameNative) where
wineserver and game processes run with different `TMPDIR`s and would
otherwise not see the same region.

Each slot's state (sem count, event flag, mutex owner/recursion/abandoned)
is packed into a single `u64` mutated with lock-free CAS, so signal
operations and already-signaled waits run without taking the region mutex.
The robust mutex only protects waiter registration and wake walks; if a
process dies mid-operation the next locker gets `EOWNERDEAD` and marks the
mutex consistent.

## Environment variables

- `NTSYNC_SHM` — override the shared-region path.
- `NTSYNC_DEBUG=1` — log to stderr, stdout and `$TMPDIR/ntsync_debug.log`
  (the file is the reliable channel for in-game diagnostics); dump
  per-process stats every 10 s (waits, fast-path hits, lock acquisitions,
  wake walks, signal→scheduled wake latency `wake_lat_cnt/us_sum/us_max`);
  emit a "stuck wait" dump with per-object state when a wait stalls.

## Layout

- `src/core.rs` — shm object table + futex wait engine (unit-tested on host,
  including a cross-process fork test and `#[ignore]`d perf simulations)
- `src/ffi.rs` — exported C ABI (`ntsync_*` functions)
- `src/lib.rs` — tests and diagnostics glue
- `include/ntsync_user.h` — C header (same struct layout as `<linux/ntsync.h>`)
- `.cargo/config.toml` — `-z max-page-size=16384` linker flags for all Android
  targets (Google Play 16 KB page-size requirement)

## API

The C API mirrors the kernel ioctls one-to-one: objects are identified by
`uint32_t` handles instead of fds, and every function returns `0` on success
or a negative errno (`-EINVAL`, `-EPERM`, `-EOVERFLOW`, `-ETIMEDOUT`,
`-EOWNERDEAD`, `-EFAULT`), exactly like the kernel.

```c
ntsync_init(NULL);                  /* or ntsync_init("/path/to/region") */

uint32_t sem;
ntsync_create_sem(&sem, &(struct ntsync_sem_args){ .count = 0, .max = 4 });

uint32_t objs[] = { sem };
struct ntsync_wait_args wait = {
    .timeout = UINT64_MAX,          /* absolute ns, CLOCK_MONOTONIC */
    .objs = (uintptr_t)objs,
    .count = 1,
    .owner = gettid(),
};
int ret = ntsync_wait_any(&wait);   /* 0, wait.index set; -EOWNERDEAD if
                                       an abandoned mutex was acquired */
uint32_t prev = 1;
ntsync_sem_release(sem, &prev);     /* prev overwritten with old count */
ntsync_close(sem);
```

Handles are plain integers — share them between processes however you like
(shared memory, environment, your server protocol); no fd passing needed.
Handle 0 is never handed out (slot 0 is reserved): it is the "no alert"
sentinel in `ntsync_wait_args.alert`.

### Region layout versioning

The shm filename carries a `.vN` layout-version suffix
(`ntsync_userspace.v7.shm`) so builds with different layouts never open the
same region. A leftover stale file is unlinked and recreated — never
truncated or zeroed in place, which would SIGBUS/corrupt processes from an
older build that still have it mapped (old mappers keep their ghost inode).

### Semantic details (matching the kernel)

- `sem_release` returns `-EOVERFLOW` and leaves the count unchanged if it
  would exceed `max`.
- Mutexes are recursive per owner tid (tids are system-wide, so ownership
  works across processes); `mutex_unlock` returns the previous recursion
  count in `args->count`; `mutex_kill(handle, owner)` marks the mutex
  abandoned (`-EPERM` if caller is not the owner). Acquiring an abandoned
  mutex succeeds and the wait/read call returns `-EOWNERDEAD`.
- Auto-reset events are consumed by one waiter; manual-reset events stay
  signaled until `ntsync_event_reset`; `ntsync_event_pulse` wakes all
  current waiters and leaves the event unsignaled. `ntsync_event_set` /
  `_reset` / `_pulse` store the previous signaled state in `*prev` (if
  non-NULL), like the kernel ioctls.
- Alertable waits follow the kernel contract: if `alert` is nonzero it names
  an event object that aborts the wait, which then returns success with
  `index == count`.
- `timeout == UINT64_MAX` waits indefinitely; otherwise it is an absolute
  ns deadline on `CLOCK_MONOTONIC` (or `CLOCK_REALTIME` with
  `NTSYNC_WAIT_REALTIME`).
- Max 64 objects per wait (`NTSYNC_MAX_WAIT_COUNT`).

### Divergences from the kernel

- Closing an object that other threads/processes are waiting on fails those
  waits with `-EINVAL` (the kernel keeps objects alive via fd references).
- Objects leaked by a crashing process are not freed automatically (no fd
  close hook in userspace). Run `ntsync_sweep_dead()` from a launcher or
  server process after a child exits.
- Waits are not signal-interruptible with `-ERESTARTSYS` semantics; `EINTR`
  wakes are absorbed and the wait resumes.

## Build

Using the build script (mirrors the proton-wine arm64ec script; defaults to
`$HOME/Android/Sdk/ndk/27.3.13750724`, API 28). Builds arm64-v8a and x86_64;
16KB page-size alignment is always on (`.cargo/config.toml`), there is no
option for it:

```sh
./build-scripts/build-android.sh --build      # build both ABIs + verify alignment
./build-scripts/build-android.sh --install    # copy .so (per-ABI) + header to $OUTPUT_DIR
./build-scripts/build-android.sh --clean
# overrides: NDK=... API=... OUTPUT_DIR=...
```

Manually:

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
NDK=~/Android/Sdk/ndk/<version>/toolchains/llvm/prebuilt/linux-x86_64/bin
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK/aarch64-linux-android24-clang \
cargo build --release --target aarch64-linux-android
# or: cargo install cargo-ndk && cargo ndk -t arm64-v8a -o jniLibs build --release
```

Verify page-size alignment:

```sh
readelf -lW target/aarch64-linux-android/release/libntsync_android.so | grep LOAD
# alignment column must be 0x4000
```

CI: `.github/workflows/build.yml` runs the host unit tests and builds
arm64-v8a / armeabi-v7a / x86_64 release libraries (API 28), verifies the
16KB alignment, and uploads each `.so` + `ntsync_user.h` as artifacts.

## Integrating with Proton

The crate builds as `cdylib`, `rlib` and `staticlib`, so Wine can also
static-link `libntsync_android.a` if a shared library is inconvenient.

Link `libntsync_android.so` into the ntdll unix-side build (or `dlopen` it)
and route the ntsync calls in `dlls/ntdll/unix/sync.c` to the `ntsync_*`
functions instead of `ioctl(fd, NTSYNC_IOC_*, ...)`. Replace the fd table
with the u32 handles; where Wine passes object fds through wineserver
(`SCM_RIGHTS`), pass the integer handle instead — all processes attached to
the same region path see the same objects. After a process exits, call
`ntsync_sweep_dead()` to reclaim its objects.

## Test

```sh
cargo test   # host-side unit tests, including a cross-process fork test

# perf simulations (signaled-wait latency, signal churn, ping-pong wake
# latency same-process and cross-process, contended mutex):
cargo test --release -- --ignored --nocapture perf
```

## License

Copyright (C) 2026 Joshua Tam <297250+joshuatam@users.noreply.github.com>

GNU Lesser General Public License v3.0 — see [LICENSE](LICENSE). LGPL-3.0
incorporates the terms of GPL-3.0 by reference
(<https://www.gnu.org/licenses/gpl-3.0.txt>). This allows linking
`libntsync_android.so` into Wine (LGPL-2.1+) or other programs without
affecting their license, while the library itself stays copyleft.
`include/ntsync_user.h` uses the same struct layout as the kernel's
GPL-2.0-with-syscall-exception uapi header `<linux/ntsync.h>`; note the
different licensing of that upstream header.
