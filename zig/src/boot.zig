/// Early boot operations: mount pseudo-filesystems, set hostname, seed entropy.
/// These must run before anything else in userspace.
const std = @import("std");
const builtin = @import("builtin");
const linux = std.os.linux;
const constants = @import("constants.zig");
const kmsg = @import("kmsg.zig");
const sys = @import("sys.zig");

const MountEntry = struct {
    source: [*:0]const u8,
    target: [*:0]const u8,
    fstype: [*:0]const u8,
    flags: u32,
    data: ?[*:0]const u8,
    /// A path that must exist for this file system to be mounted, where the
    /// kernel or the machine may not have it; it is skipped without a word
    /// otherwise.
    needs: ?[*:0]const u8 = null,
};

const MS_NOSUID = 0x02;
const MS_NODEV = 0x04;
const MS_NOEXEC = 0x08;
const MS_STRICTATIME = 0x1000000;

const essential_mounts = [_]MountEntry{
    .{ .source = "proc", .target = "/proc", .fstype = "proc", .flags = MS_NOSUID | MS_NODEV | MS_NOEXEC, .data = null },
    .{ .source = "sysfs", .target = "/sys", .fstype = "sysfs", .flags = MS_NOSUID | MS_NODEV | MS_NOEXEC, .data = null },
    .{ .source = "devtmpfs", .target = "/dev", .fstype = "devtmpfs", .flags = MS_NOSUID | MS_STRICTATIME, .data = "mode=0755" },
    .{ .source = "devpts", .target = "/dev/pts", .fstype = "devpts", .flags = MS_NOSUID | MS_NOEXEC, .data = "mode=0620,gid=5" },
    .{ .source = "tmpfs", .target = "/dev/shm", .fstype = "tmpfs", .flags = MS_NOSUID | MS_NODEV, .data = null },
    // Default tmpfs cap is ~50% RAM; live boot copies the full squashfs into /run — raise limit.
    .{ .source = "tmpfs", .target = "/run", .fstype = "tmpfs", .flags = MS_NOSUID | MS_NODEV | MS_STRICTATIME, .data = "mode=0755,size=85%" },
    .{ .source = "cgroup2", .target = "/sys/fs/cgroup", .fstype = "cgroup2", .flags = MS_NOSUID | MS_NODEV | MS_NOEXEC, .data = null },
    // The security modules' files (/sys/kernel/security/lsm and the rest).
    .{ .source = "securityfs", .target = "/sys/kernel/security", .fstype = "securityfs", .flags = MS_NOSUID | MS_NODEV | MS_NOEXEC, .data = null, .needs = "/sys/kernel/security" },
    // The firmware's variables, on a machine booted by UEFI: the boot
    // loader's (systemd-boot's boot counting) and the boot entries.
    .{ .source = "efivarfs", .target = "/sys/firmware/efi/efivars", .fstype = "efivarfs", .flags = MS_NOSUID | MS_NODEV | MS_NOEXEC, .data = null, .needs = "/sys/firmware/efi/efivars" },
};

/// Whether a directory is a mount point: it is on another device than its
/// parent directory.
fn isMountPoint(path: [*:0]const u8) bool {
    const p = std.mem.span(path);
    if (std.mem.eql(u8, p, "/")) return true;
    const slash = std.mem.lastIndexOfScalar(u8, p, '/') orelse return false;
    var parent_buf: [256:0]u8 = undefined;
    const parent: []const u8 = if (slash == 0) "/" else p[0..slash];
    if (parent.len >= parent_buf.len) return false;
    @memcpy(parent_buf[0..parent.len], parent);
    parent_buf[parent.len] = 0;
    const a = deviceOf(path) orelse return false;
    const b = deviceOf(&parent_buf) orelse return false;
    return a[0] != b[0] or a[1] != b[1];
}

fn exists(path: [*:0]const u8) bool {
    var stx: linux.Statx = undefined;
    const rc = linux.statx(linux.AT.FDCWD, path, linux.AT.SYMLINK_NOFOLLOW, linux.STATX.BASIC_STATS, &stx);
    return linux.errno(rc) == .SUCCESS;
}

fn deviceOf(path: [*:0]const u8) ?[2]u32 {
    var stx: linux.Statx = undefined;
    const rc = linux.statx(linux.AT.FDCWD, path, linux.AT.SYMLINK_NOFOLLOW, linux.STATX.BASIC_STATS, &stx);
    if (linux.errno(rc) != .SUCCESS) return null;
    return .{ stx.dev_major, stx.dev_minor };
}

