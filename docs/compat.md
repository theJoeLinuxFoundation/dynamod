# systemd compatibility: tmpfiles.d and sysusers.d

Packages built for systemd distributions declare the files, directories and
system accounts they need in `tmpfiles.d(5)` and `sysusers.d(5)` files
instead of creating them while installing. dynamod reads the same files
with two programs:

| Program | Also installed as | Does what |
|---------|-------------------|-----------|
| `dynamod-tmpfiles` | `systemd-tmpfiles` | creates, adjusts, removes and cleans files and directories |
| `dynamod-sysusers` | `systemd-sysusers` | creates system users and groups |

The `systemd-*` names are symlinks (`tools/install.sh`, skipped with
`COMPAT_LINKS=0`), so package managers and scripts that call systemd's tools
work unchanged. Both are built from `rust/dynamod-tmpfiles`,
`rust/dynamod-sysusers` and the shared `rust/dynamod-sdconf`.

## When they run

At boot, two `early-boot` oneshots (`config/services/`):

| Service | Runs | Ordering |
|---------|------|----------|
| `sysusers` | `dynamod-sysusers` | `requires` remount-root-rw, `after` fstab-mount, `before` tmpfiles-setup |
| `tmpfiles-setup` | `dynamod-tmpfiles --create --remove --boot` | `requires` remount-root-rw, `after` fstab-mount, sysusers, machine-id |

They are ordered with `after`, so a failure of `sysusers` does not stop
`tmpfiles-setup`. Services that need what tmpfiles.d creates (`/var/lib/NAME`,
`/run/NAME`) should be `after = ["tmpfiles-setup"]`. dynamod has no timers:
run `dynamod-tmpfiles --clean` from cron for systemd-tmpfiles-clean's
periodic cleaning.

A package manager runs them after a transaction that touched the
directories, typically inside the target root:

```sh
systemd-sysusers
systemd-tmpfiles --create --exclude-prefix=/dev --exclude-prefix=/proc --exclude-prefix=/sys --exclude-prefix=/run
```

or from outside with `--root=/path/to/root`. When an image is assembled
without privileges (or in a user namespace where only some ids are mapped),
changing ownership fails: each failure is reported with the path and ids,
everything else is still done, and the exit status is 73. The boot run then
sets the owners.

## Configuration files

Both programs read `*.conf` from, in order of priority:

    /etc/X.d   /run/X.d   /usr/local/lib/X.d   /usr/lib/X.d

A file overrides files of the same name further down the list; a symlink to
`/dev/null` (or an empty file) masks the name. The files are read in lexical
order of their names, whatever their directories. File arguments replace the
search: a name without `/` is looked up in the directories, anything else is
read as given (not prefixed with `--root`), `-` is standard input. With
`--inline` each argument is one configuration line.

