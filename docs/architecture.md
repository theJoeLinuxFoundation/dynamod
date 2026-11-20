# Architecture

dynamod is a Linux-only PID 1 init system with a microkernel-inspired design
(the Rust parts also build for `aarch64-linux-android`, i.e. against bionic on
an ordinary Linux system).
Each component runs as its own process, communicating over Unix domain sockets
with MessagePack-framed messages. The init itself is intentionally tiny — all
the interesting logic lives in the service manager.

## The big picture

```
kernel
  └── dynamod-init (PID 1, Zig)
        │
        │  Phase A (initramfs):
        │    mount /proc, /sys, /dev
        │    parse cmdline: root= (disk) or dynamod.live (ISO → squashfs → overlay)
        │    mount real root on /newroot, then switch_root → re-exec
        │
        │  Phase B (real root):
        │    mount remaining pseudo-fs
        │    set hostname, seed entropy
        │    spawn dynamod-svmgr via socketpair
        │    enter epoll event loop
        │
        ├── dynamod-svmgr (Rust)
        │     ├── [supervisor: early-boot] (one-for-all)
        │     │     ├── fsck
        │     │     ├── remount-root-rw
        │     │     ├── fstab-mount
        │     │     ├── modules-load
        │     │     ├── mdev-coldplug
        │     │     ├── bootmisc
        │     │     └── network
        │     │
        │     ├── [supervisor: desktop] (one-for-one)
        │     │     ├── dbus
        │     │     ├── dynamod-logind    (login1)
        │     │     ├── dynamod-sd1bridge (systemd1)
        │     │     └── dynamod-hostnamed (hostname1)
        │     │
        │     ├── sshd
        │     ├── nginx
        │     ├── postgresql
        │     └── ...
        │
        └── (console login on tty1)
```

## Components

### dynamod-init (Zig)

The PID 1 process. Intentionally minimal (~2500 lines of Zig across 15 files)
to minimize the chance of a crash that would bring down the system.

**What it does:**

- **Two-phase boot**: detects whether it's in an initramfs (via `statfs("/")`),
  and if `root=` or `dynamod.live` is on the kernel cmdline, performs the
  initramfs-to-rootfs transition (disk mount or ISO/squashfs/overlay pipeline, then
  move pseudo-fs, delete initramfs, `switch_root`, re-exec)
- **Mount pseudo-filesystems**: `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`,
  `/run`, `/sys/fs/cgroup`
- **Early system setup**: set hostname, seed entropy, generate `/etc/machine-id`
- **Spawn and babysit dynamod-svmgr**: restart with exponential backoff on crash
  (500ms initial, 30s max)
- **Zombie reaping**: collects exit status for all orphaned processes
- **Signal handling**: via `signalfd` + `epoll` (SIGCHLD, SIGTERM, SIGINT, SIGUSR1/2)
- **Shutdown**: SIGTERM all → wait 5s → SIGKILL → sync → unmount all → remount
  root read-only → `reboot(2)`

**Design constraints:**

- No heap allocations after initialization
- No panics — every error path is handled
- Fixed-size buffers only (4KB cmdline, 8KB dirent, etc.)
- Statically linked against musl (no libc dependency at runtime)

### dynamod-svmgr (Rust)

The service manager — the "brain" of the system.

**What it does:**

- Parses TOML service and supervisor configurations from `/usr/lib/dynamod/`
  (packages) and `/etc/dynamod/` (administrator; same file name replaces,
  a symlink to `/dev/null` masks)
- Starts the *enabled* services at boot (definition's `enabled`, overridden
  by `/etc/dynamod/{enabled,disabled}/NAME`) and what they require or want
- Builds and validates the dependency DAG (rejects cycles)
- Manages OTP-style supervisor trees (see below)
- Spawns services via `fork`/`exec` with namespace and cgroup isolation
- Runs a dynamic frontier algorithm for maximum-parallelism startup
- Detects readiness via sd_notify, TCP port, exec check, or file descriptor
- Handles graceful shutdown in reverse dependency order (`last` services at the end)
- Performs services' failure actions (reboot, poweroff, reboot-force) and
  shutdowns scheduled with `shutdown +M` / `shutdown HH:MM`
- Listens on `/run/dynamod/control.sock` for `dynamodctl` commands
- Maintains a heartbeat protocol with dynamod-init (5s interval)

### dynamodctl (Rust)

