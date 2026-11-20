//! Well-known filesystem paths for dynamod components.
use std::path::{Path, PathBuf};

/// Runtime directory for sockets and state.
pub const RUN_DIR: &str = "/run/dynamod";

/// Control socket path for dynamodctl <-> svmgr communication.
pub const CONTROL_SOCK: &str = "/run/dynamod/control.sock";

/// Administrator service definitions. A file here replaces the vendor file
/// of the same name; a symlink to /dev/null (or an empty file) masks it.
pub const SERVICES_DIR: &str = "/etc/dynamod/services";

/// Administrator supervisor definitions (same rules as `SERVICES_DIR`).
pub const SUPERVISORS_DIR: &str = "/etc/dynamod/supervisors";

/// Vendor (package) service definitions.
pub const VENDOR_SERVICES_DIR: &str = "/usr/lib/dynamod/services";

/// Vendor (package) supervisor definitions.
pub const VENDOR_SUPERVISORS_DIR: &str = "/usr/lib/dynamod/supervisors";

/// Service definition directories, lowest precedence first.
pub const SERVICE_DIRS: [&str; 2] = [VENDOR_SERVICES_DIR, SERVICES_DIR];

/// Supervisor definition directories, lowest precedence first.
pub const SUPERVISOR_DIRS: [&str; 2] = [VENDOR_SUPERVISORS_DIR, SUPERVISORS_DIR];

/// `enabled/NAME` exists: start service NAME at boot whatever its definition says.
pub const ENABLED_DIR: &str = "/etc/dynamod/enabled";

/// `disabled/NAME` exists: do not start service NAME at boot (wins over `enabled/NAME`).
pub const DISABLED_DIR: &str = "/etc/dynamod/disabled";

/// Environment variable naming an alternative root directory under which the
/// configuration directories, the enable/disable markers and the control
/// socket are looked up. For tests and image building; unset on a running system.
pub const ROOT_ENV: &str = "DYNAMOD_ROOT";

/// Environment variable name for the init socket fd.
pub const INIT_FD_ENV: &str = "DYNAMOD_INIT_FD";

/// Machine ID file path (32 hex characters, no dashes).
pub const MACHINE_ID: &str = "/etc/machine-id";

/// `path` (absolute) placed under `root`, or `path` itself without a root.
pub fn rooted(root: Option<&Path>, path: &str) -> PathBuf {
    match root {
        Some(r) => r.join(path.trim_start_matches('/')),
        None => PathBuf::from(path),
    }
}

/// The alternative root from `$DYNAMOD_ROOT`, if set to something other than "/".
pub fn root_from_env() -> Option<PathBuf> {
    match std::env::var_os(ROOT_ENV) {
        Some(v) if !v.is_empty() && v != "/" => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// Service definition directories under `root`, lowest precedence first.
pub fn service_dirs(root: Option<&Path>) -> Vec<PathBuf> {
    SERVICE_DIRS.iter().map(|d| rooted(root, d)).collect()
}

/// Supervisor definition directories under `root`, lowest precedence first.
pub fn supervisor_dirs(root: Option<&Path>) -> Vec<PathBuf> {
    SUPERVISOR_DIRS.iter().map(|d| rooted(root, d)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooted_paths() {
        assert_eq!(
            rooted(None, SERVICES_DIR),
            PathBuf::from("/etc/dynamod/services")
        );
        assert_eq!(
            rooted(Some(Path::new("/mnt")), SERVICES_DIR),
            PathBuf::from("/mnt/etc/dynamod/services")
        );
        let dirs = service_dirs(None);
        assert_eq!(dirs[0], PathBuf::from(VENDOR_SERVICES_DIR));
        assert_eq!(dirs[1], PathBuf::from(SERVICES_DIR));
    }
}
