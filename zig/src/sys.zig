/// Allocation-free file/fd helpers over std.os.linux.
///
/// Zig 0.17 no longer has std.fs.openFileAbsolute & co. (nor most of std.posix):
/// file I/O moved to the std.Io interface, which needs an Io instance, and the
/// stock one (std.Io.Threaded) is built around an allocator. PID 1 only ever did plain
/// blocking open/read/write/close, so these wrappers issue the syscalls
/// directly instead of threading an Io through every module. They:
///   - reproduce the open flags Zig 0.15's std.fs used (CLOEXEC, NOCTTY, ...),
///   - retry EINTR like std.posix did,
///   - turn every other errno into an error value (std.posix used
///     `unreachable` for some, i.e. a panic in safe builds).
/// Only std.os.linux wrappers are used, which map to *at / dup3 / ... syscalls
/// on architectures without the legacy ones (aarch64).
const std = @import("std");
const linux = std.os.linux;

pub const fd_t = linux.fd_t;

pub const OpenError = error{ OpenFailed, NameTooLong, BadPathName };
pub const ReadError = error{ WouldBlock, ReadFailed };
pub const WriteError = error{ WouldBlock, WriteFailed };

/// Linux caps a single read/write at this many bytes (same clamp as std.posix).
const max_rw_count: usize = 0x7ffff000;

/// Copy `path` into `buf` as a NUL-terminated string (std.posix.toPosixPath
/// equivalent, but an embedded NUL is an error instead of an assertion).
pub fn toPathZ(buf: *[linux.PATH_MAX:0]u8, path: []const u8) OpenError![*:0]const u8 {
    if (path.len >= buf.len) return error.NameTooLong;
    if (std.mem.indexOfScalar(u8, path, 0) != null) return error.BadPathName;
    @memcpy(buf[0..path.len], path);
    buf[path.len] = 0;
    return buf;
}

/// open(2) with EINTR retry; flags/mode passed through unchanged.
pub fn openZ(path: [*:0]const u8, flags: linux.O, mode: linux.mode_t) OpenError!fd_t {
    while (true) {
        const rc = linux.open(path, flags, mode);
        switch (linux.errno(rc)) {
            .SUCCESS => return @intCast(rc),
            .INTR => continue,
            else => return error.OpenFailed,
        }
    }
}

pub const OpenMode = enum { read_only, write_only, read_write };

/// Equivalent of Zig 0.15 `std.fs.openFileAbsolute(path, .{ .mode = mode })`:
/// O_CLOEXEC | O_NOCTTY (| O_LARGEFILE where the arch defines it).
pub fn openFile(path: []const u8, mode: OpenMode) OpenError!fd_t {
    var buf: [linux.PATH_MAX:0]u8 = undefined;
    const path_z = try toPathZ(&buf, path);
    var flags: linux.O = .{
        .ACCMODE = switch (mode) {
            .read_only => .RDONLY,
            .write_only => .WRONLY,
            .read_write => .RDWR,
        },
        .CLOEXEC = true,
        .NOCTTY = true,
    };
    if (@hasField(linux.O, "LARGEFILE")) flags.LARGEFILE = true;
    return openZ(path_z, flags, 0);
}

/// Equivalent of Zig 0.15 `std.fs.createFileAbsolute(path, .{})`:
/// O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC (| O_LARGEFILE), mode 0666.
pub fn createFile(path: []const u8) OpenError!fd_t {
    var buf: [linux.PATH_MAX:0]u8 = undefined;
    const path_z = try toPathZ(&buf, path);
    var flags: linux.O = .{ .ACCMODE = .WRONLY, .CREAT = true, .TRUNC = true, .CLOEXEC = true };
    if (@hasField(linux.O, "LARGEFILE")) flags.LARGEFILE = true;
    return openZ(path_z, flags, 0o666);
}

/// Open a directory for getdents64 iteration
/// (Zig 0.15 `std.fs.openDirAbsolute(path, .{ .iterate = true })`).
pub fn openDirIterable(path: []const u8) OpenError!fd_t {
    var buf: [linux.PATH_MAX:0]u8 = undefined;
    const path_z = try toPathZ(&buf, path);
    return openZ(path_z, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true }, 0);
}

/// True if `path` opens read-only as a file (Zig 0.15 code did
/// `std.fs.openFileAbsolute(path, .{})` purely as an existence probe).
/// The probe fd is closed again.
pub fn fileExists(path: []const u8) bool {
    const fd = openFile(path, .read_only) catch return false;
    close(fd);
    return true;
}

/// True if `path` is a directory. Same check as Zig 0.15
/// `std.fs.openDirAbsolute(path, .{})`, which opened with O_PATH | O_DIRECTORY.
pub fn dirExists(path: []const u8) bool {
    var buf: [linux.PATH_MAX:0]u8 = undefined;
    const path_z = toPathZ(&buf, path) catch return false;
    const fd = openZ(path_z, .{ .ACCMODE = .RDONLY, .DIRECTORY = true, .CLOEXEC = true, .PATH = true }, 0) catch return false;
    close(fd);
    return true;
}

