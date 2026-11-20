# Configuration Reference

Services and supervisors are TOML files (`*.toml`) in two places:

| Directory | Owner |
|-----------|-------|
| `/usr/lib/dynamod/services/`, `/usr/lib/dynamod/supervisors/` | packages (vendor) |
| `/etc/dynamod/services/`, `/etc/dynamod/supervisors/` | the administrator |

Files are matched by **file name**. A file in `/etc` replaces the
`/usr/lib` file of the same name entirely (no merging of fields). A file in
`/etc` that is a symlink to `/dev/null`, or empty, **masks** it: nothing of
that name is loaded. `dynamodctl reload` re-reads both directories.

```sh
cp /usr/lib/dynamod/services/sshd.toml /etc/dynamod/services/   # then edit: replaces it
ln -s /dev/null /etc/dynamod/services/dhcpcd.toml                # masks it
```

Each service file defines one service. Only `name` and `exec` are required —
everything else has sensible defaults, and every field added since the first
release defaults to the old behaviour.

## Enabled and disabled services

A service is started at boot if it is *enabled*: its `[service] enabled`
field (default `true`), overridden by the administrator with marker files
whose existence is what counts (content is ignored):

| Marker | Effect |
|--------|--------|
| `/etc/dynamod/disabled/NAME` | not started at boot (wins if both exist) |
| `/etc/dynamod/enabled/NAME` | started at boot |

`NAME` is the service's `name`. The services started at boot are the
enabled ones plus everything they `requires` or `wants`, transitively, even
if disabled. A disabled service is still loaded, shown by `dynamodctl list`
and `status`, and can be started by hand.

```sh
dynamodctl enable sshd [--now]      # creates enabled/sshd, removes disabled/sshd; --now also starts it
dynamodctl disable sshd [--now]     # the reverse; --now also stops it
dynamodctl is-enabled sshd          # prints enabled/disabled; exit 0 if enabled, 1 if not
```

These work on the files alone (no running service manager needed), also
for an image being built: `dynamodctl --root /mnt enable sshd`.

## Service configuration

### `[service]` — Identity and command

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | **required** | Unique service name |
| `supervisor` | string | `"root"` | Which supervisor manages this service |
| `exec` | string[] | **required** | Command and arguments |
| `workdir` | string | — | Working directory before exec |
| `type` | string | `"simple"` | How the service runs (see below) |
| `environment` | map | `{}` | Environment variables: `{ KEY = "value" }` |
| `environment-file` | string | — | Path to a file with `KEY=VALUE` lines |
| `enabled` | bool | `true` | Start at boot (see above) |
| `remain-after-exit` | bool | `false` | `oneshot` only: stay active after a successful exit (see below) |
| `failure-action` | string | `"none"` | What to do with the machine when the service fails (see below) |

**Service types:**

- `simple` — a long-running foreground process (most services)
- `oneshot` — runs once and exits; considered "ready" when it exits
  successfully, and not restarted after a successful exit. With
  `remain-after-exit = true` it is then "active (exited)" (`exited` in
  `dynamodctl list`) until stopped; stopping it — `dynamodctl stop`, or at
  shutdown in the usual order — runs its `[shutdown] stop-exec`, if any
  (otherwise it is just marked stopped).
- `forking` — the process forks into the background (legacy daemons)
- `notify` — sends `READY=1` to `$NOTIFY_SOCKET` when ready (sd_notify compatible)

### `[service.user]` — Privilege dropping

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `user` | string | — | Run as this user (name or UID) |
| `group` | string | — | Run as this group (name or GID) |
| `supplementary-groups` | string[] | `[]` | Additional groups |

### `[restart]` — What happens when it exits

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `policy` | string | `"permanent"` | When to restart (see below) |
| `delay` | duration | `"1s"` | Wait this long before restarting |
| `max-restarts` | int | `5` | Max restarts before giving up |
| `max-restart-window` | duration | `"60s"` | Time window for counting restarts |

**Restart policies:**

- `permanent` — always restart, no matter how it exited
- `transient` — restart only on abnormal exit (non-zero code or killed by signal)
- `temporary` — never restart (good for oneshot tasks)

