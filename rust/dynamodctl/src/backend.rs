/// What the command front ends (systemctl, reboot/shutdown, enable/disable)
/// need from the system, behind a trait so their argument handling and exit
/// codes can be tested without a running svmgr.
use std::io::Write;
use std::path::{Path, PathBuf};

use dynamod_common::config_dirs::{self, Enablement};
use dynamod_common::paths;
use dynamod_common::protocol::{MessageBody, ShutdownKind};

use crate::unitfiles::{self, UnitFile};

pub trait Backend {
    /// One request to svmgr over the control socket.
    fn request(&mut self, body: MessageBody) -> Result<MessageBody, String>;
    /// The service definition files (read directly, no svmgr needed).
    fn unit_files(&mut self) -> Vec<UnitFile>;
    /// Enablement of `name` whose definition says `default`.
    fn enablement(&mut self, name: &str, default: bool) -> Enablement;
    /// Write the enabled/disabled marker; returns whether anything changed.
    fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<bool, String>;
    /// The marker path for messages.
    fn marker_path(&self, enabled: bool, name: &str) -> String;
    /// sync (optionally) and reboot(2) now. Returns the error on failure.
    fn reboot_now(&mut self, kind: ShutdownKind, sync: bool) -> String;
    /// Broadcast to logged-in terminals.
    fn wall(&mut self, message: &str);
    fn is_root(&self) -> bool;
    /// Seconds since the epoch.
    fn now(&self) -> u64;
    /// The next local time HH:MM strictly after `now`.
    fn next_local_time(&self, now: u64, hour: u32, minute: u32) -> u64;
    /// Local time for messages, e.g. "2026-11-20 14:30".
    fn format_local_time(&self, t: u64) -> String;
    /// Use another root directory (systemctl --root).
    fn set_root(&mut self, root: PathBuf);
}

/// Output streams plus the backend, passed to every command.
pub struct Ctx<'a> {
    pub backend: &'a mut dyn Backend,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

/// writeln! to ctx.out, ignoring errors (e.g. a closed pipe).
#[macro_export]
macro_rules! say {
    ($ctx:expr, $($arg:tt)*) => {{ let _ = writeln!($ctx.out, $($arg)*); }};
}

/// writeln! to ctx.err, ignoring errors.
#[macro_export]
macro_rules! complain {
    ($ctx:expr, $($arg:tt)*) => {{ let _ = writeln!($ctx.err, $($arg)*); }};
}

/// The real system.
pub struct RealBackend {
    socket: Option<String>,
    root: Option<PathBuf>,
}

impl RealBackend {
    /// `socket` overrides the control socket; `root` (or $DYNAMOD_ROOT)
    /// prefixes the configuration paths and the default socket.
    pub fn new(socket: Option<String>, root: Option<PathBuf>) -> Self {
        Self {
            socket,
            root: root.or_else(paths::root_from_env),
        }
    }

    fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub fn socket(&self) -> String {
        self.socket.clone().unwrap_or_else(|| {
            paths::rooted(self.root(), paths::CONTROL_SOCK)
                .display()
                .to_string()
        })
    }
}

impl Backend for RealBackend {
    fn request(&mut self, body: MessageBody) -> Result<MessageBody, String> {
        crate::client::send_request(&self.socket(), body).map_err(|e| e.to_string())
    }

    fn unit_files(&mut self) -> Vec<UnitFile> {
        unitfiles::scan(self.root())
    }

    fn enablement(&mut self, name: &str, default: bool) -> Enablement {
        config_dirs::enablement(self.root(), name, default)
    }

    fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<bool, String> {
        config_dirs::set_enabled(self.root(), name, enabled).map_err(|e| e.to_string())
    }

    fn marker_path(&self, enabled: bool, name: &str) -> String {
        let dir = if enabled {
            paths::ENABLED_DIR
        } else {
            paths::DISABLED_DIR
        };
        paths::rooted(self.root(), dir)
            .join(name)
            .display()
            .to_string()
    }

