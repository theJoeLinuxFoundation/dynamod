/// dynamodctl: CLI tool for controlling the dynamod init system.
///
/// Communicates with dynamod-svmgr over the control socket at
/// /run/dynamod/control.sock.
///
/// Also a multi-call binary: installed (symlinked) as `reboot`, `poweroff`,
/// `halt`, `shutdown` or `systemctl`, it behaves as that command.
mod backend;
mod client;
mod commands;
mod ops;
mod power;
mod systemctl;
mod unitfiles;
mod wall;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use dynamod_common::protocol::ShutdownKind;

use backend::{Ctx, RealBackend};

#[derive(Parser)]
#[command(name = "dynamodctl", about = "Control the dynamod init system")]
struct Cli {
    /// Path to the control socket [default: /run/dynamod/control.sock]
    #[arg(long)]
    socket: Option<String>,

    /// Alternative root directory for definitions, enable/disable markers
    /// and the control socket (also $DYNAMOD_ROOT)
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a service
    Start {
        /// Service name
        name: String,
    },
    /// Stop a service
    Stop {
        /// Service name
        name: String,
    },
    /// Restart a service
    Restart {
        /// Service name
        name: String,
    },
    /// Show status of a service
    Status {
        /// Service name
        name: String,
    },
    /// List all services
    List,
    /// Show the supervisor tree
    Tree,
    /// Reload service definitions (/usr/lib/dynamod and /etc/dynamod)
    Reload,
    /// Start services at boot (creates /etc/dynamod/enabled/NAME)
    Enable {
        /// Service names
        #[arg(required = true)]
        names: Vec<String>,
        /// Also start them now
        #[arg(long)]
        now: bool,
    },
    /// Do not start services at boot (creates /etc/dynamod/disabled/NAME)
    Disable {
        /// Service names
        #[arg(required = true)]
        names: Vec<String>,
        /// Also stop them now
        #[arg(long)]
        now: bool,
    },
    /// Exit 0 if the service starts at boot, 1 if not
    IsEnabled {
        /// Service name
        name: String,
        /// Print nothing
        #[arg(short, long)]
        quiet: bool,
    },
    /// Shut down the system: `shutdown [poweroff|reboot|halt]` now, or the
    /// shutdown command: `shutdown [-r|-h|-P|-H|-k|-c] [TIME] [MESSAGE...]`
    #[command(disable_help_flag = true)]
    Shutdown {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Reboot cleanly (-f: immediately, root only)
    #[command(disable_help_flag = true)]
    Reboot {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Power off cleanly (-f: immediately, root only)
    #[command(disable_help_flag = true)]
    Poweroff {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Halt cleanly (-f: immediately, root only)
    #[command(disable_help_flag = true)]
    Halt {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The systemctl compatibility front end
    #[command(disable_help_flag = true)]
    Systemctl {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Run one of the multi-call commands with the real backend.
fn multicall(name: &str, args: &[String], backend: &mut RealBackend) -> Option<i32> {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let mut ctx = Ctx {
        backend,
        out: &mut out,
        err: &mut err,
    };
    Some(match name {
        "reboot" => power::main_power(&mut ctx, name, ShutdownKind::Reboot, args),
        "poweroff" => power::main_power(&mut ctx, name, ShutdownKind::Poweroff, args),
        "halt" => power::main_power(&mut ctx, name, ShutdownKind::Halt, args),
        "shutdown" => power::main_shutdown(&mut ctx, args),
        "systemctl" => systemctl::main(&mut ctx, args),
        _ => return None,
    })
}

fn main() {
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let prog = argv
        .first()
        .and_then(|a| Path::new(a).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("dynamodctl")
        .to_string();

    // Invoked as reboot, poweroff, halt, shutdown or systemctl
    let mut backend = RealBackend::new(None, None);
    if let Some(code) = multicall(&prog, argv.get(1..).unwrap_or(&[]), &mut backend) {
        std::process::exit(code);
    }

    let cli = Cli::parse();
    let mut backend = RealBackend::new(cli.socket.clone(), cli.root.clone());
    let socket = backend.socket();
    let socket_path = &socket;

    let mut out = std::io::stdout();
    let mut err = std::io::stderr();

    let result = match cli.command {
        Command::Start { name } => commands::start(socket_path, &name),
        Command::Stop { name } => commands::stop(socket_path, &name),
        Command::Restart { name } => commands::restart(socket_path, &name),
        Command::Status { name } => commands::status(socket_path, &name),
        Command::List => commands::list(socket_path),
        Command::Tree => commands::tree(socket_path),
        Command::Reload => commands::reload(socket_path),
        Command::Enable { names, now } => {
            let mut ctx = Ctx {
                backend: &mut backend,
                out: &mut out,
                err: &mut err,
            };
            std::process::exit(ops::set_enabled(&mut ctx, &names, true, now, false));
        }
        Command::Disable { names, now } => {
            let mut ctx = Ctx {
                backend: &mut backend,
                out: &mut out,
                err: &mut err,
            };
            std::process::exit(ops::set_enabled(&mut ctx, &names, false, now, false));
        }
        Command::IsEnabled { name, quiet } => {
            let mut ctx = Ctx {
                backend: &mut backend,
                out: &mut out,
                err: &mut err,
            };
            std::process::exit(ops::is_enabled(&mut ctx, &[name], quiet));
        }
        Command::Shutdown { args } => match args.as_slice() {
            // The original form: immediate shutdown of the given kind
            [] => commands::shutdown(socket_path, "poweroff"),
            [kind] if matches!(kind.as_str(), "poweroff" | "off" | "reboot" | "halt") => {
                commands::shutdown(socket_path, kind)
            }
            _ => std::process::exit(multicall("shutdown", &args, &mut backend).unwrap_or(1)),
        },
        Command::Reboot { args } => {
            std::process::exit(multicall("reboot", &args, &mut backend).unwrap_or(1))
        }
        Command::Poweroff { args } => {
            std::process::exit(multicall("poweroff", &args, &mut backend).unwrap_or(1))
        }
        Command::Halt { args } => {
            std::process::exit(multicall("halt", &args, &mut backend).unwrap_or(1))
        }
        Command::Systemctl { args } => {
            std::process::exit(multicall("systemctl", &args, &mut backend).unwrap_or(1))
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