If `max-restarts` is exceeded within `max-restart-window`, the supervisor
escalates the failure to its parent.

A service stopped on request (`dynamodctl stop`) is not restarted, whatever
its policy.

### `failure-action` — When a service fails

| Value | Effect |
|-------|--------|
| `none` | nothing beyond the restart policy (default) |
| `reboot` | clean reboot: services are stopped in order, then dynamod-init reboots |
| `poweroff` | clean poweroff, likewise |
| `reboot-force` | `sync` and `reboot(2)` at once, without stopping services or unmounting |

A service *fails* when a oneshot exits non-zero (or is killed) at boot, when
its readiness check times out or fails, when it cannot be spawned, or when
its process exits abnormally and is not restarted (restart policy, or the
supervisor giving up on it after `max-restarts`). A service that is being
restarted has not failed yet.

### `[readiness]` — When is it "ready"?

Dependencies wait for a service to be ready, not just started. This section
controls how readiness is detected.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | `"none"` | Detection method (see below) |
| `port` | int | — | TCP port to poll (for `tcp-port`) |
| `check-exec` | string[] | — | Health check command (for `exec`) |
| `timeout` | duration | `"30s"` | Give up after this long |

**Readiness types:**

- `none` — ready immediately after the process starts
- `notify` — service sends `READY=1` to `$NOTIFY_SOCKET` (sd_notify protocol)
- `tcp-port` — dynamod polls the specified TCP port until it accepts connections
- `exec` — dynamod runs a command periodically; ready when it exits 0
- `fd` — service writes a byte to the file descriptor in `$DYNAMOD_READY_FD`

### `[dependencies]` — Startup ordering

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `requires` | string[] | `[]` | Hard deps — must be ready; blocked if they fail |
| `wants` | string[] | `[]` | Soft deps — ordering only if present, won't block |
| `after` | string[] | `[]` | Start after these services (ordering, no failure propagation) |
| `before` | string[] | `[]` | Start before these services |
| `after-all` | bool | `false` | Start only once every other service started at boot is ready, failed or blocked |

Ordering only applies between services that are started at boot: `after`
a disabled service that nothing pulls in does not wait for it.

`after-all` services wait until the rest of the boot has settled: nothing
else is waiting to start or to become ready. Services that themselves wait
for an `after-all` service do not count, and several `after-all` services
start together (ordered among themselves by their own dependencies). A
service that never becomes ready (a oneshot that never exits, a long
readiness timeout) delays them accordingly.

The difference between `requires` and `after`:

- `requires = ["database"]` — if database fails to start, this service is blocked
- `after = ["database"]` — start after database, but if database fails, start anyway

### `[cgroup]` — Resource limits (cgroups v2)

| Field | Type | Description |
|-------|------|-------------|
| `memory-max` | size | Hard memory limit — OOM kill above this |
| `memory-high` | size | Soft limit — kernel applies reclaim pressure |
| `cpu-weight` | int | Relative CPU share, 1–10000 (default 100) |
| `cpu-max` | string | CPU bandwidth: `"quota period"` (e.g. `"200000 100000"` = 200%) |
| `pids-max` | int | Maximum number of processes |
| `io-weight` | int | Relative I/O priority, 1–10000 (default 100) |

### `[namespace]` — Process isolation

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enable` | string[] | `[]` | Namespaces to activate: `pid`, `mnt`, `net`, `uts`, `ipc`, `user`, `cgroup` |
| `bind-mounts` | object[] | `[]` | Each: `{ source, target, writable }` |
| `private-tmp` | bool | `false` | Give the service its own `/tmp` |
| `protect-system` | string | — | `"strict"` (read-only root) or `"full"` (read-only `/usr`, `/boot`) |

### `[shutdown]` — How to stop it

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `stop-signal` | string | `"SIGTERM"` | Signal to send for graceful stop |
| `stop-timeout` | duration | `"10s"` | Time to wait before sending SIGKILL |
| `stop-exec` | string[] | — | At shutdown, run this command before sending the signal; for an active `remain-after-exit` oneshot, the command that stops it |
| `last` | bool | `false` | At shutdown, stop this service only after every service without `last` |

`stop-exec` gets `stop-timeout` to finish, then it is killed; its output goes
to dynamod-logd (or svmgr's stderr when logd is no longer running).

`last` overrides dependency order: a `last` service stops after everything
else, even the services it depends on (several `last` services keep reverse
dependency order among themselves). File systems are still mounted then:
dynamod-init unmounts only after the service manager hands back.

## Supervisor configuration

Supervisors define the tree structure and restart strategies.

### `[supervisor]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | **required** | Unique supervisor name |
| `parent` | string | `"root"` | Parent supervisor in the tree |
| `strategy` | string | `"one-for-one"` | Restart strategy (see below) |