`--root=DIR` operates on another root: the directories above, every path in
the configuration, and the account files are taken inside `DIR`, and
symlinks met on the way are followed inside it (an image's `/var/run -> /run`
leads to the image's `/run`). User and group names are always resolved from
the root's own `/etc/passwd` and `/etc/group`, never through the C library
(on bionic that would not read those files at all).

## tmpfiles.d

Line format and semantics are those of tmpfiles.d(5).

| Type | Supported as |
|------|--------------|
| `f`, `f+` | create (and write the argument) / create or truncate |
| `w`, `w+` | write / append to existing files (globs); like systemd, `w` does not truncate |
| `d`, `D` | create directory; `D`'s contents removed by `--remove` |
| `e` | adjust existing directories (globs), clean them by age |
| `v`, `q`, `Q` | a plain directory, as `d`: btrfs subvolumes are never created (an existing one is used as it is); no quota groups |
| `p`, `p+` | fifo |
| `L`, `L+`, `L?` | symlink; `+` replaces what is there; `?` only if the target exists; no argument: `/usr/share/factory/PATH` |
| `c`, `c+`, `b`, `b+` | device nodes; silently skipped without `CAP_MKNOD` or when `mknod` is refused (containers, user namespaces) |
| `C`, `C+` | copy (default source `/usr/share/factory/PATH`); skipped if the source is missing; into a missing or empty destination only, `C+` merges without overwriting. Modes, owners (the line's if given) and times are copied; hard links, xattrs and ACLs are not |
| `x`, `X` | exclude from cleaning (`X`: the directory itself, not its contents) |
| `r`, `R` | remove (globs) with `--remove`; `R` recursively, staying on one file system |
| `z`, `Z` | adjust mode and ownership (globs), `Z` recursively; no SELinux relabeling |
| `t`, `T` | set extended attributes |
| `m`, `F` | obsolete spellings of `z` and `f+`, accepted with a warning |

Modifiers: `!` (only with `--boot`), `-` (a failure to create does not count),
`=` (remove what is in the way, parents included, if its type is wrong),
`+`, `~` (Base64 argument), `^` (argument from the credential
`$CREDENTIALS_DIRECTORY/NAME`; the line is skipped if it is not set), `$`
(removed by `--purge`).

- **Mode**: octal; `~` masks it by the existing bits, `:` applies it only to
  new nodes. Default 0755 for directories, 0644 otherwise; `z`/`Z`/`e` with
  `-` leave the mode alone. The mode is applied after the owner, so set-id
  bits survive `chown`, and it is applied even when the owner could not be.
- **User, group**: name or number, `:` prefix for new nodes only. An unknown
  name makes the line invalid (skipped with `--graceful`).
- **Age**: `10d`, `1h30min`, `1.5h`, `500ms`, ... (systemd's units), `~` to
  keep the first level, an `abcmABCM:` prefix to choose the timestamps.
  Cleaning (`--clean`, for `d D e v q Q C x X`) skips sticky files, device
  nodes, live sockets, files and directories others hold a BSD lock on,
  other file systems and mount points, `lost+found`, and paths that have
  lines of their own; directory timestamps are restored afterwards.
- **Argument**: C escapes and specifiers are expanded for `f w L C`; written
  as it is, without an added newline (as systemd does).
- **Order**: for one path the line of the first file (in name order) wins;
  a parent's lines are applied before its children's, plain paths before
  globs, removal and cleaning before creation.
- Paths below `/var/run/` are rewritten to `/run/` with a warning.

Specifiers (path and argument): `%%`, `%a %A %b %B %C %D %g %G %h %H %l %L
%m %M %o %q %S %t %T %u %U %v %V %w %W`, with system-instance values (`%h`
is `/root`, `%u` `root`, `%t` `/run`, ...). `%m`, `%o` and the other
os-release values come from the root; `%H`, `%v`, `%b` from the running
kernel. A line whose `%m` cannot be resolved yet (no `/etc/machine-id`, as in
an image being built) is skipped with a notice; an unknown specifier makes the
line invalid.

Commands and options: `--create`, `--remove`, `--clean`, `--purge` (needs
file arguments), `--boot`, `--graceful`, `-n`/`--dry-run`, `--prefix=`,
`--exclude-prefix=`, `-E`, `--root=`, `--inline`, `--cat-config`, `--tldr`,
`--no-pager` (ignored), `--help`, `--version`.

Exit status: 0; 65 if a line was invalid (the others are still applied); 73
if something could not be done; 1 for usage errors. A path that exists with
the wrong type is reported but not counted as a failure (unless `=` or `+`
asks for it to be replaced).

**Not supported:** line types `h`/`H` (file attributes), `a`/`A` (ACLs) and
`k`/`K` (capabilities) are rejected as invalid lines; `--user`, `--image`,
`--replace`; the age column on `r`/`R`; the BSD-lock check of `--remove`; the
`tmpfiles.extra` credential; systemd's refusal of "unsafe" ownership
transitions in parent paths (on the running system the kernel's
`fs.protected_symlinks` still applies).

## sysusers.d

| Line | Meaning |
|------|---------|
| `u NAME ID "GECOS" HOME SHELL` | a user, and a group of the same name unless the ID names one; `u!` also locks it |
| `g NAME ID` | a group |
| `m USER GROUP` | membership; a user or group not declared otherwise is created too |
| `r - FROM-TO` | add a range to allocate ids from (default 1-999) |

The ID is a number, `-` (allocate), a path (the owner of that file inside the
root), `uid:gid` or `uid:groupname` (that group must exist or be declared).
GECOS, home (default `/`) and shell (default `/usr/sbin/nologin`, `/bin/sh`
for uid 0) apply to `u` lines; specifiers `%% %a %A %b %B %H %l %m %M %o %q
%T %v %V %w %W` are expanded in every field.

- **Existing accounts are never changed**; an existing user is left alone
  entirely (systemd would still create a missing group of its name). `m`
  lines add members to existing groups, in `/etc/group` and `/etc/gshadow`.
- **Allocation**, as systemd: a given id is used if free, otherwise (with a
  message) one is allocated; allocated ids go downwards from the top of the
  ranges, avoiding every id in use, and a user and its group get the same
  number when possible. `g` lines are processed before `u` lines.
- **Files**: new entries go after the existing ones (before NIS `+`/`-`
  lines): `passwd` `name:x:uid:gid:gecos:home:shell`, `group` `name:x:gid:members`,
  `shadow` `name:!*:DAYS::::::` (`!*`: no password, locked; `DAYS` is today,
  or the day of `$SOURCE_DATE_EPOCH`; `u!` also sets the expiry to 1),
  `gshadow` `name:!*::members`. Existing lines are kept byte for byte.
- **Writing**: under the `/etc/.pwd.lock` lock (as `lckpwdf()`), all new
  files are written to temporary files with the old file's mode and owner
  (new `shadow`/`gshadow`: mode 0000), synced, and only then renamed over the
  old ones; the previous versions are kept as `passwd-` and so on. A second
  run with the same configuration writes nothing.
- **Errors**: if any line is invalid nothing is written (as systemd), since
  a skipped line could otherwise let an `m` line create its user with another
  id. An entry that cannot be created (a `uid:gid` whose group does not
  exist) is reported and the others are still written. Exit status 1 in
  both cases.

Options: `--root=`, `-n`/`--dry-run`, `--cat-config`, `--tldr`, `--inline`,
`--no-pager` (ignored), `--help`, `--version`.

**Not supported:** `--replace`, `--image`; credentials
(`passwd.hashed-password.*`, `passwd.shell.*`, `sysusers.extra`); ranges from
`/etc/login.defs`; any account database but the files; creating home
directories (that is a tmpfiles.d line).
