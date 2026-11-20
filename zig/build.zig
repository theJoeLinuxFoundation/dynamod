const std = @import("std");

pub fn build(b: *std.Build) void {
    // Freestanding (no libc) static PID 1 for the build host's architecture by
    // default; cross-compile with e.g. -Dtarget=aarch64-linux-none.
    // `.cpu_model = .baseline` keeps the portable baseline CPU feature set
    // (an unset cpu_arch would otherwise mean "native", i.e. the host's CPU).
    const target = b.standardTargetOptions(.{
        .default_target = .{
            .os_tag = .linux,
            .abi = .none,
            .cpu_model = .baseline,
        },
    });

    // `-Drelease` / `--release` select ReleaseSafe; a plain `zig build` is a
    // Debug build. With a preferred mode set, std.Build exposes no -Doptimize,
    // so it is declared here too to allow picking any mode explicitly.
    const release_optimize = b.standardOptimizeOption(.{
        .preferred_optimize_mode = .safe,
    });
    const optimize = b.option(
        std.builtin.Optimize,
        "optimize",
        "Prioritize performance, safety, or binary size (overrides -Drelease)",
    ) orelse release_optimize;

    const exe = b.addExecutable(.{
        .name = "dynamod-init",
        .root_module = b.createModule(.{
            .root_source_file = b.path("src/main.zig"),
            .target = target,
            .optimize = optimize,
        }),
    });

    b.installArtifact(exe);

    const run_step = b.step("run", "Run dynamod-init");
    const run_cmd = b.addRunArtifact(exe);
    run_step.dependOn(&run_cmd.step);
    run_cmd.step.dependOn(b.getInstallStep());
    run_cmd.addPassthruArgs();

    const exe_tests = b.addTest(.{
        .root_module = exe.root_module,
    });
    const run_exe_tests = b.addRunArtifact(exe_tests);
    const test_step = b.step("test", "Run unit tests");
    test_step.dependOn(&run_exe_tests.step);
}
