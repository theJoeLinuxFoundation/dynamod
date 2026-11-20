//! reboot, poweroff, halt and shutdown.
//!
//! - `reboot|poweroff|halt` ask svmgr for a clean shutdown (services stop in
//!   reverse dependency order, then dynamod-init kills what is left,
//!   unmounts and calls reboot(2)). With `-f`, root only: sync and reboot(2)
//!   right away, no services stopped.
//! - `shutdown [-r|-h|-P|-H] [TIME] [MESSAGE...]`: TIME is `now`, `+MINUTES`
//!   (or plain MINUTES) or `HH:MM` local time (the next one), default `+1`.
//!   A delayed shutdown is kept by svmgr (ScheduleShutdown) and runs the
//!   same clean path when due; `shutdown -c` cancels it, `-k` only sends the
//!   wall message.

use dynamod_common::protocol::{MessageBody, ShutdownKind};

use crate::backend::Ctx;
use crate::{complain, say};

/// When to shut down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSpec {
    Now,
    Minutes(u64),
    At { hour: u32, minute: u32 },
}

/// A parsed power command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PowerCmd {
    /// Clean shutdown now, or (force) reboot(2) now.
    Now {
        kind: ShutdownKind,
        force: bool,
        sync: bool,
        wall: bool,
        message: Option<String>,
    },
    /// Clean shutdown at a later time.
    Schedule {
        kind: ShutdownKind,
        when: TimeSpec,
        wall: bool,
        message: Option<String>,
    },
    /// Cancel a scheduled shutdown.
    Cancel {
        wall: bool,
        message: Option<String>,
    },
    /// Only send the wall message (`shutdown -k`).
    WarnOnly {
        kind: ShutdownKind,
        when: TimeSpec,
        message: Option<String>,
    },
    /// `-w`: nothing to do (dynamod keeps no wtmp).
    Nothing,
    Help(String),
}

/// Parse `now`, `+M`, `M` or `HH:MM`.
pub fn parse_time(s: &str) -> Result<TimeSpec, String> {
    let bad = || format!("invalid time '{s}' (use now, +MINUTES or HH:MM)");
    if s == "now" {
        return Ok(TimeSpec::Now);
    }
    let digits = |t: &str| !t.is_empty() && t.len() <= 9 && t.bytes().all(|b| b.is_ascii_digit());
    let minutes = s.strip_prefix('+').unwrap_or(s);
    if digits(minutes) {
        let m: u64 = minutes.parse().map_err(|_| bad())?;
        return Ok(if m == 0 {
            TimeSpec::Now
        } else {
            TimeSpec::Minutes(m)
        });
    }
    if let Some((h, m)) = s.split_once(':') {
        if digits(h) && h.len() <= 2 && digits(m) && m.len() == 2 {
            let (hour, minute): (u32, u32) =
                (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
            if hour < 24 && minute < 60 {
                return Ok(TimeSpec::At { hour, minute });
            }
        }
    }
    Err(bad())
}

/// Split "-abc" into "-a", "-b", "-c"; keep "--long" and positionals.
/// `takes_value` names the short options whose value may be glued on.
fn expand_short(args: &[String], takes_value: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    for a in args {
        if a.len() > 2 && a.starts_with('-') && !a.starts_with("--") {
            let chars: Vec<char> = a[1..].chars().collect();
            let mut i = 0;
            while i < chars.len() {
                out.push(format!("-{}", chars[i]));
                if takes_value.contains(&chars[i]) && i + 1 < chars.len() {
                    out.push(chars[i + 1..].iter().collect());
                    break;
                }
                i += 1;
            }
        } else {
            out.push(a.clone());
        }
    }
    out
}

fn kind_name(kind: ShutdownKind) -> &'static str {
    match kind {
        ShutdownKind::Reboot => "reboot",
        ShutdownKind::Poweroff => "poweroff",
        ShutdownKind::Halt => "halt",
    }
}

pub fn power_usage(prog: &str) -> String {
    format!(
        "Usage: {prog} [OPTIONS]\n\n\
         Ask dynamod for a clean {prog} (services are stopped first).\n\n\
         Options:\n  \
         -f, --force       {prog} immediately (sync, reboot(2)); root only\n  \
         -n, --no-sync     with --force, do not sync first\n  \
         -p, --poweroff    power off (also: --reboot, --halt)\n  \
         -w, --wtmp-only   do nothing (dynamod keeps no wtmp)\n  \
         -d, --no-wtmp     accepted, ignored\n      \
         --no-wall     do not send a wall message\n  \
         -h, --help        show this help\n"
    )
}