CLI tool for operators. Connects to the control socket.

```
dynamodctl list              # List all services with status and boot state
dynamodctl start <name>      # Start a service
dynamodctl stop <name>       # Stop a service
dynamodctl restart <name>    # Restart a service
dynamodctl status <name>     # Show service details
dynamodctl tree              # Display the supervisor tree
dynamodctl reload            # Re-read /usr/lib/dynamod and /etc/dynamod
dynamodctl enable|disable [--now] <name>...  # Boot markers (no svmgr needed)
dynamodctl is-enabled <name> # Exit 0 if enabled, 1 if not
dynamodctl shutdown <mode>   # poweroff, reboot, or halt, now
dynamodctl reboot|poweroff|halt [-f]         # As the commands below
```

`--root DIR` (or `$DYNAMOD_ROOT`) prefixes the configuration paths, the
markers and the control socket, for image building and tests.

dynamodctl is also a multi-call binary, dispatching on its program name
(`tools/install.sh` creates the links):

| Name | Behaviour |
|------|-----------|
| `reboot`, `poweroff`, `halt` | Clean shutdown through svmgr. `-f`/`--force` (root only): `sync` and `reboot(2)` at once, nothing stopped (`-n`: no sync). `-p`, `--reboot`, `--halt` change the kind; `-w` does nothing (no wtmp); `--no-wall` |
| `shutdown [-r\|-h\|-P\|-H] [TIME] [MSG]` | TIME is `now`, `+M` (or `M`), `HH:MM` (next local time), default `+1`. `now` shuts down at once; later times are kept by svmgr (`ScheduleShutdown`) and run the same clean path when due. `-c` cancels, `-k` only warns. Wall messages go to every terminal that accepts them (`mesg y`) |
| `systemctl` | A small front end, not systemd (`--version` says so): `start`, `stop`, `restart`, `reload-or-restart` (= restart), `try-restart`, `status` (exit 3 inactive, 4 unknown), `show [-p PROP] [--value]`, `is-active` (0/3), `is-failed`, `is-enabled`, `enable`/`disable [--now]`, `list-units [--all] [--state=]`, `list-unit-files`, `daemon-reload`, `reboot`/`poweroff`/`halt [-f]`. `NAME.service` is `NAME`; other unit types and unknown verbs are errors (exit 1); starting an unknown service exits 5. `-q`, `--no-pager`, `--no-block` and similar are accepted; start/stop never wait for the job anyway |

### dynamod-logd (Rust)

Log collection daemon. Accepts log streams from services, stores them in
rotating log files under `/var/log/dynamod/`, and maintains an in-memory
ring buffer for fast queries.

### systemd-mimic layer

Four components that implement systemd's frontend D-Bus APIs so that desktop
environments, Wayland compositors, and common Linux tools work without
modification. All are clean-room implementations based on freedesktop.org
specs — no systemd source code, MIT licensed.

| Component | Bus Name | Key Features |
|-----------|----------|-------------|
| **dynamod-logind** | `org.freedesktop.login1` | Session/seat/user management, `TakeControl`/`TakeDevice` for Wayland GPU access, power management, inhibitor locks, VT switching |
| **dynamod-sd1bridge** | `org.freedesktop.systemd1` | Translates `StartUnit`/`StopUnit`/`ListUnits` to dynamod's native IPC; makes `systemctl` work |
| **dynamod-hostnamed** | `hostname1`, `timedate1`, `locale1` | System identification for GNOME Settings panels |
| **dynamod-sdnotify** | — | Drop-in `libsystemd.so.0` providing `sd_notify()`, `sd_listen_fds()`, `sd_booted()`, `sd_journal_print()`, `sd_pid_get_session()` |

