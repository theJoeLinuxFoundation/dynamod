/// `systemctl`: a small compatibility front end for administrators and
/// scripts, in terms of dynamod's services. It is not systemd: only
/// services exist (`NAME.service` is `NAME`), and only the verbs below.
use std::path::PathBuf;

use dynamod_common::protocol::{MessageBody, ServiceDetails, ShutdownKind};

use crate::backend::Ctx;
use crate::{complain, ops, power, say};

const OTHER_UNIT_TYPES: &[&str] = &[
    "socket",
    "timer",
    "target",
    "mount",
    "automount",
    "path",
    "slice",
    "scope",
    "device",
    "swap",
];

/// Parsed command line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub verb: Option<String>,
    pub args: Vec<String>,
    pub quiet: bool,
    pub now: bool,
    pub all: bool,
    pub force: bool,
    pub value: bool,
    pub no_legend: bool,
    pub no_wall: bool,
    pub types: Vec<String>,
    pub states: Vec<String>,
    pub properties: Vec<String>,
    pub root: Option<String>,
    pub message: Option<String>,
    pub version: bool,
    pub help: bool,
}

/// Options that take a value: (long name, short letter).
const VALUE_OPTIONS: &[(&str, Option<char>)] = &[
    ("type", Some('t')),
    ("property", Some('p')),
    ("state", None),
    ("root", None),
    ("message", None),
];

fn set_value(inv: &mut Invocation, name: &str, value: String) {
    let list = || {
        value
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    match name {
        "type" => inv.types.extend(list()),
        "property" => inv.properties.extend(list()),
        "state" => inv.states.extend(list()),
        "root" => inv.root = Some(value),
        "message" => inv.message = Some(value),
        _ => {}
    }
}

fn set_flag(inv: &mut Invocation, name: &str) -> Result<(), String> {
    match name {
        "quiet" | "q" => inv.quiet = true,
        "now" => inv.now = true,
        "all" | "a" => inv.all = true,
        "force" | "f" => inv.force = true,
        "value" => inv.value = true,
        "no-legend" => inv.no_legend = true,
        "no-wall" => inv.no_wall = true,
        "version" => inv.version = true,
        "help" | "h" => inv.help = true,
        // Accepted, no effect: dynamod never pages, never asks for
        // passwords, and start/stop never wait for the job anyway.
        "no-pager" | "no-block" | "no-ask-password" | "plain" | "full" | "l" | "system"
        | "no-reload" => {}
        "user" | "global" => {
            return Err("dynamod has no per-user service manager (--user is not supported)".into());
        }
        "host" | "H" | "machine" | "M" => return Err(format!("--{name} is not supported")),
        other => {
            let dashes = if other.len() == 1 { "-" } else { "--" };
            return Err(format!("unknown option '{dashes}{other}'"));
        }
    }
    Ok(())
}

/// Parse systemctl's command line (without the program name).
pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut inv = Invocation::default();
    let mut positionals: Vec<String> = Vec::new();
    let mut it = args.iter().cloned();
    let mut only_positionals = false;

    while let Some(a) = it.next() {
        if only_positionals || a == "-" || !a.starts_with('-') {
            positionals.push(a);
            continue;
        }
        if a == "--" {
            only_positionals = true;
            continue;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            if VALUE_OPTIONS.iter().any(|(n, _)| *n == name) {
                let value = match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .ok_or(format!("option '--{name}' needs a value"))?,
                };
                set_value(&mut inv, name, value);
            } else if inline.is_some() {
                return Err(format!("option '--{name}' takes no value"));
            } else {
                set_flag(&mut inv, name)?;
            }
            continue;
        }
        // Short options, possibly combined (-aq) or with a glued value (-tservice).
        let chars: Vec<char> = a[1..].chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if let Some((name, _)) = VALUE_OPTIONS.iter().find(|(_, s)| *s == Some(c)) {
                let rest: String = chars[i + 1..].iter().collect();
                let value = if rest.is_empty() {
                    it.next().ok_or(format!("option '-{c}' needs a value"))?
                } else {
                    rest
                };
                set_value(&mut inv, name, value);
                break;
            }
            set_flag(&mut inv, &c.to_string())?;
            i += 1;
        }
    }

    let mut positionals = positionals.into_iter();
    inv.verb = positionals.next();
    inv.args = positionals.collect();
    Ok(inv)
}