/// Parse the arguments of `reboot`, `poweroff` or `halt` (`kind` from the
/// program name).
pub fn parse_power(
    prog: &str,
    mut kind: ShutdownKind,
    args: &[String],
) -> Result<PowerCmd, String> {
    let (mut force, mut sync, mut wall, mut wtmp_only) = (false, true, true, false);
    let mut message = None;
    for a in expand_short(args, &[]) {
        match a.as_str() {
            "-f" | "--force" => force = true,
            "-n" | "--no-sync" => sync = false,
            "-w" | "--wtmp-only" => wtmp_only = true,
            "-d" | "--no-wtmp" | "-i" | "--no-block" | "-q" | "--quiet" => {}
            "--no-wall" => wall = false,
            "-p" | "--poweroff" => kind = ShutdownKind::Poweroff,
            "--reboot" => kind = ShutdownKind::Reboot,
            "--halt" => kind = ShutdownKind::Halt,
            "-h" | "--help" => return Ok(PowerCmd::Help(power_usage(prog))),
            s if s.starts_with("--message=") => message = Some(s["--message=".len()..].to_string()),
            s if s.starts_with('-') => return Err(format!("{prog}: unknown option '{s}'")),
            s => return Err(format!("{prog}: unexpected argument '{s}'")),
        }
    }
    if wtmp_only {
        return Ok(PowerCmd::Nothing);
    }
    Ok(PowerCmd::Now {
        kind,
        force,
        sync,
        wall,
        message,
    })
}

pub fn shutdown_usage() -> String {
    "Usage: shutdown [OPTIONS] [TIME] [MESSAGE...]\n\n\
     Shut the system down cleanly through dynamod, now or at TIME.\n\n\
     TIME: now, +MINUTES (or MINUTES), HH:MM (local, the next one); default +1\n\n\
     Options:\n  \
     -r, --reboot     reboot\n  \
     -P, --poweroff   power off (default)\n  \
     -h               power off (unless -H)\n  \
     -H, --halt       halt\n  \
     -k               only send the wall message\n  \
     -c               cancel a scheduled shutdown\n      \
     --no-wall    do not send wall messages\n  \
     -f, -F, -t SEC   accepted, ignored (sysvinit fsck/delay options)\n      \
     --help       show this help\n"
        .to_string()
}

/// Parse the arguments of `shutdown`.
pub fn parse_shutdown(args: &[String]) -> Result<PowerCmd, String> {
    let mut kind = ShutdownKind::Poweroff;
    let mut halt_set = false;
    let (mut warn_only, mut cancel, mut wall) = (false, false, true);
    let mut positionals: Vec<String> = Vec::new();

    // Options may come anywhere (as with getopt); after "--" all is positional.
    let args = expand_short(args, &['t']);
    let mut it = args.into_iter();
    let mut only_positionals = false;
    while let Some(a) = it.next() {
        if only_positionals {
            positionals.push(a);
            continue;
        }
        match a.as_str() {
            "-r" | "--reboot" => kind = ShutdownKind::Reboot,
            "-P" | "--poweroff" => kind = ShutdownKind::Poweroff,
            "-h" => {
                if !halt_set {
                    kind = ShutdownKind::Poweroff;
                }
            }
            "-H" | "--halt" => {
                kind = ShutdownKind::Halt;
                halt_set = true;
            }
            "-k" => warn_only = true,
            "-c" => cancel = true,
            "--no-wall" => wall = false,
            "-f" | "-F" | "-q" | "--no-block" => {}
            "-t" => {
                it.next().ok_or("shutdown: -t needs a number of seconds")?;
            }
            "--help" => return Ok(PowerCmd::Help(shutdown_usage())),
            "--" => only_positionals = true,
            s if s.starts_with('-')
                && s.len() > 1
                && !s[1..].bytes().all(|b| b.is_ascii_digit()) =>
            {
                return Err(format!("shutdown: unknown option '{s}'"));
            }
            _ => positionals.push(a),
        }
    }

    let join = |v: &[String]| {
        let m = v.join(" ");
        if m.is_empty() { None } else { Some(m) }
    };

    if cancel {
        return Ok(PowerCmd::Cancel {
            wall,
            message: join(&positionals),
        });
    }
    let (when, message) = match positionals.split_first() {
        Some((t, rest)) => (
            parse_time(t).map_err(|e| format!("shutdown: {e}"))?,
            join(rest),
        ),
        None => (TimeSpec::Minutes(1), None),
    };
    if warn_only {
        return Ok(PowerCmd::WarnOnly {
            kind,
            when,
            message,
        });
    }
    Ok(match when {
        TimeSpec::Now => PowerCmd::Now {
            kind,
            force: false,
            sync: true,
            wall,
            message,
        },
        when => PowerCmd::Schedule {
            kind,
            when,
            wall,
            message,
        },
    })
}