Verified working with **sway** (Wayland compositor via seatd) and
**GNOME Shell 47** (Mutter acquiring GPU via logind's TakeDevice).

## Boot sequence

### Phase A: Initramfs (when `root=` or `dynamod.live` is on the cmdline)

**Disk path** (`root=` set, `dynamod.live` off):

1. Kernel unpacks initramfs, executes `dynamod-init` as PID 1
2. Mount `/proc`, `/sys`, `/dev` (3 pseudo-filesystems)
3. Read `/proc/cmdline`, extract `root=`, `rootfstype=`, `rootflags=`, `rootwait`, `rootdelay`
4. Run `mdev -s` to create device nodes (needed for UUID/LABEL resolution)
5. Resolve root device:
   - `/dev/sda1` → use directly
   - `UUID=xxxx` → scan `/dev/disk/by-uuid/`
   - `PARTUUID=xxxx` → scan `/dev/disk/by-partuuid/`
   - `LABEL=xxxx` → scan `/dev/disk/by-label/`
6. Wait for device if `rootwait` is set (poll every 250ms, max 30s)
7. Mount root device on `/newroot` (read-only by default; default fstype `ext4` if omitted)
8. Move `/proc`, `/sys`, `/dev` to `/newroot/proc`, `/newroot/sys`, `/newroot/dev`
9. Delete initramfs contents (recursive, skip mount points) to free RAM
10. `chdir("/newroot")` → `mount(".", "/", MS_MOVE)` → `chroot(".")` → `chdir("/")`
11. `execve("/sbin/dynamod-init")` — re-exec from the real rootfs

**Live / ISO path** (`dynamod.live=1` or bare `dynamod.live`):

| Parameter | Meaning |
|-----------|---------|
| `dynamod.media=` | Block device with the ISO (same forms as `root=`: `/dev/sr0`, `LABEL=…`, `UUID=…`). Point it at the **whole device**, not a partition — the ISO 9660 parser reads absolute offsets |
| `dynamod.squashfs=` | Path to the squashfs file inside the ISO (default `/live/root.squashfs`) |
| `dynamod.overlay=` | If `0`/`false`/`no`, bind-mount squashfs on `/newroot` (read-only). Otherwise (default) use overlayfs with upper/work on tmpfs under `/run/dynamod/live/` |
| `dynamod.live.iso_parse=` | If `0`/`false`/`no`, skip the in-init ISO 9660 parser and use the legacy iso9660/udf mount path |

Steps: resolve `dynamod.media` (honours `rootwait` / `rootdelay`); best-effort `modprobe` for `loop`, `squashfs`, `iso9660`, `udf`, `overlay` when `/bin/modprobe` exists in the initramfs; **parse ISO 9660 directly from the raw device** (`zig/src/iso9660.zig` — Joliet first, then Rock Ridge `NM`, then plain identifiers) to find the squashfs extents and `pread` them into tmpfs — the ISO is never mounted and no VFS file I/O touches it (iso9660 `open`/`stat` can block indefinitely on QEMU ATAPI CD-ROM, and loop devices backed by files on iso9660 hang on some kernels, e.g. Debian #1106070); attach a **loop** device to the tmpfs copy and mount **squashfs**; optionally stack **overlayfs** on `/newroot`; then the same move/delete/re-exec sequence as the disk path. If parsing fails (pure-UDF media, unexpected structures) or `dynamod.live.iso_parse=0` is set, init falls back to mounting the device as **iso9660** or **udf** and streaming the squashfs into tmpfs from the mounted ISO.

A **single** `root=` mount with `rootfstype=squashfs` is still supported when the squashfs lives on a **block** device (no file-on-ISO indirection).

Packaging helper: `tools/mkiso.sh` builds `dynamod-live.iso` with volume id **DYNAISO** and `live/root.squashfs` from an existing `tools/mkimage.sh` disk image; use `dynamod.media=LABEL=DYNAISO` in the kernel cmdline. Minimal initramfs BusyBox **mdev** does not create `/dev/disk/by-label/`; init falls back to `blkid -L`, then to scanning flagless `blkid` output (BusyBox `blkid` has no `-L`). If your BusyBox build omits `blkid`, use `dynamod.media=/dev/sr0` or add util-linux `blkid` to the initramfs.

**Early-boot services on live systems:** With overlay (default), `/` is writable and `remount-root-rw` behaves like a normal disk root. With `dynamod.overlay=0`, root stays read-only — adjust or drop `remount-root-rw` / machine-id generation for that profile if needed.

**Custom / modular kernels:** The test initramfs from `tools/mkimage.sh` does not include `/lib/modules` unless you add it. If ATA/SCSI CD (`sr_mod`), `iso9660`, `squashfs`, `overlay`, or `loop` are **modules** (`=m`), `modprobe` in the initramfs will fail until you either: (1) build those options **built-in** (`=y`) in your kernel, or (2) rebuild the initramfs with **`INITRAMFS_MODULES_DIR=/lib/modules/<same-as-boot-kernel>`** (full `modules_install` tree for the kernel you pass to QEMU). dynamod skips `modprobe` when `/lib/modules` is missing and logs a short warning instead of spamming errors.

### Phase B: Real root

1. Mount remaining pseudo-filesystems: `/dev/pts`, `/dev/shm`, `/run`, `/sys/fs/cgroup`
   (already-mounted paths from Phase A return EBUSY, which is silently ignored)
2. Create `/run/dynamod/` runtime directory
3. Set hostname from `/etc/hostname`
4. Seed kernel entropy from `/var/lib/dynamod/random-seed`
5. Generate `/etc/machine-id` if missing (32 hex chars from `/dev/urandom`)
6. Set up signal handling via `signalfd`
7. Create socketpair, `fork`/`exec` dynamod-svmgr, pass one end via `DYNAMOD_INIT_FD`
8. Enter `epoll` event loop: signals, svmgr pidfd, IPC socket

### Phase C: Service startup (in dynamod-svmgr)

1. Load all `*.toml` from `/usr/lib/dynamod/{services,supervisors}/`, then
   `/etc/dynamod/{services,supervisors}/` (by file name: `/etc` replaces,
   a `/dev/null` symlink or empty file masks)
2. Validate configs (no cycles, cross-references check)
3. Initialize cgroup v2 hierarchy at `/sys/fs/cgroup/dynamod/`
4. Bind control socket at `/run/dynamod/control.sock`
5. Compute the boot set: enabled services plus, transitively, what they
   `requires` or `wants`; build its dependency graph and initial frontier
6. Start all services with zero unmet dependencies in parallel
7. As services become ready, decrement unmet counts for dependents
8. If a service fails, block all transitive `requires` dependents, and
   perform its `failure-action`, if any
9. Once the frontier has settled, start the `after-all` services
10. Log startup summary: "N ready, M blocked, K not started (disabled)"

Typical boot on a disk-based system:

```
fsck → remount-root-rw → fstab-mount → (bootmisc, hostname, sysctl, network, modules-load)
  → syslog → dynamod-logd → mdev-coldplug → (getty, dbus → logind → sd1bridge → hostnamed)
```

## Shutdown sequence

`reboot`, `poweroff`, `halt`, `shutdown`, `systemctl reboot`, logind and
failure actions all send `Shutdown` over the control socket (a due scheduled
shutdown does the same internally):

1. Svmgr computes reverse topological order (dependents stop first), then
   moves the `last` services to the end
2. For each service: run `stop-exec` if configured (bounded by
   `stop-timeout`), then send `stop-signal` (default SIGTERM), wait
   `stop-timeout` (default 10s), SIGKILL if still alive. An active
   `remain-after-exit` oneshot only runs its `stop-exec`
3. Clean up cgroup for each stopped service
4. Svmgr sends `RequestShutdown` to init and waits to be killed by it (so
   that init cannot see svmgr exit before reading the request)
5. Init sends SIGTERM to all remaining processes, waits 5s reaping zombies
6. Init sends SIGKILL to remaining processes, waits 2s
7. Save random seed to `/var/lib/dynamod/random-seed`
8. `sync()` all filesystems
9. Read `/proc/mounts`, unmount everything in reverse order (falls back to
   hardcoded list if `/proc/mounts` is unreadable)
10. Remount `/` read-only
11. Final `sync()`, then `reboot(2)` syscall (poweroff/reboot/halt)

`reboot -f` and `failure-action = "reboot-force"` skip all of this: `sync()`
and `reboot(2)` directly from dynamodctl or svmgr.

A SIGTERM/SIGINT/SIGUSR1 sent to dynamod-init takes a shorter path: init
forwards `ShutdownSignal` to svmgr but starts its own sequence (SIGTERM to
all, 5s) at once, so stop commands such as a `last` service's `stop-exec`
may not run. Use the commands above for a shutdown that stops services.

## IPC protocol

All inter-component communication uses length-prefixed MessagePack over
Unix domain sockets (SOCK_STREAM).

```
┌──────────┬──────────────┬─────────────────────┐
│ magic    │ length       │ payload              │
│ 0x44 0x4D│ u32 LE (4B) │ MessagePack (N bytes)│
│ "DM"     │              │                      │
└──────────┴──────────────┴─────────────────────┘
```

- **Magic**: `0x44 0x4D` ("DM") for quick validation and resync
- **Max payload**: 64 KiB
- **Serialization**: `rmp-serde` with `struct_as_map()` on the Rust side
  (string keys), hand-written minimal decoder on the Zig side (~300 lines)

### Channels

| Channel | Transport | Messages |
|---------|-----------|----------|
| init ↔ svmgr | socketpair (pre-fork) | Heartbeat, HeartbeatAck, RequestShutdown, ShutdownSignal, LogToKmsg |
| svmgr ↔ dynamodctl | `/run/dynamod/control.sock` | StartService, StopService, RestartService, ServiceStatus, DescribeService, ListServices, TreeStatus, Reload, Shutdown, ScheduleShutdown, CancelShutdown, GetServiceByPid |

## Supervisor trees

Inspired by Erlang/OTP. Services are organized into a tree of supervisors,
each with its own restart strategy and intensity limits.

### Restart strategies

- **one-for-one**: Only the failed child is restarted. Good for independent services.
- **one-for-all**: All children stop (reverse order) then restart (start order).
  Use when children are tightly coupled.
- **rest-for-one**: The failed child and all children started after it are restarted.
  Use when later children depend on earlier ones.

### Restart intensity

Each supervisor tracks restart timestamps in a sliding window. If
`max_restarts` is exceeded within `max_restart_window`, the supervisor
itself fails and the failure escalates to its parent.

For example, with `max-restarts = 5` and `max-restart-window = "60s"`:
if a child crashes 6 times in one minute, the supervisor gives up and
escalates to its parent.

### Per-service restart policies

- **permanent**: Always restart, no matter how it exited
- **transient**: Restart only on abnormal exit (non-zero code or signal)
- **temporary**: Never restart (for oneshot tasks)

Whatever the policy, a service stopped on request is not restarted, and a
oneshot that exited 0 is not run again (a `remain-after-exit` oneshot stays
"exited", i.e. active, until stopped).

### Failure actions

A service fails for good when it cannot be spawned, a oneshot exits non-zero
at boot, its readiness check times out or fails, or it exits abnormally and
is not restarted (policy, or intensity exceeded). Then its `failure-action`
applies: `reboot`/`poweroff` start the clean shutdown above, `reboot-force`
calls `reboot(2)` immediately. The request is queued and performed by the
main loop within ~100ms; a pending `reboot-force` overrides a clean one.

## Dependency resolution

Services declare ordering and hard/soft dependencies:

- `requires = ["network"]` — hard dependency: if network fails, this service is blocked
- `wants = ["syslog"]` — soft dependency: ordering hint, but won't block if absent
- `after = ["bootmisc"]` — pure ordering: start after bootmisc, no failure propagation
- `before = ["nginx"]` — reverse ordering: ensure this starts before nginx

### Dynamic frontier algorithm

Instead of computing a single topological order upfront, dynamod maintains
a live frontier of services whose dependencies are all satisfied:

1. Compute `unmet[s]` = count of unready dependencies for each service
2. Start all services with `unmet == 0` in parallel
3. When a service becomes READY: decrement `unmet` for all its dependents;
   if any reach zero, add them to the frontier
4. When a service FAILS: block all transitive `requires` dependents;
   `wants`/`after`-only dependents of the failed or blocked services can
   still start
5. `after-all` services whose own dependencies are met are held until the
   frontier has settled (nothing else waiting to start or to become ready,
   apart from services that wait for an `after-all` service)

This maximizes parallelism. In a diamond dependency (A → B, A → C, B+C → D),
B and C start simultaneously after A is ready.

Cycles are detected via DFS with three-color marking before startup.
If found, dynamod logs the exact cycle path and refuses to start.

## Resource isolation

### Cgroups v2

Each service gets its own cgroup at `/sys/fs/cgroup/dynamod/<service>/`.
Configurable limits:

- `memory.max` / `memory.high` — hard OOM limit and soft reclaim pressure
- `cpu.weight` / `cpu.max` — scheduling weight and bandwidth
- `pids.max` — process count limit
- `io.weight` — I/O scheduling weight

OOM kills are detected by polling `memory.events`.

### Linux namespaces

Per-service isolation via `unshare(2)`:

- **pid**: isolated PID namespace (service sees itself as PID 1)
- **mnt**: private mount namespace with optional read-only root, private `/tmp`, bind mounts
- **net**: network namespace isolation
- **uts**: separate hostname
- **ipc**: isolated System V IPC
- **user**: user namespace mapping
- **cgroup**: isolated cgroup view