test "the root and /proc are mount points, /proc/self is not" {
    try std.testing.expect(isMountPoint("/"));
    try std.testing.expect(isMountPoint("/proc"));
    try std.testing.expect(!isMountPoint("/proc/self"));
}

/// Mount all essential pseudo-filesystems needed for early boot.
pub fn mountEssentialFilesystems(klog_arg: ?kmsg) void {
    for (&essential_mounts) |*entry| {
        const target = std.mem.span(entry.target);
        if (entry.needs) |path| if (!exists(path)) continue;

        // Create mount point if it doesn't exist (use raw syscall for sentinel ptr)
        _ = linux.mkdirat(@bitCast(@as(i32, linux.AT.FDCWD)), entry.target, 0o755);

        // Moved here from the initramfs (switchroot.moveMounts): mounting
        // again would hide it, for a file system such as tmpfs that can be
        // mounted more than once (/run holds the live image).
        if (isMountPoint(entry.target)) {
            if (klog_arg) |k| k.info("{s} is already mounted", .{target});
            continue;
        }

        const data: usize = if (entry.data) |d| @intFromPtr(d) else 0;
        const rc = linux.mount(entry.source, entry.target, entry.fstype, entry.flags, data);
        const e = linux.errno(rc);
        if (e != .SUCCESS and e != .BUSY) {
            if (klog_arg) |k| {
                k.err("failed to mount {s}: errno {d}", .{ target, @backingInt(e) });
            }
        } else {
            if (klog_arg) |k| {
                k.info("mounted {s}", .{target});
            }
        }
    }
}

/// Create the /run/dynamod runtime directory.
pub fn createRuntimeDir(klog_arg: ?kmsg) void {
    const rc = linux.mkdirat(@bitCast(@as(i32, linux.AT.FDCWD)), constants.run_dir, 0o755);
    const e = linux.errno(rc);
    if (e != .SUCCESS and e != .EXIST) {
        if (klog_arg) |k| {
            k.err("failed to create {s}: errno {d}", .{ constants.run_dir, @backingInt(e) });
        }
    }
}

/// Set the system hostname from /etc/hostname.
pub fn setHostname(klog_arg: ?kmsg) void {
    var buf: [256]u8 = undefined;
    const file = sys.openFile(constants.hostname_path, .read_only) catch {
        if (klog_arg) |k| k.info("no {s} found, skipping hostname", .{constants.hostname_path});
        return;
    };
    defer sys.close(file);

    const len = sys.read(file, &buf) catch {
        if (klog_arg) |k| k.warn("failed to read {s}", .{constants.hostname_path});
        return;
    };

    // Trim trailing whitespace/newlines
    var hostname = buf[0..len];
    while (hostname.len > 0 and (hostname[hostname.len - 1] == '\n' or hostname[hostname.len - 1] == '\r' or hostname[hostname.len - 1] == ' ')) {
        hostname = hostname[0 .. hostname.len - 1];
    }

    if (hostname.len == 0) return;

    // Raw sethostname syscall (std.os.linux has no wrapper; the syscall exists
    // on every Linux arch, e.g. nr 170 on x86_64, 161 on aarch64).
    const rc = linux.syscall2(.sethostname, @intFromPtr(hostname.ptr), hostname.len);
    const e = linux.errno(rc);
    if (e != .SUCCESS) {
        if (klog_arg) |k| k.warn("failed to set hostname: errno {d}", .{@backingInt(e)});
    } else {
        if (klog_arg) |k| k.info("hostname set to '{s}'", .{hostname});
    }
}

/// Generate /etc/machine-id if it does not exist.
/// The machine-id is a 32-character lowercase hex string followed by a newline.
/// Many D-Bus consumers and desktop tools expect this file to exist.
pub fn ensureMachineId(klog_arg: ?kmsg) void {
    // Check if machine-id already exists and has content
    if (sys.openFile(constants.machine_id_path, .read_only)) |file| {
        defer sys.close(file);
        var buf: [33]u8 = undefined;
        const len = sys.read(file, &buf) catch 0;
        if (len >= 32) return; // Already has a valid machine-id
    } else |_| {}

    // Generate 16 random bytes from /dev/urandom
    const urandom = sys.openFile("/dev/urandom", .read_only) catch {
        if (klog_arg) |k| k.warn("cannot open /dev/urandom for machine-id", .{});
        return;
    };
    defer sys.close(urandom);

    var random_bytes: [16]u8 = undefined;
    _ = sys.read(urandom, &random_bytes) catch {
        if (klog_arg) |k| k.warn("cannot read /dev/urandom for machine-id", .{});
        return;
    };

    // Convert to 32 hex characters + newline
    const hex_chars = "0123456789abcdef";
    var hex_buf: [33]u8 = undefined;
    for (random_bytes, 0..) |byte, i| {
        hex_buf[i * 2] = hex_chars[byte >> 4];
        hex_buf[i * 2 + 1] = hex_chars[byte & 0x0f];
    }
    hex_buf[32] = '\n';

    // Write machine-id file
    const file = sys.createFile(constants.machine_id_path) catch {
        if (klog_arg) |k| k.warn("cannot create {s}", .{constants.machine_id_path});
        return;
    };
    defer sys.close(file);

    _ = sys.write(file, &hex_buf) catch {
        if (klog_arg) |k| k.warn("cannot write {s}", .{constants.machine_id_path});
        return;
    };

    if (klog_arg) |k| k.info("generated {s}", .{constants.machine_id_path});
}

