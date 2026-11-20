//! enable / disable / is-enabled, shared by `dynamodctl` and `systemctl`.
//!
//! These work on the files (/etc/dynamod/{enabled,disabled}/NAME and the
//! definitions), so they need no running svmgr; only `--now` talks to it.

use dynamod_common::config_dirs::validate_service_name;
use dynamod_common::protocol::MessageBody;

use crate::backend::Ctx;
use crate::{complain, say};

/// Look `name` up among the definition files: Ok(default enabled), or an
/// error message.
fn find_definition(ctx: &mut Ctx, name: &str) -> Result<bool, String> {
    validate_service_name(name)?;
    let files = ctx.backend.unit_files();
    if let Some(f) = files.iter().find(|f| !f.masked && f.name == name) {
        return Ok(f.enabled_default);
    }
    if files.iter().any(|f| f.masked && f.name == name) {
        return Err(format!("service '{name}' is masked"));
    }
    Err(format!("no such service: '{name}'"))
}

/// Enable (or disable) each service; with `now`, also start (or stop) it.
/// Exit code 0 if everything worked, 1 otherwise.
pub fn set_enabled(ctx: &mut Ctx, names: &[String], enable: bool, now: bool, quiet: bool) -> i32 {
    let verb = if enable { "enable" } else { "disable" };
    let mut rc = 0;
    for name in names {
        if let Err(e) = find_definition(ctx, name) {
            complain!(ctx, "Failed to {verb} '{name}': {e}");
            rc = 1;
            continue;
        }
        match ctx.backend.set_enabled(name, enable) {
            Ok(true) if !quiet => {
                let marker = ctx.backend.marker_path(enable, name);
                say!(ctx, "{name}: {verb}d ({marker})");
            }
            Ok(_) => {}
            Err(e) => {
                complain!(ctx, "Failed to {verb} '{name}': {e}");
                rc = 1;
                continue;
            }
        }
        if now {
            let body = if enable {
                MessageBody::StartService { name: name.clone() }
            } else {
                MessageBody::StopService { name: name.clone() }
            };
            let action = if enable { "start" } else { "stop" };
            match ctx.backend.request(body) {
                Ok(MessageBody::Ack) => {}
                Ok(MessageBody::Error { message }) => {
                    complain!(ctx, "Failed to {action} '{name}': {message}");
                    rc = 1;
                }
                Ok(_) => {
                    complain!(ctx, "Failed to {action} '{name}': unexpected response");
                    rc = 1;
                }
                Err(e) => {
                    complain!(ctx, "Failed to {action} '{name}': {e}");
                    rc = 1;
                }
            }
        }
    }
    rc
}

/// Print "enabled"/"disabled"/"masked" for each service. Exit code 0 if at
/// least one is enabled, 1 otherwise (as systemctl).
pub fn is_enabled(ctx: &mut Ctx, names: &[String], quiet: bool) -> i32 {
    let mut any = false;
    for name in names {
        match find_definition(ctx, name) {
            Ok(default) => {
                let e = ctx.backend.enablement(name, default);
                any |= e.enabled;
                if !quiet {
                    say!(ctx, "{}", e.state());
                }
            }
            Err(e) if e.ends_with("is masked") => {
                if !quiet {
                    say!(ctx, "masked");
                }
            }
            Err(e) => complain!(ctx, "Failed to get the state of '{name}': {e}"),
        }
    }
    if any { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, run};

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn enable_writes_marker_and_now_starts() {
        let mut b = FakeBackend::with(&[("sshd", "stopped", false)]);
        let (rc, out, _) = run(&mut b, |c| {
            set_enabled(c, &names(&["sshd"]), true, true, false)
        });
        assert_eq!(rc, 0);
        assert!(out.contains("/etc/dynamod/enabled/sshd"));
        assert!(b.enabled_markers.contains("sshd"));
        assert_eq!(b.requests, vec!["StartService { name: \"sshd\" }"]);
    }

    #[test]
    fn disable_now_stops() {
        let mut b = FakeBackend::with(&[("sshd", "running", true)]);
        b.enabled_markers.insert("sshd".into());
        let (rc, _, _) = run(&mut b, |c| {
            set_enabled(c, &names(&["sshd"]), false, true, true)
        });
        assert_eq!(rc, 0);
        assert!(b.disabled_markers.contains("sshd"));
        assert!(!b.enabled_markers.contains("sshd"));
        assert_eq!(b.requests, vec!["StopService { name: \"sshd\" }"]);
    }

    #[test]
    fn enable_unknown_or_masked_fails() {
        let mut b = FakeBackend::with(&[("sshd", "stopped", false)]);
        b.masked.insert("dhcpcd".into());
        let (rc, _, err) = run(&mut b, |c| {
            set_enabled(c, &names(&["nope", "dhcpcd", "sshd"]), true, false, false)
        });
        assert_eq!(rc, 1);
        assert!(err.contains("no such service: 'nope'"));
        assert!(err.contains("'dhcpcd' is masked"));
        assert!(
            b.enabled_markers.contains("sshd"),
            "the valid name is still enabled"
        );
        assert!(b.requests.is_empty(), "no --now, no requests");
    }

    #[test]
    fn enable_without_permission_fails() {
        let mut b = FakeBackend::with(&[("sshd", "stopped", false)]);
        b.root = false;
        let (rc, _, err) = run(&mut b, |c| {
            set_enabled(c, &names(&["sshd"]), true, false, false)
        });
        assert_eq!(rc, 1);
        assert!(err.contains("Permission denied"));
    }

    #[test]
    fn is_enabled_exit_codes() {
        let mut b = FakeBackend::with(&[("core", "running", true), ("opt", "stopped", false)]);
        b.masked.insert("gone".into());
        assert_eq!(
            run(&mut b, |c| is_enabled(c, &names(&["core"]), false)),
            (0, "enabled\n".into(), String::new())
        );
        assert_eq!(run(&mut b, |c| is_enabled(c, &names(&["opt"]), false)).0, 1);
        assert_eq!(
            run(&mut b, |c| is_enabled(c, &names(&["gone"]), false)),
            (1, "masked\n".into(), String::new())
        );
        let (rc, out, err) = run(&mut b, |c| is_enabled(c, &names(&["nope"]), false));
        assert_eq!((rc, out.as_str()), (1, ""));
        assert!(err.contains("no such service"));

        b.disabled_markers.insert("core".into());
        b.enabled_markers.insert("opt".into());
        assert_eq!(
            run(&mut b, |c| is_enabled(c, &names(&["core"]), true)),
            (1, String::new(), String::new())
        );
        assert_eq!(run(&mut b, |c| is_enabled(c, &names(&["opt"]), true)).0, 0);
        assert_eq!(
            run(&mut b, |c| is_enabled(c, &names(&["core", "opt"]), true)).0,
            0,
            "any enabled"
        );
    }
}
