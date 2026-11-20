//! Immediate reboot/poweroff/halt, without stopping services or unmounting
//! file systems (`reboot -f`, `failure-action = "reboot-force"`).
//!
//! The clean path is different: svmgr stops services, then asks dynamod-init
//! (RequestShutdown), which kills what is left, unmounts and calls reboot(2).
use crate::protocol::ShutdownKind;

/// Sync (unless `sync` is false), then call reboot(2) with the command for
/// `kind`. Only returns on failure (e.g. EPERM when not root).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn reboot_now(kind: ShutdownKind, sync: bool) -> std::io::Error {
    let cmd = match kind {
        ShutdownKind::Reboot => libc::LINUX_REBOOT_CMD_RESTART,
        ShutdownKind::Poweroff => libc::LINUX_REBOOT_CMD_POWER_OFF,
        ShutdownKind::Halt => libc::LINUX_REBOOT_CMD_HALT,
    };
    if sync {
        unsafe { libc::sync() };
    }
    // The raw system call: bionic and musl/glibc disagree on the wrapper.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_reboot,
            libc::LINUX_REBOOT_MAGIC1,
            libc::LINUX_REBOOT_MAGIC2,
            cmd,
            std::ptr::null::<libc::c_void>(),
        )
    };
    if rc == 0 {
        // RESTART/POWER_OFF/HALT do not return on success; be safe anyway.
        return std::io::Error::other("reboot(2) returned");
    }
    std::io::Error::last_os_error()
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn reboot_now(_kind: ShutdownKind, _sync: bool) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, "reboot(2) is Linux-only")
}
