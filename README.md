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
| `dev->wait_all_lock`                 | process-shared **robust** pthread mutex in shm  |
| per-object wait queues + `wake_up_process` | futex `FUTEX_WAIT`/`FUTEX_WAKE` on a global generation counter in shm |
| wake-then-recheck (`try_wake_any/all`) | waiters wake, take the lock, re-evaluate conditions |
| fd lifetime / close-on-death         | creator-pid tracking + `ntsync_sweep_dead()`    |
| `get_task` owner-death check         | explicit `ntsync_mutex_kill` (same as kernel)   |

The region is initialized on first use; the path is the `ntsync_init(path)`
argument if given, else `$TMPDIR/ntsync_userspace.shm` (the caller is
expected to export `TMPDIR`; Termux does this for every process, so all
Wine processes automatically share the region). First opener creates and
initializes the file under `flock`; later processes just `mmap` it.
Capacity is 16384 objects (~0.5 MB).

The robust mutex protects all state transitions; if a process dies
mid-operation the next locker gets `EOWNERDEAD` and marks the mutex
consistent (all operations are small single-pass mutations, so recovery is
safe).

## Layout

- `src/core.rs` — shm object table + futex wait engine (unit-tested on host,
  including a cross-process fork test)
- `src/ffi.rs` — exported C ABI (`ntsync_*` functions)
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
  current waiters and leaves the event unsignaled.
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
- Alertable waits are not supported (`alert` must be 0); Wine falls back to
  a server-side wait when it needs them.
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