/// `NAME.service` -> `NAME`; other unit types are an error.
pub fn unit_to_service(unit: &str) -> Result<String, String> {
    if let Some(name) = unit.strip_suffix(".service") {
        return Ok(name.to_string());
    }
    if let Some((_, suffix)) = unit.rsplit_once('.') {
        if OTHER_UNIT_TYPES.contains(&suffix) {
            return Err(format!(
                "{unit}: dynamod manages services only; {suffix} units are not supported"
            ));
        }
    }
    Ok(unit.to_string())
}

/// dynamod status -> (ActiveState, SubState).
pub fn active_state(status: &str) -> (&'static str, &'static str) {
    match status {
        "running" => ("active", "running"),
        "exited" => ("active", "exited"),
        "starting" | "waiting-ready" => ("activating", "start"),
        "stopping" => ("deactivating", "stop"),
        "stopped" => ("inactive", "dead"),
        "abandoned" => ("failed", "failed"),
        s if s.starts_with("failed") => ("failed", "failed"),
        _ => ("inactive", "dead"),
    }
}

pub fn usage() -> String {
    "Usage: systemctl [OPTIONS] COMMAND [NAME...]\n\n\
     A compatibility front end for dynamod (this is not systemd).\n\
     NAME may carry a .service suffix; other unit types do not exist.\n\n\
     Commands:\n  \
     start|stop|restart NAME...     start, stop or restart services\n  \
     reload-or-restart NAME...      restart (dynamod has no reload operation)\n  \
     try-restart NAME...            restart the services that are active\n  \
     status [NAME...]               show service status (exit 3 if inactive, 4 if unknown)\n  \
     show [-p PROP[,PROP]] NAME...  show properties (--value: values only)\n  \
     is-active|is-failed NAME...    exit 0 if any is active / failed\n  \
     is-enabled NAME...             exit 0 if any is enabled at boot\n  \
     enable|disable [--now] NAME... write /etc/dynamod/{enabled,disabled}/NAME\n  \
     list-units [--all] [--state=S] list services (active and failed ones without --all)\n  \
     list-unit-files                list service definitions and their boot state\n  \
     daemon-reload                  re-read the definitions (dynamodctl reload)\n  \
     reboot|poweroff|halt [-f]      clean shutdown; -f: immediately (root)\n\n\
     Accepted, no effect: --no-pager --no-block --no-ask-password --plain --full --system\n"
        .to_string()
}