fn going_down(kind: ShutdownKind) -> &'static str {
    match kind {
        ShutdownKind::Reboot => "reboot",
        ShutdownKind::Poweroff => "power off",
        ShutdownKind::Halt => "halt",
    }
}

fn with_message(text: String, message: &Option<String>) -> String {
    match message {
        Some(m) => format!("{m}\n\n{text}"),
        None => text,
    }
}

/// Resolve TIME to a wall-clock time.
fn resolve(ctx: &Ctx, when: TimeSpec) -> u64 {
    let now = ctx.backend.now();
    match when {
        TimeSpec::Now => now,
        TimeSpec::Minutes(m) => now + m * 60,
        TimeSpec::At { hour, minute } => ctx.backend.next_local_time(now, hour, minute),
    }
}

fn ack(ctx: &mut Ctx, prog: &str, result: Result<MessageBody, String>) -> i32 {
    match result {
        Ok(MessageBody::Ack) => 0,
        Ok(MessageBody::Error { message }) => {
            complain!(ctx, "{prog}: {message}");
            1
        }
        Ok(_) => {
            complain!(ctx, "{prog}: unexpected response from dynamod");
            1
        }
        Err(e) => {
            complain!(ctx, "{prog}: {e}");
            1
        }
    }
}

/// Run a parsed power command. Returns the exit code.
pub fn run(ctx: &mut Ctx, prog: &str, cmd: PowerCmd) -> i32 {
    match cmd {
        PowerCmd::Help(text) => {
            let _ = write!(ctx.out, "{text}");
            0
        }
        PowerCmd::Nothing => 0,
        PowerCmd::Now {
            kind,
            force: true,
            sync,
            ..
        } => {
            if !ctx.backend.is_root() {
                complain!(ctx, "{prog}: --force needs root");
                return 1;
            }
            let err = ctx.backend.reboot_now(kind, sync);
            complain!(ctx, "{prog}: reboot(2) failed: {err}");
            1
        }
        PowerCmd::Now {
            kind,
            force: false,
            wall,
            message,
            ..
        } => {
            if wall {
                let text = format!("The system is going down for {} NOW!", kind_name(kind));
                ctx.backend.wall(&with_message(text, &message));
            }
            let result = ctx.backend.request(MessageBody::Shutdown { kind });
            ack(ctx, prog, result)
        }
        PowerCmd::Schedule {
            kind,
            when,
            wall,
            message,
        } => {
            let at = resolve(ctx, when);
            let result = ctx
                .backend
                .request(MessageBody::ScheduleShutdown { kind, at });
            let rc = ack(ctx, prog, result);
            if rc == 0 {
                let local = ctx.backend.format_local_time(at);
                if wall {
                    let text = format!("The system will {} at {local}!", going_down(kind));
                    ctx.backend.wall(&with_message(text, &message));
                }
                say!(
                    ctx,
                    "{} scheduled for {local}, use 'shutdown -c' to cancel.",
                    kind_name(kind)
                );
            }
            rc
        }
        PowerCmd::Cancel { wall, message } => {
            let result = ctx.backend.request(MessageBody::CancelShutdown);
            let rc = ack(ctx, prog, result);
            if rc == 0 && wall {
                let text = "The system shutdown has been cancelled.".to_string();
                ctx.backend.wall(&with_message(text, &message));
            }
            rc
        }
        PowerCmd::WarnOnly {
            kind,
            when,
            message,
        } => {
            let text = match when {
                TimeSpec::Now => format!("The system is going down for {} NOW!", kind_name(kind)),
                when => {
                    let at = resolve(ctx, when);
                    format!(
                        "The system will {} at {}!",
                        going_down(kind),
                        ctx.backend.format_local_time(at)
                    )
                }
            };
            ctx.backend.wall(&with_message(text, &message));
            0
        }
    }
}

/// Entry point for `reboot`, `poweroff`, `halt` (program-name kind).
pub fn main_power(ctx: &mut Ctx, prog: &str, kind: ShutdownKind, args: &[String]) -> i32 {
    match parse_power(prog, kind, args) {
        Ok(cmd) => run(ctx, prog, cmd),
        Err(e) => {
            complain!(ctx, "{e}");
            1
        }
    }
}