### `[restart]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max-restarts` | int | `10` | Max restarts before the supervisor itself fails |
| `max-restart-window` | duration | `"300s"` | Time window for counting |

**Restart strategies:**

- `one-for-one` — only the crashed child restarts. Best for independent services.
- `one-for-all` — all children stop (reverse order) then restart (start order).
  Use when children are tightly coupled.
- `rest-for-one` — the crashed child and all children started after it restart.
  Use when later children depend on earlier ones.

## Value formats

### Duration

`"30s"` (seconds), `"5m"` (minutes), `"1h"` (hours), or a plain number
(interpreted as seconds).

### Size

`"512M"` (mebibytes), `"2G"` (gibibytes), `"1024K"` (kibibytes), `"max"`
(unlimited), or a plain number (interpreted as bytes).

## Full example

```toml
# /etc/dynamod/services/postgresql.toml

[service]
name = "postgresql"
supervisor = "data-stores"
exec = ["/usr/bin/postgres", "-D", "/var/lib/postgresql/data"]
workdir = "/var/lib/postgresql"
type = "simple"
environment = { PGDATA = "/var/lib/postgresql/data", LC_ALL = "C.UTF-8" }

[service.user]
user = "postgres"
group = "postgres"

[restart]
policy = "permanent"
delay = "3s"
max-restarts = 5
max-restart-window = "120s"

[readiness]
type = "tcp-port"
port = 5432
timeout = "45s"

[dependencies]
requires = ["network"]
wants = ["syslog"]

[cgroup]
memory-max = "2G"
memory-high = "1536M"
cpu-weight = 200
pids-max = 256

[namespace]
enable = ["pid", "mnt"]
private-tmp = true
protect-system = "strict"
bind-mounts = [
    { source = "/var/lib/postgresql", target = "/var/lib/postgresql", writable = true },
    { source = "/run/postgresql", target = "/run/postgresql", writable = true },
]

[shutdown]
stop-signal = "SIGTERM"
stop-timeout = "30s"
```

## Upgrade-at-shutdown and boot-check example

Three services of the kind a transactional-upgrade system needs:

```toml
# /usr/lib/dynamod/services/dave-finalize.toml — apply a prepared upgrade at shutdown
[service]
name = "dave-finalize"
exec = ["/bin/true"]
type = "oneshot"
remain-after-exit = true

[shutdown]
stop-exec = ["/usr/bin/dave", "finalize"]
stop-timeout = "10m"
last = true
```

```toml
# /usr/lib/dynamod/services/dave-boot-check.toml — reboot if the new deployment is bad
[service]
name = "dave-boot-check"
exec = ["/usr/bin/dave", "boot-check"]
type = "oneshot"
failure-action = "reboot"

[restart]
policy = "temporary"

[dependencies]
after-all = true
```

```toml
# /usr/lib/dynamod/services/boot-watchdog.toml — give up on a boot that never completes
# (the boot check stops it with `dynamodctl stop boot-watchdog` once it has passed)
[service]
name = "boot-watchdog"
exec = ["/bin/sh", "-c", "sleep 600; exit 1"]
failure-action = "reboot-force"

[restart]
policy = "temporary"
```

## Minimal example

The simplest possible service:

```toml
# /etc/dynamod/services/hello.toml
[service]
name = "hello"
exec = ["/usr/bin/echo", "hello world"]
type = "oneshot"

[restart]
policy = "temporary"
```