pub fn version() -> String {
    format!(
        "dynamod systemctl front end {} (dynamodctl)\n\
         This is not systemd: it maps a subset of systemctl onto dynamod services.\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// Entry point: parse and run. Returns the exit code.
pub fn main(ctx: &mut Ctx, args: &[String]) -> i32 {
    let inv = match parse(args) {
        Ok(inv) => inv,
        Err(e) => {
            complain!(ctx, "systemctl: {e}");
            return 1;
        }
    };
    if let Some(ref root) = inv.root {
        ctx.backend.set_root(PathBuf::from(root));
    }
    run(ctx, &inv)
}

fn services_of(ctx: &mut Ctx, inv: &Invocation) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for unit in &inv.args {
        match unit_to_service(unit) {
            Ok(n) => names.push(n),
            Err(e) => {
                complain!(ctx, "systemctl: {e}");
                return None;
            }
        }
    }
    Some(names)
}

fn check_types(ctx: &mut Ctx, inv: &Invocation) -> bool {
    for t in &inv.types {
        if t != "service" {
            complain!(
                ctx,
                "systemctl: dynamod manages services only; --type={t} is not supported"
            );
            return false;
        }
    }
    true
}

/// Run a parsed invocation.
pub fn run(ctx: &mut Ctx, inv: &Invocation) -> i32 {
    if inv.help {
        let _ = write!(ctx.out, "{}", usage());
        return 0;
    }
    if inv.version {
        let _ = write!(ctx.out, "{}", version());
        return 0;
    }
    let Some(verb) = inv.verb.as_deref() else {
        // Bare `systemctl` lists units, as systemd does.
        return list_units(ctx, inv);
    };
    let Some(names) = services_of(ctx, inv) else {
        return 1;
    };
    let need_names = |ctx: &mut Ctx| {
        if names.is_empty() {
            complain!(ctx, "systemctl: '{verb}' needs at least one service name");
            false
        } else {
            true
        }
    };

    match verb {
        "start"
        | "stop"
        | "restart"
        | "reload-or-restart"
        | "try-restart"
        | "try-reload-or-restart" => {
            if !need_names(ctx) {
                return 1;
            }
            let mut rc = 0;
            for name in &names {
                let r = start_stop(ctx, verb, name);
                if r != 0 {
                    rc = r;
                }
            }
            rc
        }
        "reload" => {
            complain!(
                ctx,
                "systemctl: dynamod services have no reload operation; use restart"
            );
            1
        }
        "status" => {
            if names.is_empty() {
                return list_units(ctx, inv);
            }
            let mut rc = 0;
            for (i, name) in names.iter().enumerate() {
                if i > 0 {
                    say!(ctx, "");
                }
                let r = status(ctx, name);
                if rc == 0 {
                    rc = r;
                }
            }
            rc
        }
        "show" => {
            if !need_names(ctx) {
                return 1;
            }
            show(ctx, inv, &names)
        }
        "is-active" | "is-failed" => {
            if !need_names(ctx) {
                return 1;
            }
            let want_failed = verb == "is-failed";
            let mut any = false;
            for name in &names {
                let state = match describe(ctx, name) {
                    Ok(Some(d)) => active_state(&d.status).0,
                    Ok(None) => "inactive",
                    Err(e) => {
                        complain!(ctx, "systemctl: {e}");
                        return 1;
                    }
                };
                any |= if want_failed {
                    state == "failed"
                } else {
                    state == "active"
                };
                if !inv.quiet {
                    say!(ctx, "{state}");
                }
            }
            match (any, want_failed) {
                (true, _) => 0,
                (false, false) => 3,
                (false, true) => 1,
            }
        }
        "is-enabled" => {
            if !need_names(ctx) {
                return 1;
            }
            ops::is_enabled(ctx, &names, inv.quiet)
        }
        "enable" | "disable" => {
            if !need_names(ctx) {
                return 1;
            }
            ops::set_enabled(ctx, &names, verb == "enable", inv.now, inv.quiet)
        }
        "list-units" => {
            if !check_types(ctx, inv) {
                return 1;
            }
            list_units(ctx, inv)
        }
        "list-unit-files" => {
            if !check_types(ctx, inv) {
                return 1;
            }
            list_unit_files(ctx, inv)
        }
        "daemon-reload" => match ctx.backend.request(MessageBody::Reload) {
            Ok(MessageBody::Ack) => 0,
            Ok(MessageBody::Error { message }) => {
                complain!(ctx, "systemctl: {message}");
                1
            }
            Ok(_) => 1,
            Err(e) => {
                complain!(ctx, "systemctl: {e}");
                1
            }
        },
        "reboot" | "poweroff" | "halt" => {
            let kind = match verb {
                "reboot" => ShutdownKind::Reboot,
                "poweroff" => ShutdownKind::Poweroff,
                _ => ShutdownKind::Halt,
            };
            let cmd = power::PowerCmd::Now {
                kind,
                force: inv.force,
                sync: true,
                wall: !inv.no_wall,
                message: inv.message.clone(),
            };
            power::run(ctx, "systemctl", cmd)
        }
        other => {
            complain!(
                ctx,
                "systemctl: unknown command verb '{other}' (see systemctl --help)"
            );
            1
        }
    }
}

/// svmgr's details of `name`: Ok(None) if it has no such service.
fn describe(ctx: &mut Ctx, name: &str) -> Result<Option<ServiceDetails>, String> {
    match ctx.backend.request(MessageBody::DescribeService {
        name: name.to_string(),
    })? {
        MessageBody::ServiceDetails(d) => Ok(Some(d)),
        MessageBody::Error { message } if message.starts_with("unknown service") => Ok(None),
        MessageBody::Error { message } => Err(message),
        _ => Err("unexpected response from dynamod".into()),
    }
}

fn start_stop(ctx: &mut Ctx, verb: &str, name: &str) -> i32 {
    let body = match verb {
        "start" => MessageBody::StartService { name: name.into() },
        "stop" => MessageBody::StopService { name: name.into() },
        "try-restart" | "try-reload-or-restart" => {
            match describe(ctx, name) {
                Ok(Some(d)) if active_state(&d.status).0 == "active" => {}
                Ok(Some(_)) => return 0,
                Ok(None) => {
                    complain!(
                        ctx,
                        "Failed to restart {name}.service: unknown service: {name}"
                    );
                    return 5;
                }
                Err(e) => {
                    complain!(ctx, "Failed to restart {name}.service: {e}");
                    return 1;
                }
            }
            MessageBody::RestartService { name: name.into() }
        }
        _ => MessageBody::RestartService { name: name.into() },
    };
    let action = match verb {
        "start" => "start",
        "stop" => "stop",
        _ => "restart",
    };
    match ctx.backend.request(body) {
        Ok(MessageBody::Ack) => 0,
        Ok(MessageBody::Error { message }) => {
            complain!(ctx, "Failed to {action} {name}.service: {message}");
            if message.starts_with("unknown service") {
                5
            } else {
                1
            }
        }
        Ok(_) => {
            complain!(
                ctx,
                "Failed to {action} {name}.service: unexpected response"
            );
            1
        }
        Err(e) => {
            complain!(ctx, "Failed to {action} {name}.service: {e}");
            1
        }
    }
}

fn status(ctx: &mut Ctx, name: &str) -> i32 {
    let d = match describe(ctx, name) {
        Ok(Some(d)) => d,
        Ok(None) => {
            complain!(ctx, "Unit {name}.service could not be found.");
            return 4;
        }
        Err(e) => {
            complain!(ctx, "systemctl: {e}");
            return 1;
        }
    };
    let (active, sub) = active_state(&d.status);
    let enabled = if d.enabled { "enabled" } else { "disabled" };
    let source = d.source.as_deref().unwrap_or("-");
    say!(ctx, "* {name}.service - dynamod service {name}");
    say!(
        ctx,
        "     Loaded: loaded ({source}; {enabled}; from {})",
        d.enabled_source
    );
    say!(ctx, "     Active: {active} ({sub})");
    if let Some(pid) = d.pid {
        say!(ctx, "   Main PID: {pid}");
    }
    say!(ctx, "       Type: {}", d.service_type);
    say!(ctx, " Supervisor: {}", d.supervisor);
    let mut options = Vec::new();
    if d.remain_after_exit {
        options.push("remain-after-exit".to_string());
    }
    if d.after_all {
        options.push("after-all".to_string());
    }
    if d.last {
        options.push("last".to_string());
    }
    if d.failure_action != "none" {
        options.push(format!("failure-action={}", d.failure_action));
    }
    if !options.is_empty() {
        say!(ctx, "    Options: {}", options.join(", "));
    }
    if active == "active" { 0 } else { 3 }
}

fn properties(name: &str, d: Option<&ServiceDetails>) -> Vec<(&'static str, String)> {
    let unit = format!("{name}.service");
    let Some(d) = d else {
        return vec![
            ("Id", unit.clone()),
            ("Names", unit),
            ("LoadState", "not-found".into()),
            ("ActiveState", "inactive".into()),
            ("SubState", "dead".into()),
            ("UnitFileState", String::new()),
            ("MainPID", "0".into()),
        ];
    };
    let (active, sub) = active_state(&d.status);
    let yes_no = |b: bool| if b { "yes" } else { "no" }.to_string();
    vec![
        ("Id", unit.clone()),
        ("Names", unit),
        ("Description", format!("dynamod service {name}")),
        ("LoadState", "loaded".into()),
        ("ActiveState", active.into()),
        ("SubState", sub.into()),
        (
            "UnitFileState",
            if d.enabled { "enabled" } else { "disabled" }.into(),
        ),
        ("MainPID", d.pid.unwrap_or(0).to_string()),
        ("Type", d.service_type.clone()),
        ("RemainAfterExit", yes_no(d.remain_after_exit)),
        ("FragmentPath", d.source.clone().unwrap_or_default()),
    ]
}

fn show(ctx: &mut Ctx, inv: &Invocation, names: &[String]) -> i32 {
    for (i, name) in names.iter().enumerate() {
        let d = match describe(ctx, name) {
            Ok(d) => d,
            Err(e) => {
                complain!(ctx, "systemctl: {e}");
                return 1;
            }
        };
        if i > 0 && !inv.value {
            say!(ctx, "");
        }
        let props = properties(name, d.as_ref());
        let wanted: Vec<(&str, String)> = if inv.properties.is_empty() {
            props
        } else {
            // In the order asked for; unknown properties print nothing.
            inv.properties
                .iter()
                .filter_map(|p| props.iter().find(|(k, _)| k == p).cloned())
                .collect()
        };
        for (k, v) in wanted {
            if inv.value {
                say!(ctx, "{v}");
            } else {
                say!(ctx, "{k}={v}");
            }
        }
    }
    0
}

fn table(ctx: &mut Ctx, header: &[&str], rows: &[Vec<String>], legend: bool) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let line = |cells: Vec<&str>| {
        let last = cells.len() - 1;
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == last {
                    c.to_string()
                } else {
                    format!("{c:<w$}", w = widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    if legend {
        say!(ctx, "{}", line(header.to_vec()));
    }
    for row in rows {
        say!(ctx, "{}", line(row.iter().map(String::as_str).collect()));
    }
}

fn list_units(ctx: &mut Ctx, inv: &Invocation) -> i32 {
    let services = match ctx.backend.request(MessageBody::ListServices) {
        Ok(MessageBody::ServiceList { services }) => services,
        Ok(MessageBody::Error { message }) => {
            complain!(ctx, "systemctl: {message}");
            return 1;
        }
        Ok(_) => {
            complain!(ctx, "systemctl: unexpected response from dynamod");
            return 1;
        }
        Err(e) => {
            complain!(ctx, "systemctl: {e}");
            return 1;
        }
    };
    let mut services = services;
    services.sort_by(|a, b| a.name.cmp(&b.name));
    let patterns: Vec<String> = inv
        .args
        .iter()
        .filter_map(|a| unit_to_service(a).ok())
        .collect();
    let rows: Vec<Vec<String>> = services
        .iter()
        .filter(|s| patterns.is_empty() || patterns.iter().any(|p| glob(p, &s.name)))
        .filter_map(|s| {
            let (active, sub) = active_state(&s.status);
            let shown = if !inv.states.is_empty() {
                inv.states.iter().any(|st| st == active || st == sub)
            } else {
                inv.all || active != "inactive"
            };
            shown.then(|| {
                vec![
                    format!("{}.service", s.name),
                    "loaded".into(),
                    active.into(),
                    sub.into(),
                    format!("dynamod service {}", s.name),
                ]
            })
        })
        .collect();
    let legend = !inv.no_legend;
    table(
        ctx,
        &["UNIT", "LOAD", "ACTIVE", "SUB", "DESCRIPTION"],
        &rows,
        legend,
    );
    if legend {
        say!(ctx, "\n{} loaded units listed.", rows.len());
        if !inv.all && inv.states.is_empty() {
            say!(
                ctx,
                "To show all installed services use 'systemctl list-units --all'."
            );
        }
    }
    0
}

fn list_unit_files(ctx: &mut Ctx, inv: &Invocation) -> i32 {
    let files = ctx.backend.unit_files();
    let patterns: Vec<String> = inv
        .args
        .iter()
        .filter_map(|a| unit_to_service(a).ok())
        .collect();
    let mut rows = Vec::new();
    for f in &files {
        if !patterns.is_empty() && !patterns.iter().any(|p| glob(p, &f.name)) {
            continue;
        }
        let state = if f.masked {
            "masked"
        } else {
            ctx.backend.enablement(&f.name, f.enabled_default).state()
        };
        if !inv.states.is_empty() && !inv.states.iter().any(|s| s == state) {
            continue;
        }
        rows.push(vec![format!("{}.service", f.name), state.to_string()]);
    }
    let legend = !inv.no_legend;
    table(ctx, &["UNIT FILE", "STATE"], &rows, legend);
    if legend {
        say!(ctx, "\n{} unit files listed.", rows.len());
    }
    0
}

/// Shell-style `*` and `?` matching, for `list-units PATTERN`.
fn glob(pattern: &str, name: &str) -> bool {
    fn m(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => m(&p[1..], n) || (!n.is_empty() && m(p, &n[1..])),
            (Some(b'?'), Some(_)) => m(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => m(&p[1..], &n[1..]),
            _ => false,
        }
    }
    m(pattern.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, run as run_fake};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn sys(b: &mut FakeBackend, args: &[&str]) -> (i32, String, String) {
        let args = s(args);
        run_fake(b, |c| main(c, &args))
    }

    fn backend() -> FakeBackend {
        FakeBackend::with(&[
            ("sshd", "running", false),
            ("dave-finalize", "exited", true),
            ("broken", "failed", true),
            ("idle", "stopped", true),
        ])
    }

    #[test]
    fn option_parsing() {
        let inv = parse(&s(&[
            "--no-pager",
            "-q",
            "show",
            "-p",
            "ActiveState",
            "--property=SubState,MainPID",
            "sshd",
        ]))
        .unwrap();
        assert_eq!(inv.verb.as_deref(), Some("show"));
        assert_eq!(inv.args, vec!["sshd"]);
        assert!(inv.quiet);
        assert_eq!(inv.properties, vec!["ActiveState", "SubState", "MainPID"]);

        let inv = parse(&s(&["list-units", "-at", "service", "--no-legend"])).unwrap();
        assert!(inv.all && inv.no_legend);
        assert_eq!(inv.types, vec!["service"]);
        let inv = parse(&s(&["list-units", "-tservice", "--type=service"])).unwrap();
        assert_eq!(inv.types, vec!["service", "service"]);

        let inv = parse(&s(&["enable", "--now", "sshd.service", "--no-block"])).unwrap();
        assert!(inv.now);
        assert_eq!(inv.args, vec!["sshd.service"]);

        assert!(parse(&s(&["--user", "start", "x"])).is_err());
        assert!(parse(&s(&["--frob", "start"])).is_err());
        assert!(parse(&s(&["-Z"])).is_err());
        assert!(parse(&s(&["show", "-p"])).is_err(), "missing value");
        assert!(
            parse(&s(&["--now=yes", "start"])).is_err(),
            "flag with value"
        );
    }

    #[test]
    fn unit_names() {
        assert_eq!(unit_to_service("sshd.service"), Ok("sshd".into()));
        assert_eq!(unit_to_service("sshd"), Ok("sshd".into()));
        assert_eq!(
            unit_to_service("network.dhcpcd"),
            Ok("network.dhcpcd".into())
        );
        for other in [
            "fstrim.timer",
            "sshd.socket",
            "multi-user.target",
            "boot.mount",
        ] {
            assert!(
                unit_to_service(other)
                    .unwrap_err()
                    .contains("services only")
            );
        }
    }

    #[test]
    fn start_stop_restart_map_to_requests() {
        let mut b = backend();
        assert_eq!(sys(&mut b, &["start", "sshd.service", "idle"]).0, 0);
        assert_eq!(sys(&mut b, &["stop", "sshd"]).0, 0);
        assert_eq!(sys(&mut b, &["restart", "sshd"]).0, 0);
        assert_eq!(sys(&mut b, &["reload-or-restart", "sshd"]).0, 0);
        assert_eq!(
            b.requests,
            vec![
                "StartService { name: \"sshd\" }",
                "StartService { name: \"idle\" }",
                "StopService { name: \"sshd\" }",
                "RestartService { name: \"sshd\" }",
                "RestartService { name: \"sshd\" }",
            ]
        );
    }

    #[test]
    fn try_restart_only_restarts_active_services() {
        let mut b = backend();
        assert_eq!(sys(&mut b, &["try-restart", "idle"]).0, 0);
        assert!(!b.requests.iter().any(|r| r.starts_with("RestartService")));
        assert_eq!(sys(&mut b, &["try-restart", "sshd"]).0, 0);
        assert_eq!(
            b.requests.last().unwrap(),
            "RestartService { name: \"sshd\" }"
        );
    }

    #[test]
    fn exit_codes() {
        let mut b = backend();
        // is-active: 0 active (running or exited), 3 otherwise
        assert_eq!(
            sys(&mut b, &["is-active", "sshd"]),
            (0, "active\n".into(), String::new())
        );
        assert_eq!(sys(&mut b, &["is-active", "dave-finalize"]).0, 0);
        assert_eq!(
            sys(&mut b, &["is-active", "idle"]),
            (3, "inactive\n".into(), String::new())
        );
        assert_eq!(
            sys(&mut b, &["is-active", "broken"]),
            (3, "failed\n".into(), String::new())
        );
        assert_eq!(
            sys(&mut b, &["is-active", "nope"]),
            (3, "inactive\n".into(), String::new())
        );
        assert_eq!(
            sys(&mut b, &["is-active", "-q", "idle", "sshd"]),
            (0, String::new(), String::new())
        );
        // is-failed: 0 if failed, 1 otherwise
        assert_eq!(sys(&mut b, &["is-failed", "broken"]).0, 0);
        assert_eq!(sys(&mut b, &["is-failed", "sshd"]).0, 1);
        // is-enabled: 0 / 1
        assert_eq!(
            sys(&mut b, &["is-enabled", "idle.service"]),
            (0, "enabled\n".into(), String::new())
        );
        assert_eq!(
            sys(&mut b, &["is-enabled", "sshd"]),
            (1, "disabled\n".into(), String::new())
        );
        // status: 0 active, 3 inactive, 4 unknown
        assert_eq!(sys(&mut b, &["status", "sshd"]).0, 0);
        assert_eq!(sys(&mut b, &["status", "idle"]).0, 3);
        let (rc, _, err) = sys(&mut b, &["status", "nope"]);
        assert_eq!(rc, 4);
        assert!(err.contains("could not be found"));
        // start of an unknown service: 5, as systemd
        assert_eq!(sys(&mut b, &["start", "nope"]).0, 5);
        // unknown verb, other unit types, no names, reload: 1
        let (rc, _, err) = sys(&mut b, &["frobnicate", "x"]);
        assert_eq!(rc, 1);
        assert!(err.contains("unknown command verb 'frobnicate'"));
        let (rc, _, err) = sys(&mut b, &["start", "fstrim.timer"]);
        assert_eq!(rc, 1);
        assert!(err.contains("timer units are not supported"));
        assert_eq!(sys(&mut b, &["start"]).0, 1);
        assert_eq!(sys(&mut b, &["reload", "sshd"]).0, 1);
        assert_eq!(sys(&mut b, &["list-units", "--type=socket"]).0, 1);
        // svmgr unreachable
        b.svmgr_down = true;
        assert_eq!(sys(&mut b, &["is-active", "sshd"]).0, 1);
        assert_eq!(sys(&mut b, &["start", "sshd"]).0, 1);
        // ... but enable works on the files alone
        assert_eq!(sys(&mut b, &["enable", "sshd"]).0, 0);
        assert!(b.enabled_markers.contains("sshd"));
    }

    #[test]
    fn enable_disable_now() {
        let mut b = backend();
        assert_eq!(sys(&mut b, &["enable", "--now", "sshd.service"]).0, 0);
        assert!(b.enabled_markers.contains("sshd"));
        assert_eq!(
            b.requests.last().unwrap(),
            "StartService { name: \"sshd\" }"
        );
        assert_eq!(sys(&mut b, &["disable", "--now", "sshd"]).0, 0);
        assert!(b.disabled_markers.contains("sshd"));
        assert_eq!(b.requests.last().unwrap(), "StopService { name: \"sshd\" }");
        assert_eq!(sys(&mut b, &["enable", "nope"]).0, 1);
    }

    #[test]
    fn show_properties() {
        let mut b = backend();
        assert_eq!(
            sys(&mut b, &["show", "-p", "ActiveState", "sshd"]),
            (0, "ActiveState=active\n".into(), String::new())
        );
        assert_eq!(
            sys(
                &mut b,
                &[
                    "show",
                    "--property=ActiveState,SubState",
                    "--value",
                    "dave-finalize.service"
                ]
            ),
            (0, "active\nexited\n".into(), String::new())
        );
        assert_eq!(
            sys(
                &mut b,
                &["show", "-p", "LoadState", "-p", "ActiveState", "nope"]
            ),
            (
                0,
                "LoadState=not-found\nActiveState=inactive\n".into(),
                String::new()
            )
        );
        let (_, out, _) = sys(&mut b, &["show", "idle"]);
        assert!(out.contains("Id=idle.service\n"));
        assert!(out.contains("UnitFileState=enabled\n"));
        assert!(out.contains("MainPID=0\n"));
    }

    #[test]
    fn list_units_filters() {
        let mut b = backend();
        let (rc, out, _) = sys(&mut b, &["list-units", "--type=service", "--no-legend"]);
        assert_eq!(rc, 0);
        let units: Vec<&str> = out
            .lines()
            .map(|l| l.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(
            units,
            vec!["broken.service", "dave-finalize.service", "sshd.service"]
        );
        let (_, out, _) = sys(&mut b, &["list-units", "--all", "--no-legend"]);
        assert_eq!(out.lines().count(), 4);
        let (_, out, _) = sys(&mut b, &["list-units", "--state=failed", "--no-legend"]);
        assert_eq!(out.lines().count(), 1);
        let (_, out, _) = sys(&mut b, &["list-units", "--all", "--no-legend", "d*"]);
        assert_eq!(out.lines().count(), 1);
        let (_, out, _) = sys(&mut b, &["list-units"]);
        assert!(out.starts_with("UNIT"));
        assert!(out.contains("3 loaded units listed."));
    }

    #[test]
    fn list_unit_files_shows_boot_state() {
        let mut b = backend();
        b.masked.insert("dhcpcd".into());
        b.enabled_markers.insert("sshd".into());
        let (rc, out, _) = sys(&mut b, &["list-unit-files", "--no-legend"]);
        assert_eq!(rc, 0);
        assert!(
            out.contains("sshd.service")
                && out
                    .lines()
                    .any(|l| l.starts_with("sshd.service") && l.ends_with("enabled"))
        );
        assert!(
            out.lines()
                .any(|l| l.starts_with("dhcpcd.service") && l.ends_with("masked"))
        );
    }

    #[test]
    fn daemon_reload_and_power_verbs() {
        let mut b = backend();
        assert_eq!(sys(&mut b, &["daemon-reload"]).0, 0);
        assert_eq!(b.requests.last().unwrap(), "Reload");
        assert_eq!(sys(&mut b, &["reboot"]).0, 0);
        assert_eq!(b.requests.last().unwrap(), "Shutdown { kind: Reboot }");
        assert_eq!(sys(&mut b, &["poweroff", "--no-wall"]).0, 0);
        assert_eq!(b.requests.last().unwrap(), "Shutdown { kind: Poweroff }");
        let walls = b.walls.len();
        assert_eq!(sys(&mut b, &["halt", "--no-wall"]).0, 0);
        assert_eq!(b.walls.len(), walls);
        assert_eq!(sys(&mut b, &["reboot", "-f"]).0, 1, "fake reboot(2) fails");
        assert_eq!(b.reboots, vec![(ShutdownKind::Reboot, true)]);
    }

    #[test]
    fn version_says_it_is_not_systemd() {
        let mut b = backend();
        let (rc, out, _) = sys(&mut b, &["--version"]);
        assert_eq!(rc, 0);
        assert!(out.contains("not systemd"));
        assert_eq!(sys(&mut b, &["--help"]).0, 0);
    }
}
