use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A parsed service definition from a TOML file.
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceDef {
    /// The file this definition was loaded from (not part of the TOML).
    #[serde(skip)]
    pub source: Option<PathBuf>,
    pub service: ServiceSection,
    #[serde(default)]
    pub restart: RestartSection,
    #[serde(default)]
    pub readiness: ReadinessSection,
    #[serde(default)]
    pub dependencies: DependencySection,
    #[serde(default)]
    pub cgroup: Option<CgroupSection>,
    #[serde(default)]
    pub namespace: Option<NamespaceSection>,
    #[serde(default)]
    pub shutdown: ShutdownSection,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceSection {
    pub name: String,
    #[serde(default = "default_supervisor")]
    pub supervisor: String,
    pub exec: Vec<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(rename = "type", default = "default_service_type")]
    pub service_type: ServiceType,
    #[serde(default)]
    pub environment: HashMap<String, String>,
    #[serde(rename = "environment-file")]
    #[serde(default)]
    pub environment_file: Option<String>,
    #[serde(default)]
    pub user: Option<UserSection>,
    /// Start at boot. The administrator overrides it with
    /// /etc/dynamod/{enabled,disabled}/NAME.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// For `type = "oneshot"`: after a successful exit the service stays
    /// active ("exited") until stopped, and stopping it runs `stop-exec`.
    #[serde(rename = "remain-after-exit", default)]
    pub remain_after_exit: bool,
    /// What to do with the machine when this service fails.
    #[serde(rename = "failure-action", default)]
    pub failure_action: FailureAction,
}

/// `[service] failure-action`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FailureAction {
    /// Nothing beyond the restart policy (default).
    #[default]
    None,
    /// Clean reboot: stop services, then dynamod-init reboots.
    Reboot,
    /// Clean poweroff.
    Poweroff,
    /// sync and reboot(2) at once, without stopping services.
    RebootForce,
}

impl FailureAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureAction::None => "none",
            FailureAction::Reboot => "reboot",
            FailureAction::Poweroff => "poweroff",
            FailureAction::RebootForce => "reboot-force",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserSection {
    pub user: Option<String>,
    pub group: Option<String>,
    #[serde(rename = "supplementary-groups", default)]
    pub supplementary_groups: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    Simple,
    Oneshot,
    Forking,
    Notify,
}

