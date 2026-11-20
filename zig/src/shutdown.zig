/// Shutdown sequence for dynamod-init.
/// Handles graceful shutdown: signal processes, unmount filesystems, reboot/halt.
///
/// Sequence:
/// 1. Send SIGTERM to all processes (except PID 1)
/// 2. Wait shutdown_sigterm_timeout_s, reaping zombies
/// 3. Send SIGKILL to all remaining processes
/// 4. Wait shutdown_sigkill_timeout_s, reaping zombies
/// 5. Save entropy to random seed file
/// 6. Sync all filesystems
/// 7. Unmount all filesystems in reverse order
/// 8. Remount / read-only
/// 9. Final sync
/// 10. Call reboot(2) with the appropriate command
const std = @import("std");
const linux = std.os.linux;
const constants = @import("constants.zig");
const kmsg = @import("kmsg.zig");
const reaper = @import("reaper.zig");
const sys = @import("sys.zig");

pub const ShutdownKind = enum {
    poweroff,
    reboot,
    halt,
};

/// Execute the full shutdown sequence.
/// This function does not return — it ends with a reboot(2) syscall.
pub fn execute(kind: ShutdownKind, klog: ?kmsg) noreturn {
    if (klog) |k| k.emerg("initiating {s}", .{@tagName(kind)});

    // Step 1: Send SIGTERM to all processes (except ourselves — PID 1)
    if (klog) |k| k.info("sending SIGTERM to all processes", .{});
    _ = linux.kill(-1, linux.SIG.TERM);

    // Step 2: Wait for processes to exit, actively reaping
    if (klog) |k| k.info("waiting {d}s for processes to terminate", .{constants.shutdown_sigterm_timeout_s});
    waitAndReap(constants.shutdown_sigterm_timeout_s, klog);

    // Step 3: Send SIGKILL to remaining processes
    if (klog) |k| k.info("sending SIGKILL to remaining processes", .{});
    _ = linux.kill(-1, linux.SIG.KILL);

    // Step 4: Wait briefly and reap
    waitAndReap(constants.shutdown_sigkill_timeout_s, klog);

    // Step 5: Save random seed for next boot
    saveRandomSeed(klog);

    // Step 6: Sync filesystems
    if (klog) |k| k.info("syncing filesystems", .{});
    linux.sync();

    // Step 7: Unmount all pseudo-filesystems (best effort, reverse order)
    unmountAll(klog);

    // Step 8: Remount / read-only
    remountRootReadonly(klog);

    // Step 9: Final sync
    linux.sync();

    // Step 10: Final reboot syscall
    if (klog) |k| k.emerg("{s} now", .{@tagName(kind)});

    const cmd: linux.LINUX_REBOOT.CMD = switch (kind) {
        .poweroff => .POWER_OFF,
        .reboot => .RESTART,
        .halt => .HALT,
    };

    _ = linux.reboot(.MAGIC1, .MAGIC2, cmd, null);

    // Should never reach here
    while (true) {
        _ = linux.pause();
    }
}

/// Wait for a given number of seconds, actively reaping zombies every 100ms.
fn waitAndReap(secs: u32, klog: ?kmsg) void {
    const iterations = secs * 10; // 100ms per iteration
    const sleep_ts = linux.timespec{ .sec = 0, .nsec = 100_000_000 };
    var i: u32 = 0;
    while (i < iterations) : (i += 1) {
        const reaped = reaper.reapAll(klog, null);
        _ = reaped;
        _ = linux.nanosleep(&sleep_ts, null);
    }
}

fn saveRandomSeed(klog: ?kmsg) void {
    const urandom = sys.openFile("/dev/urandom", .read_only) catch return;
    defer sys.close(urandom);

    var buf: [512]u8 = undefined;
    const len = sys.read(urandom, &buf) catch return;
    if (len == 0) return;

    // Ensure parent directory exists
    _ = linux.mkdirat(@bitCast(@as(i32, linux.AT.FDCWD)), "/var/lib/dynamod", 0o755);

    const seed_file = sys.createFile(constants.random_seed_path) catch return;
    defer sys.close(seed_file);
    _ = sys.write(seed_file, buf[0..len]) catch return;

    if (klog) |k| k.info("saved random seed ({d} bytes)", .{len});
}