    fn reboot_now(&mut self, kind: ShutdownKind, sync: bool) -> String {
        dynamod_common::power::reboot_now(kind, sync).to_string()
    }

    fn wall(&mut self, message: &str) {
        crate::wall::broadcast(message);
    }

    fn is_root(&self) -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    // libc::time_t is deprecated on musl (it will become 64-bit); it is the
    // type localtime_r/mktime take on every libc.
    #[allow(deprecated)]
    fn next_local_time(&self, now: u64, hour: u32, minute: u32) -> u64 {
        // SAFETY: localtime_r/mktime on a zeroed, locally owned struct tm.
        unsafe {
            let now_t = now as libc::time_t;
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&now_t, &mut tm).is_null() {
                return now;
            }
            let today = tm;
            tm.tm_hour = hour as libc::c_int;
            tm.tm_min = minute as libc::c_int;
            tm.tm_sec = 0;
            tm.tm_isdst = -1;
            let mut t = libc::mktime(&mut tm);
            if t <= now_t {
                tm = today;
                tm.tm_mday += 1;
                tm.tm_hour = hour as libc::c_int;
                tm.tm_min = minute as libc::c_int;
                tm.tm_sec = 0;
                tm.tm_isdst = -1;
                t = libc::mktime(&mut tm);
            }
            if t < 0 { now } else { t as u64 }
        }
    }

    #[allow(deprecated)]
    fn format_local_time(&self, t: u64) -> String {
        // SAFETY: as above.
        unsafe {
            let t = t as libc::time_t;
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&t, &mut tm).is_null() {
                return format!("@{t}");
            }
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min
            )
        }
    }

    fn set_root(&mut self, root: PathBuf) {
        self.root = Some(root);
    }
}

/// A recording fake for tests.
#[cfg(test)]
pub mod fake {
    use super::*;
    use dynamod_common::config_dirs::EnablementSource;
    use dynamod_common::protocol::{ServiceDetails, ServiceEntry};
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    pub struct FakeBackend {
        /// name -> (status, enabled-by-definition)
        pub services: BTreeMap<String, (String, bool)>,
        pub enabled_markers: BTreeSet<String>,
        pub disabled_markers: BTreeSet<String>,
        pub masked: BTreeSet<String>,
        pub requests: Vec<String>,
        pub walls: Vec<String>,
        pub reboots: Vec<(ShutdownKind, bool)>,
        pub root: bool,
        pub svmgr_down: bool,
        pub now: u64,
    }

    impl FakeBackend {
        pub fn with(services: &[(&str, &str, bool)]) -> Self {
            let mut b = FakeBackend {
                root: true,
                now: 1_000_000,
                ..Default::default()
            };
            for (name, status, enabled) in services {
                b.services
                    .insert(name.to_string(), (status.to_string(), *enabled));
            }
            b
        }

        fn details(&mut self, name: &str) -> Option<ServiceDetails> {
            let (status, default) = self.services.get(name).cloned()?;
            let e = self.enablement(name, default);
            Some(ServiceDetails {
                name: name.into(),
                status,
                pid: None,
                supervisor: "root".into(),
                enabled: e.enabled,
                enabled_source: e.source_name().into(),
                service_type: "simple".into(),
                remain_after_exit: false,
                after_all: false,
                last: false,
                failure_action: "none".into(),
                source: Some(format!("/usr/lib/dynamod/services/{name}.toml")),
            })
        }
    }