impl ServiceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceType::Simple => "simple",
            ServiceType::Oneshot => "oneshot",
            ServiceType::Forking => "forking",
            ServiceType::Notify => "notify",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RestartSection {
    #[serde(default = "default_restart_policy")]
    pub policy: RestartPolicy,
    #[serde(default = "default_delay")]
    pub delay: String,
    #[serde(rename = "max-restarts", default = "default_max_restarts")]
    pub max_restarts: u32,
    #[serde(rename = "max-restart-window", default = "default_max_restart_window")]
    pub max_restart_window: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RestartPolicy {
    Permanent,
    Transient,
    Temporary,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadinessSection {
    #[serde(rename = "type", default = "default_readiness_type")]
    pub readiness_type: ReadinessType,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(rename = "check-exec")]
    #[serde(default)]
    pub check_exec: Option<Vec<String>>,
    #[serde(default = "default_readiness_timeout")]
    pub timeout: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReadinessType {
    None,
    Notify,
    TcpPort,
    Exec,
    Fd,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DependencySection {
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub wants: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub before: Vec<String>,
    /// Start only once every other service started at boot is ready,
    /// failed or blocked.
    #[serde(rename = "after-all", default)]
    pub after_all: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CgroupSection {
    #[serde(rename = "memory-max")]
    pub memory_max: Option<String>,
    #[serde(rename = "memory-high")]
    pub memory_high: Option<String>,
    #[serde(rename = "cpu-weight")]
    pub cpu_weight: Option<u32>,
    #[serde(rename = "cpu-max")]
    pub cpu_max: Option<String>,
    #[serde(rename = "pids-max")]
    pub pids_max: Option<u32>,
    #[serde(rename = "io-weight")]
    pub io_weight: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamespaceSection {
    #[serde(default)]
    pub enable: Vec<String>,
    #[serde(rename = "bind-mounts", default)]
    pub bind_mounts: Vec<BindMount>,
    #[serde(rename = "private-tmp", default)]
    pub private_tmp: bool,
    #[serde(rename = "protect-system")]
    pub protect_system: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BindMount {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShutdownSection {
    #[serde(rename = "stop-signal", default = "default_stop_signal")]
    pub stop_signal: String,
    #[serde(rename = "stop-timeout", default = "default_stop_timeout")]
    pub stop_timeout: String,
    #[serde(rename = "stop-exec")]
    #[serde(default)]
    pub stop_exec: Option<Vec<String>>,
    /// At shutdown, stop this service only after every service without
    /// `last` has stopped.
    #[serde(default)]
    pub last: bool,
}

// Defaults
fn default_true() -> bool {
    true
}
fn default_supervisor() -> String {
    "root".into()
}
fn default_service_type() -> ServiceType {
    ServiceType::Simple
}
fn default_restart_policy() -> RestartPolicy {
    RestartPolicy::Permanent
}
fn default_delay() -> String {
    "1s".into()
}
fn default_max_restarts() -> u32 {
    5
}
fn default_max_restart_window() -> String {
    "60s".into()
}
fn default_readiness_type() -> ReadinessType {
    ReadinessType::None
}
fn default_readiness_timeout() -> String {
    "30s".into()
}
fn default_stop_signal() -> String {
    "SIGTERM".into()
}
fn default_stop_timeout() -> String {
    "10s".into()
}

impl Default for RestartSection {
    fn default() -> Self {
        Self {
            policy: default_restart_policy(),
            delay: default_delay(),
            max_restarts: default_max_restarts(),
            max_restart_window: default_max_restart_window(),
        }
    }
}

impl Default for ReadinessSection {
    fn default() -> Self {
        Self {
            readiness_type: default_readiness_type(),
            port: None,
            check_exec: None,
            timeout: default_readiness_timeout(),
        }
    }
}

impl Default for DependencySection {
    fn default() -> Self {
        Self {
            requires: vec![],
            wants: vec![],
            after: vec![],
            before: vec![],
            after_all: false,
        }
    }
}

impl Default for ShutdownSection {
    fn default() -> Self {
        Self {
            stop_signal: default_stop_signal(),
            stop_timeout: default_stop_timeout(),
            stop_exec: None,
            last: false,
        }
    }
}

/// Load a service definition from a TOML file.
pub fn load_service(path: &Path) -> Result<ServiceDef, ServiceLoadError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| ServiceLoadError::Io(path.display().to_string(), e))?;
    let def: ServiceDef = toml::from_str(&content)
        .map_err(|e| ServiceLoadError::Parse(path.display().to_string(), e))?;
    Ok(def)
}

/// Load the service definitions of `dirs` (lowest precedence first): a file
/// in a later directory replaces the one of the same name in an earlier
/// directory; a mask (symlink to /dev/null, or empty file) removes it.
/// Files that fail to parse are skipped with a warning.
pub fn load_services(dirs: &[PathBuf]) -> Vec<ServiceDef> {
    let mut services = Vec::new();
    for file in dynamod_common::config_dirs::merge_dirs(dirs) {
        if file.masked {
            tracing::info!("{} is masked by {}", file.file_name, file.path.display());
            continue;
        }
        for old in &file.replaces {
            tracing::info!("{} replaces {}", file.path.display(), old.display());
        }
        match load_service(&file.path) {
            Ok(mut def) => {
                def.source = Some(file.path.clone());
                services.push(def);
            }
            Err(e) => tracing::warn!("skipping {}: {e}", file.path.display()),
        }
    }
    services
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceLoadError {
    #[error("I/O error reading {0}: {1}")]
    Io(String, std::io::Error),
    #[error("parse error in {0}: {1}")]
    Parse(String, toml::de::Error),
}

/// Parse a duration string like "30s", "5m", "1h" into seconds.
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('s') {
        n.parse().ok()
    } else if let Some(n) = s.strip_suffix('m') {
        n.parse::<u64>().ok().map(|n| n * 60)
    } else if let Some(n) = s.strip_suffix('h') {
        n.parse::<u64>().ok().map(|n| n * 3600)
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration_secs("30s"), Some(30));
        assert_eq!(parse_duration_secs("5m"), Some(300));
        assert_eq!(parse_duration_secs("1h"), Some(3600));
        assert_eq!(parse_duration_secs("60"), Some(60));
    }

    #[test]
    fn test_parse_example_service() {
        let toml_str = r#"
[service]
name = "test-svc"
exec = ["/bin/echo", "hello"]
type = "oneshot"

[restart]
policy = "temporary"

[readiness]
type = "none"
timeout = "10s"

[dependencies]
requires = []

[shutdown]
stop-signal = "SIGTERM"
stop-timeout = "5s"
"#;
        let def: ServiceDef = toml::from_str(toml_str).unwrap();
        assert_eq!(def.service.name, "test-svc");
        assert_eq!(def.service.service_type, ServiceType::Oneshot);
        assert_eq!(def.restart.policy, RestartPolicy::Temporary);
        assert_eq!(def.service.supervisor, "root");
        // New fields default to the old behaviour.
        assert!(def.service.enabled);
        assert!(!def.service.remain_after_exit);
        assert_eq!(def.service.failure_action, FailureAction::None);
        assert!(!def.dependencies.after_all);
        assert!(!def.shutdown.last);
    }

    #[test]
    fn test_parse_new_fields() {
        let toml_str = r#"
[service]
name = "dave-finalize"
exec = ["/bin/true"]
type = "oneshot"
enabled = false
remain-after-exit = true
failure-action = "reboot-force"

[dependencies]
after-all = true

[shutdown]
stop-exec = ["/usr/bin/dave", "finalize"]
last = true
"#;
        let def: ServiceDef = toml::from_str(toml_str).unwrap();
        assert!(!def.service.enabled);
        assert!(def.service.remain_after_exit);
        assert_eq!(def.service.failure_action, FailureAction::RebootForce);
        assert!(def.dependencies.after_all);
        assert!(def.shutdown.last);

        for (text, action) in [
            ("none", FailureAction::None),
            ("reboot", FailureAction::Reboot),
            ("poweroff", FailureAction::Poweroff),
            ("reboot-force", FailureAction::RebootForce),
        ] {
            let t = format!(
                "[service]\nname = \"x\"\nexec = [\"/bin/true\"]\nfailure-action = \"{text}\"\n"
            );
            let def: ServiceDef = toml::from_str(&t).unwrap();
            assert_eq!(def.service.failure_action, action);
            assert_eq!(action.as_str(), text);
        }
        let bad = "[service]\nname = \"x\"\nexec = [\"/bin/true\"]\nfailure-action = \"explode\"\n";
        assert!(toml::from_str::<ServiceDef>(bad).is_err());
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dynamod-svmgr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn test_load_services_precedence_and_masking() {
        let root = temp_root("load");
        let dirs = dynamod_common::paths::service_dirs(Some(&root));
        let svc =
            |name: &str, exec: &str| format!("[service]\nname = \"{name}\"\nexec = [\"{exec}\"]\n");
        write(&dirs[0].join("sshd.toml"), &svc("sshd", "/usr/sbin/sshd"));
        write(&dirs[0].join("udev.toml"), &svc("udev", "/sbin/udevd"));
        write(&dirs[0].join("dhcpcd.toml"), &svc("dhcpcd", "/sbin/dhcpcd"));
        write(&dirs[0].join("broken.toml"), "not toml [");
        // The administrator replaces sshd, masks dhcpcd, adds a service.
        write(
            &dirs[1].join("sshd.toml"),
            &svc("sshd", "/usr/local/sbin/sshd"),
        );
        std::os::unix::fs::symlink("/dev/null", dirs[1].join("dhcpcd.toml")).unwrap();
        write(&dirs[1].join("local.toml"), &svc("local", "/bin/true"));

        let defs = load_services(&dirs);
        let mut names: Vec<&str> = defs.iter().map(|d| d.service.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["local", "sshd", "udev"]);
        let sshd = defs.iter().find(|d| d.service.name == "sshd").unwrap();
        assert_eq!(sshd.service.exec, vec!["/usr/local/sbin/sshd"]);
        assert_eq!(
            sshd.source.as_deref(),
            Some(dirs[1].join("sshd.toml").as_path())
        );
        let udev = defs.iter().find(|d| d.service.name == "udev").unwrap();
        assert_eq!(
            udev.source.as_deref(),
            Some(dirs[0].join("udev.toml").as_path())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