/// Entry point for `shutdown`.
pub fn main_shutdown(ctx: &mut Ctx, args: &[String]) -> i32 {
    match parse_shutdown(args) {
        Ok(cmd) => run(ctx, "shutdown", cmd),
        Err(e) => {
            complain!(ctx, "{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, run as run_fake};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn time_parsing() {
        assert_eq!(parse_time("now"), Ok(TimeSpec::Now));
        assert_eq!(parse_time("+0"), Ok(TimeSpec::Now));
        assert_eq!(parse_time("0"), Ok(TimeSpec::Now));
        assert_eq!(parse_time("+5"), Ok(TimeSpec::Minutes(5)));
        assert_eq!(parse_time("10"), Ok(TimeSpec::Minutes(10)));
        assert_eq!(
            parse_time("23:59"),
            Ok(TimeSpec::At {
                hour: 23,
                minute: 59
            })
        );
        assert_eq!(parse_time("7:05"), Ok(TimeSpec::At { hour: 7, minute: 5 }));
        assert_eq!(parse_time("00:00"), Ok(TimeSpec::At { hour: 0, minute: 0 }));
        for bad in [
            "24:00", "12:60", "12:5", "+", "+x", "tomorrow", "-5", "1:2:3", "", "123:00",
        ] {
            assert!(parse_time(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn shutdown_argument_mapping() {
        let reboot = ShutdownKind::Reboot;
        let poweroff = ShutdownKind::Poweroff;
        assert_eq!(
            parse_shutdown(&s(&["-r", "now"])),
            Ok(PowerCmd::Now {
                kind: reboot,
                force: false,
                sync: true,
                wall: true,
                message: None
            })
        );
        assert_eq!(
            parse_shutdown(&s(&["-h", "now"])),
            Ok(PowerCmd::Now {
                kind: poweroff,
                force: false,
                sync: true,
                wall: true,
                message: None
            })
        );
        assert_eq!(
            parse_shutdown(&s(&["-H", "-h", "now"])),
            Ok(PowerCmd::Now {
                kind: ShutdownKind::Halt,
                force: false,
                sync: true,
                wall: true,
                message: None
            })
        );
        assert_eq!(
            parse_shutdown(&[]),
            Ok(PowerCmd::Schedule {
                kind: poweroff,
                when: TimeSpec::Minutes(1),
                wall: true,
                message: None
            })
        );
        assert_eq!(
            parse_shutdown(&s(&["-r", "+5", "kernel", "upgrade"])),
            Ok(PowerCmd::Schedule {
                kind: reboot,
                when: TimeSpec::Minutes(5),
                wall: true,
                message: Some("kernel upgrade".into())
            })
        );
        assert_eq!(
            parse_shutdown(&s(&["-rf", "23:30"])),
            Ok(PowerCmd::Schedule {
                kind: reboot,
                when: TimeSpec::At {
                    hour: 23,
                    minute: 30
                },
                wall: true,
                message: None
            })
        );
        assert_eq!(
            parse_shutdown(&s(&["-c", "never", "mind"])),
            Ok(PowerCmd::Cancel {
                wall: true,
                message: Some("never mind".into())
            })
        );
        assert_eq!(
            parse_shutdown(&s(&["-k", "+2"])),
            Ok(PowerCmd::WarnOnly {
                kind: poweroff,
                when: TimeSpec::Minutes(2),
                message: None
            })
        );
        assert_eq!(
            parse_shutdown(&s(&["-t", "5", "-r", "now"])),
            Ok(PowerCmd::Now {
                kind: reboot,
                force: false,
                sync: true,
                wall: true,
                message: None
            })
        );
        // Options may follow TIME, as with getopt; "--" ends them.
        assert_eq!(
            parse_shutdown(&s(&["+5", "-r", "going", "down"])),
            Ok(PowerCmd::Schedule {
                kind: reboot,
                when: TimeSpec::Minutes(5),
                wall: true,
                message: Some("going down".into())
            })
        );
        assert_eq!(
            parse_shutdown(&s(&["-h", "now", "--", "-r", "is", "text"])),
            Ok(PowerCmd::Now {
                kind: poweroff,
                force: false,
                sync: true,
                wall: true,
                message: Some("-r is text".into())
            })
        );
        assert!(parse_shutdown(&s(&["-r", "soon"])).is_err());
        assert!(parse_shutdown(&s(&["--frobnicate"])).is_err());
        assert!(matches!(
            parse_shutdown(&s(&["--help"])),
            Ok(PowerCmd::Help(_))
        ));
    }

    #[test]
    fn power_argument_mapping() {
        let p = |args: &[&str]| parse_power("reboot", ShutdownKind::Reboot, &s(args));
        assert_eq!(
            p(&[]),
            Ok(PowerCmd::Now {
                kind: ShutdownKind::Reboot,
                force: false,
                sync: true,
                wall: true,
                message: None
            })
        );
        assert_eq!(
            p(&["-f"]),
            Ok(PowerCmd::Now {
                kind: ShutdownKind::Reboot,
                force: true,
                sync: true,
                wall: true,
                message: None
            })
        );
        assert_eq!(
            p(&["-fn"]),
            Ok(PowerCmd::Now {
                kind: ShutdownKind::Reboot,
                force: true,
                sync: false,
                wall: true,
                message: None
            })
        );
        assert_eq!(
            p(&["--force", "-p"]),
            Ok(PowerCmd::Now {
                kind: ShutdownKind::Poweroff,
                force: true,
                sync: true,
                wall: true,
                message: None
            })
        );
        assert_eq!(p(&["-w"]), Ok(PowerCmd::Nothing));
        assert!(p(&["-x"]).is_err());
        assert!(p(&["recovery"]).is_err());
        let h = parse_power("halt", ShutdownKind::Halt, &s(&["--no-wall"]));
        assert_eq!(
            h,
            Ok(PowerCmd::Now {
                kind: ShutdownKind::Halt,
                force: false,
                sync: true,
                wall: false,
                message: None
            })
        );
    }

    #[test]
    fn reboot_asks_svmgr_and_walls() {
        let mut b = FakeBackend::with(&[]);
        let (rc, _, _) = run_fake(&mut b, |c| {
            main_power(c, "reboot", ShutdownKind::Reboot, &[])
        });
        assert_eq!(rc, 0);
        assert_eq!(b.requests, vec!["Shutdown { kind: Reboot }"]);
        assert_eq!(b.walls.len(), 1);
        assert!(b.reboots.is_empty());
    }

    #[test]
    fn reboot_force_needs_root_and_calls_reboot() {
        let mut b = FakeBackend::with(&[]);
        b.root = false;
        let (rc, _, err) = run_fake(&mut b, |c| {
            main_power(c, "reboot", ShutdownKind::Reboot, &s(&["-f"]))
        });
        assert_eq!(rc, 1);
        assert!(err.contains("needs root"));
        assert!(b.reboots.is_empty());

        b.root = true;
        let (rc, _, _) = run_fake(&mut b, |c| {
            main_power(c, "poweroff", ShutdownKind::Poweroff, &s(&["-f", "-n"]))
        });
        assert_eq!(rc, 1, "the fake's reboot(2) fails, so we return");
        assert_eq!(b.reboots, vec![(ShutdownKind::Poweroff, false)]);
        assert!(b.requests.is_empty(), "no clean shutdown with -f");
    }

    #[test]
    fn shutdown_schedules_in_svmgr() {
        let mut b = FakeBackend::with(&[]);
        let (rc, out, _) = run_fake(&mut b, |c| main_shutdown(c, &s(&["-r", "+5"])));
        assert_eq!(rc, 0);
        assert_eq!(
            b.requests,
            vec![format!(
                "ScheduleShutdown {{ kind: Reboot, at: {} }}",
                1_000_000 + 300
            )]
        );
        assert!(out.contains("scheduled"));

        // HH:MM: the fake's local time is UTC; 1_000_000 is 13:46:40.
        b.requests.clear();
        let (rc, _, _) = run_fake(&mut b, |c| main_shutdown(c, &s(&["12:00"])));
        assert_eq!(rc, 0);
        let tomorrow_noon = 1_000_000 - 1_000_000 % 86_400 + 86_400 + 12 * 3600;
        assert_eq!(
            b.requests,
            vec![format!(
                "ScheduleShutdown {{ kind: Poweroff, at: {tomorrow_noon} }}"
            )]
        );

        b.requests.clear();
        let (rc, _, _) = run_fake(&mut b, |c| main_shutdown(c, &s(&["-c"])));
        assert_eq!(rc, 0);
        assert_eq!(b.requests, vec!["CancelShutdown"]);

        b.requests.clear();
        let walls = b.walls.len();
        let (rc, _, _) = run_fake(&mut b, |c| main_shutdown(c, &s(&["-k", "now", "test"])));
        assert_eq!(rc, 0);
        assert!(b.requests.is_empty(), "-k sends nothing to svmgr");
        assert_eq!(b.walls.len(), walls + 1);

        b.svmgr_down = true;
        let (rc, _, err) = run_fake(&mut b, |c| main_shutdown(c, &s(&["-h", "now"])));
        assert_eq!(rc, 1);
        assert!(err.contains("cannot connect"));
    }
}