    impl Backend for FakeBackend {
        fn request(&mut self, body: MessageBody) -> Result<MessageBody, String> {
            if self.svmgr_down {
                return Err("cannot connect to /run/dynamod/control.sock".into());
            }
            self.requests.push(format!("{body:?}"));
            let unknown = |n: &str| MessageBody::Error {
                message: format!("unknown service: {n}"),
            };
            Ok(match body {
                MessageBody::StartService { ref name }
                | MessageBody::StopService { ref name }
                | MessageBody::RestartService { ref name } => {
                    if self.services.contains_key(name) {
                        MessageBody::Ack
                    } else {
                        unknown(name)
                    }
                }
                MessageBody::DescribeService { ref name } => match self.details(name) {
                    Some(d) => MessageBody::ServiceDetails(d),
                    None => unknown(name),
                },
                MessageBody::ListServices => {
                    let names: Vec<String> = self.services.keys().cloned().collect();
                    let services = names
                        .iter()
                        .map(|n| {
                            let d = self.details(n).unwrap();
                            ServiceEntry {
                                name: d.name,
                                status: d.status,
                                pid: d.pid,
                                enabled: Some(d.enabled),
                            }
                        })
                        .collect();
                    MessageBody::ServiceList { services }
                }
                _ => MessageBody::Ack,
            })
        }

        fn unit_files(&mut self) -> Vec<UnitFile> {
            let mut files: Vec<UnitFile> = self
                .services
                .iter()
                .map(|(n, (_, en))| UnitFile {
                    name: n.clone(),
                    path: PathBuf::from(format!("/usr/lib/dynamod/services/{n}.toml")),
                    enabled_default: *en,
                    masked: false,
                })
                .collect();
            for m in &self.masked {
                files.push(UnitFile {
                    name: m.clone(),
                    path: PathBuf::from(format!("/etc/dynamod/services/{m}.toml")),
                    enabled_default: false,
                    masked: true,
                });
            }
            files
        }

        fn enablement(&mut self, name: &str, default: bool) -> Enablement {
            if self.disabled_markers.contains(name) {
                Enablement {
                    enabled: false,
                    source: EnablementSource::DisabledMarker,
                }
            } else if self.enabled_markers.contains(name) {
                Enablement {
                    enabled: true,
                    source: EnablementSource::EnabledMarker,
                }
            } else {
                Enablement {
                    enabled: default,
                    source: EnablementSource::Definition,
                }
            }
        }

        fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<bool, String> {
            if !self.root {
                return Err("Permission denied (os error 13)".into());
            }
            let (keep, drop) = if enabled {
                (&mut self.enabled_markers, &mut self.disabled_markers)
            } else {
                (&mut self.disabled_markers, &mut self.enabled_markers)
            };
            let removed = drop.remove(name);
            let added = keep.insert(name.to_string());
            Ok(removed || added)
        }

        fn marker_path(&self, enabled: bool, name: &str) -> String {
            format!(
                "/etc/dynamod/{}/{name}",
                if enabled { "enabled" } else { "disabled" }
            )
        }

        fn reboot_now(&mut self, kind: ShutdownKind, sync: bool) -> String {
            self.reboots.push((kind, sync));
            "Operation not permitted (fake)".into()
        }

        fn wall(&mut self, message: &str) {
            self.walls.push(message.to_string());
        }

        fn is_root(&self) -> bool {
            self.root
        }

        fn now(&self) -> u64 {
            self.now
        }

        fn next_local_time(&self, now: u64, hour: u32, minute: u32) -> u64 {
            // The fake's local time is UTC.
            let day = now - now % 86_400;
            let t = day + (hour as u64) * 3600 + (minute as u64) * 60;
            if t > now { t } else { t + 86_400 }
        }

        fn format_local_time(&self, t: u64) -> String {
            format!("@{t}")
        }

        fn set_root(&mut self, _root: PathBuf) {}
    }

    /// Run `f` with a context on `backend`, returning (exit code, stdout, stderr).
    pub fn run(
        backend: &mut FakeBackend,
        f: impl FnOnce(&mut Ctx) -> i32,
    ) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut ctx = Ctx {
                backend,
                out: &mut out,
                err: &mut err,
            };
            f(&mut ctx)
        };
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }
}