/// Detect if we're running in an initramfs (rootfs or tmpfs).
/// Returns true if the root filesystem is RAMFS or TMPFS, indicating
/// we're in an initramfs and may need to switch_root to a real rootfs.
pub fn isInitramfs() bool {
    var st: Statfs = undefined;
    const root: [*:0]const u8 = "/";
    if (linux.errno(statfs(root, &st)) != .SUCCESS) return false;
    return isRamBackedFsType(st.f_type);
}

fn isRamBackedFsType(f_type: c_long) bool {
    const t: i64 = f_type;
    return t == constants.RAMFS_MAGIC or t == constants.TMPFS_MAGIC;
}

/// Kernel `struct statfs` as filled in by statfs(2) on 64-bit architectures that
/// use <asm-generic/statfs.h> (x86_64, aarch64, riscv64, loongarch64): every
/// field is a `long` (`__statfs_word`) except the two-int `f_fsid`, 120 bytes.
/// std.os.linux (Zig 0.17.1) has neither a statfs wrapper nor this struct.
/// Architectures with their own layout (mips, s390x, ...) are rejected below
/// rather than silently misread.
const Statfs = extern struct {
    f_type: c_long,
    f_bsize: c_long,
    f_blocks: c_long,
    f_bfree: c_long,
    f_bavail: c_long,
    f_files: c_long,
    f_ffree: c_long,
    f_fsid: [2]c_int,
    f_namelen: c_long,
    f_frsize: c_long,
    f_flags: c_long,
    f_spare: [4]c_long,
};

comptime {
    switch (builtin.cpu.arch) {
        .x86_64, .aarch64, .riscv64, .loongarch64 => std.debug.assert(@sizeOf(Statfs) == 120),
        else => @compileError("boot.zig: verify struct statfs layout for " ++ @tagName(builtin.cpu.arch)),
    }
}

/// statfs(2). The syscall is named `statfs` on all the architectures above
/// (x86_64: 137, aarch64/riscv64/loongarch64: 43).
fn statfs(path: [*:0]const u8, buf: *Statfs) usize {
    return linux.syscall2(.statfs, @intFromPtr(path), @intFromPtr(buf));
}

/// Seed the kernel PRNG from saved random seed.
pub fn seedEntropy(klog_arg: ?kmsg) void {
    const seed_file = sys.openFile(constants.random_seed_path, .read_only) catch return;
    defer sys.close(seed_file);

    var buf: [512]u8 = undefined;
    const len = sys.read(seed_file, &buf) catch return;
    if (len == 0) return;

    const urandom = sys.openFile("/dev/urandom", .write_only) catch return;
    defer sys.close(urandom);
    _ = sys.write(urandom, buf[0..len]) catch {};

    if (klog_arg) |k| k.info("seeded entropy from {s} ({d} bytes)", .{ constants.random_seed_path, len });
}

// --- Tests ---

test "statfs struct matches the kernel and detects ram-backed roots" {
    try std.testing.expect(isRamBackedFsType(@intCast(constants.RAMFS_MAGIC)));
    try std.testing.expect(isRamBackedFsType(@intCast(constants.TMPFS_MAGIC)));
    try std.testing.expect(!isRamBackedFsType(0xEF53)); // ext4

    // /proc is always procfs (PROC_SUPER_MAGIC); this checks that f_type is
    // read from the right offset with the right width on this arch.
    var st: Statfs = undefined;
    try std.testing.expectEqual(linux.E.SUCCESS, linux.errno(statfs("/proc", &st)));
    try std.testing.expectEqual(@as(c_long, 0x9fa0), st.f_type);
    try std.testing.expect(st.f_bsize > 0);
}