/// close(2); the result is ignored (EINTR still closes on Linux, EBADF is a bug
/// we would rather survive than panic on).
pub fn close(fd: fd_t) void {
    _ = linux.close(fd);
}

/// One read(2), EINTR retried. 0 means EOF.
pub fn read(fd: fd_t, buf: []u8) ReadError!usize {
    while (true) {
        const rc = linux.read(fd, buf.ptr, @min(buf.len, max_rw_count));
        switch (linux.errno(rc)) {
            .SUCCESS => return rc,
            .INTR => continue,
            .AGAIN => return error.WouldBlock,
            else => return error.ReadFailed,
        }
    }
}

/// read(2) until `buf` is full or EOF; returns the byte count.
pub fn readAll(fd: fd_t, buf: []u8) ReadError!usize {
    var index: usize = 0;
    while (index != buf.len) {
        const n = try read(fd, buf[index..]);
        if (n == 0) break;
        index += n;
    }
    return index;
}

/// One pread(2) (pread64 on every arch), EINTR retried. 0 means EOF.
pub fn pread(fd: fd_t, buf: []u8, offset: u64) ReadError!usize {
    const off = std.math.cast(linux.off_t, offset) orelse return error.ReadFailed;
    while (true) {
        const rc = linux.pread(fd, buf.ptr, @min(buf.len, max_rw_count), off);
        switch (linux.errno(rc)) {
            .SUCCESS => return rc,
            .INTR => continue,
            .AGAIN => return error.WouldBlock,
            else => return error.ReadFailed,
        }
    }
}

/// One write(2), EINTR retried; returns bytes written.
pub fn write(fd: fd_t, bytes: []const u8) WriteError!usize {
    while (true) {
        const rc = linux.write(fd, bytes.ptr, @min(bytes.len, max_rw_count));
        switch (linux.errno(rc)) {
            .SUCCESS => return rc,
            .INTR => continue,
            .AGAIN => return error.WouldBlock,
            else => return error.WriteFailed,
        }
    }
}

/// dup2(old, new) with the result ignored, as all callers do.
/// aarch64 has no dup2 syscall; std.os.linux.dup2 falls back to dup3 there, but
/// its `old == new` branch (runtime-safety builds) calls fcntl with swapped
/// arguments in Zig 0.17.1. dup2(fd, fd) is a no-op, so skip it, else dup3(.., 0)
/// (identical to dup2 when old != new) on every arch.
pub fn dup2(old: fd_t, new: fd_t) void {
    if (old == new) return;
    _ = linux.dup3(old, new, 0);
}

/// Wall-clock milliseconds since the epoch (Zig 0.15 `std.time.milliTimestamp`,
/// which was removed; like it, returns 0 if the clock is unavailable).
pub fn milliTimestamp() i64 {
    var ts: linux.timespec = undefined;
    if (linux.errno(linux.clock_gettime(.REALTIME, &ts)) != .SUCCESS) return 0;
    const sec: i64 = ts.sec;
    const nsec: i64 = ts.nsec;
    return (sec *| std.time.ms_per_s) +| @divFloor(nsec, std.time.ns_per_ms);
}

test "toPathZ" {
    var buf: [linux.PATH_MAX:0]u8 = undefined;
    const z = try toPathZ(&buf, "/etc/hostname");
    try std.testing.expectEqualStrings("/etc/hostname", std.mem.span(z));
    try std.testing.expectError(error.BadPathName, toPathZ(&buf, "/a\x00b"));
    const long: [linux.PATH_MAX]u8 = @splat('a');
    try std.testing.expectError(error.NameTooLong, toPathZ(&buf, &long));
}

test "milliTimestamp is after 2020" {
    try std.testing.expect(milliTimestamp() > 1_577_836_800_000);
}

test "file helpers on procfs" {
    // /proc/self/cmdline always exists and is readable on Linux.
    const fd = try openFile("/proc/self/cmdline", .read_only);
    defer close(fd);
    var buf: [16]u8 = undefined;
    const n = try read(fd, &buf);
    try std.testing.expect(n > 0);
    try std.testing.expect(fileExists("/proc/self/cmdline"));
    try std.testing.expect(!fileExists("/nonexistent/dynamod-test"));
    try std.testing.expect(dirExists("/proc"));
    // Not /proc/self/cmdline: qemu-user (foreign-arch test runs) fakes that
    // file with a memfd and ignores O_DIRECTORY.
    try std.testing.expect(!dirExists("/dev/null"));
    try std.testing.expect(!dirExists("/nonexistent/dynamod-test"));
}