fn unmountAll(klog: ?kmsg) void {
    // Try to read /proc/mounts to discover all mount points.
    // This handles real root filesystems with additional mounts (/home, /boot, etc.)
    // beyond the 7 hardcoded pseudo-filesystems.
    var mounts_buf: [32768]u8 = undefined;
    const mounts_len = blk: {
        const file = sys.openFile("/proc/mounts", .read_only) catch break :blk @as(usize, 0);
        defer sys.close(file);
        // A single read(2), as before.
        break :blk sys.read(file, &mounts_buf) catch 0;
    };

    if (mounts_len > 0) {
        unmountFromProc(mounts_buf[0..mounts_len], klog);
    } else {
        unmountHardcoded(klog);
    }
}

/// Unmount all filesystems discovered from /proc/mounts, in reverse order.
/// Skips "/" itself (handled separately by remountRootReadonly).
fn unmountFromProc(data: []const u8, klog: ?kmsg) void {
    // Parse mount targets (second field in each line of /proc/mounts).
    // Store them so we can unmount in reverse order.
    var targets: [256][256]u8 = undefined;
    var target_lens: [256]usize = @splat(0);
    var count: usize = 0;

    var lines = std.mem.tokenizeScalar(u8, data, '\n');
    while (lines.next()) |line| {
        if (count >= targets.len) break;
        // Format: device mountpoint fstype options dump pass
        var fields = std.mem.tokenizeScalar(u8, line, ' ');
        _ = fields.next(); // skip device
        const mountpoint = fields.next() orelse continue;

        // Skip root (handled by remountRootReadonly)
        if (mountpoint.len == 1 and mountpoint[0] == '/') continue;

        if (mountpoint.len >= targets[0].len) continue;
        @memcpy(targets[count][0..mountpoint.len], mountpoint);
        targets[count][mountpoint.len] = 0;
        target_lens[count] = mountpoint.len;
        count += 1;
    }

    // Unmount in reverse order (last mounted = first unmounted)
    var i: usize = count;
    while (i > 0) {
        i -= 1;
        const name = targets[i][0..target_lens[i]];
        const target_z: [*:0]const u8 = @ptrCast(targets[i][0..target_lens[i] :0]);
        const rc = linux.umount2(target_z, linux.MNT.DETACH);
        const e = linux.errno(rc);
        if (e != .SUCCESS and e != .INVAL and e != .NOENT) {
            if (klog) |k| k.warn("failed to unmount {s}: errno {d}", .{ name, @backingInt(e) });
        } else if (e == .SUCCESS) {
            if (klog) |k| k.info("unmounted {s}", .{name});
        }
    }
}

/// Fallback: unmount hardcoded pseudo-filesystem paths.
fn unmountHardcoded(klog: ?kmsg) void {
    const targets = [_][*:0]const u8{
        "/sys/fs/cgroup",
        "/dev/shm",
        "/dev/pts",
        "/run",
        "/dev",
        "/sys",
        "/proc",
    };

    for (&targets) |target| {
        const name = std.mem.span(target);
        const rc = linux.umount2(target, linux.MNT.DETACH);
        const e = linux.errno(rc);
        if (e != .SUCCESS and e != .INVAL and e != .NOENT) {
            if (klog) |k| k.warn("failed to unmount {s}: errno {d}", .{ name, @backingInt(e) });
        } else if (e == .SUCCESS) {
            if (klog) |k| k.info("unmounted {s}", .{name});
        }
    }
}

fn remountRootReadonly(klog: ?kmsg) void {
    const MS_REMOUNT = 0x20;
    const MS_RDONLY = 0x01;
    const rc = linux.mount("", "/", "", MS_REMOUNT | MS_RDONLY, 0);
    const e = linux.errno(rc);
    if (e != .SUCCESS) {
        if (klog) |k| k.warn("failed to remount / read-only: errno {d}", .{@backingInt(e)});
    } else {
        if (klog) |k| k.info("remounted / read-only", .{});
    }
}
