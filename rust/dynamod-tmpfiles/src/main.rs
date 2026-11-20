/// dynamod-tmpfiles: creates, removes and cleans files and directories as
/// tmpfiles.d(5) configuration describes, compatible with systemd-tmpfiles
/// (and installed under that name too).
///
/// Run at boot with `--create --remove --boot`, and by the package manager
/// after packages change their tmpfiles.d files.
mod config;
mod exec;
mod glob;
mod item;
mod parse;

use std::path::PathBuf;

use dynamod_sdconf::accounts::IdDb;
use dynamod_sdconf::confdirs::{self, ConfFile};
use dynamod_sdconf::specifier::Specifiers;
use dynamod_sdconf::{Root, log_error};

use crate::config::Config;
use crate::exec::{Executor, OP_CLEAN, OP_CREATE, OP_PURGE, OP_REMOVE};
use crate::parse::ParseCtx;

const EX_DATAERR: i32 = 65;
const EX_CANTCREAT: i32 = 73;

#[derive(Debug, Default, PartialEq)]
struct Opts {
    ops: u8,
    boot: bool,
    graceful: bool,
    dry_run: bool,
    /// `--cat-config` (`Some(false)`) or `--tldr` (`Some(true)`).
    cat: Option<bool>,
    inline: bool,
    root: Option<PathBuf>,
    include: Vec<String>,
    exclude: Vec<String>,
    files: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum Action {
    Run(Opts),
    Help,
    Version,
}

fn usage(prog: &str) -> String {
    format!(
        "{prog} [OPTIONS...] [CONFIGURATION FILE...]

Create, delete, and clean up files and directories (tmpfiles.d(5)).

Commands:
     --create               Create and adjust files and directories
     --clean                Clean up files and directories older than their age
     --remove               Remove files and directories marked for removal
     --purge                Delete files and directories marked with '$' (needs files)
     --cat-config           Show the configuration files
     --tldr                 Show the non-comment parts of the configuration files
  -h --help                 Show this help
     --version              Show the version

Options:
     --boot                 Also execute lines marked with '!'
     --graceful             Quietly ignore lines with unknown users or groups
  -n --dry-run              Only print what would be done
     --inline               Treat the arguments as configuration lines
     --root=PATH            Operate on an alternate file system root
     --prefix=PATH          Only apply rules with this path prefix
     --exclude-prefix=PATH  Ignore rules with this path prefix
  -E                        Ignore rules below /dev, /proc, /run and /sys
     --no-pager             Accepted and ignored

Without file arguments, *.conf files are read from /etc/tmpfiles.d,
/run/tmpfiles.d, /usr/local/lib/tmpfiles.d and /usr/lib/tmpfiles.d.
"
    )
}

fn parse_args(args: &[String]) -> Result<Action, String> {
    let mut o = Opts::default();
    let mut i = 0;
    let mut only_files = false;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if only_files || !a.starts_with('-') || a == "-" {
            o.files.push(a.clone());
            continue;
        }
        if a == "--" {
            only_files = true;
            continue;
        }
        let (name, inline_val) = match a.split_once('=') {
            Some((n, v)) if a.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            if let Some(v) = &inline_val {
                return Ok(v.clone());
            }
            let v = args
                .get(i)
                .cloned()
                .ok_or_else(|| format!("option '{what}' requires an argument"))?;
            i += 1;
            Ok(v)
        };
        match name.as_str() {
            "--create" => o.ops |= OP_CREATE,
            "--clean" => o.ops |= OP_CLEAN,
            "--remove" => o.ops |= OP_REMOVE,
            "--purge" => o.ops |= OP_PURGE,
            "--boot" => o.boot = true,
            "--graceful" => o.graceful = true,
            "--dry-run" => o.dry_run = true,
            "--inline" => o.inline = true,
            "--cat-config" => o.cat = Some(false),
            "--tldr" => o.cat = Some(true),
            "--no-pager" => {}
            "--root" => o.root = Some(PathBuf::from(value("--root")?)),
            "--prefix" => o.include.push(value("--prefix")?),
            "--exclude-prefix" => o.exclude.push(value("--exclude-prefix")?),
            "--help" => return Ok(Action::Help),
            "--version" => return Ok(Action::Version),
            "--user" | "--image" | "--image-policy" | "--replace" => {
                return Err(format!(
                    "option '{name}' is not supported by dynamod-tmpfiles"
                ));
            }
            s if s.starts_with("--") => return Err(format!("unknown option '{s}'")),
            s => {
                for c in s[1..].chars() {
                    match c {
                        'E' => o
                            .exclude
                            .extend(["/dev", "/proc", "/run", "/sys"].map(String::from)),
                        'n' => o.dry_run = true,
                        'h' => return Ok(Action::Help),
                        _ => return Err(format!("unknown option '-{c}'")),
                    }
                }
            }
        }
    }
    Ok(Action::Run(o))
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let prog = args
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
        .unwrap_or_else(|| "dynamod-tmpfiles".into());
    dynamod_sdconf::log::init(&prog);
    std::process::exit(run(&prog, &args[1.min(args.len())..]));
}

fn run(prog: &str, args: &[String]) -> i32 {
    let o = match parse_args(args) {
        Ok(Action::Run(o)) => o,
        Ok(Action::Help) => {
            print!("{}", usage(prog));
            return 0;
        }
        Ok(Action::Version) => {
            println!(
                "dynamod-tmpfiles {} (compatible with systemd-tmpfiles)",
                env!("CARGO_PKG_VERSION")
            );
            return 0;
        }
        Err(e) => {
            log_error!("{e}");
            return 1;
        }
    };
    // Modes in the configuration are meant literally.
    // SAFETY: umask() cannot fail.
    unsafe { libc::umask(0) };

    let root = match Root::new(o.root.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            log_error!("--root={}: {e}", o.root.as_ref().unwrap().display());
            return 1;
        }
    };

    let mut unreadable = false;
    let files: Vec<ConfFile> = if o.inline {
        Vec::new()
    } else if o.files.is_empty() {
        confdirs::list(&root, "tmpfiles.d")
    } else {
        o.files
            .iter()
            .filter_map(|a| match confdirs::from_argument(&root, "tmpfiles.d", a) {
                Ok(f) => Some(f),
                Err(e) => {
                    log_error!("Failed to open {e}");
                    unreadable = true;
                    None
                }
            })
            .collect()
    };

    if let Some(tldr) = o.cat {
        let mut out = std::io::stdout().lock();
        let r = if o.inline {
            o.files
                .iter()
                .try_for_each(|l| std::io::Write::write_all(&mut out, format!("{l}\n").as_bytes()))
        } else {
            confdirs::cat(&files, tldr, &mut out)
        };
        if let Err(e) = r {
            log_error!("{e}");
            return 1;
        }
        return if unreadable { 1 } else { 0 };
    }

    if o.ops == 0 {
        log_error!("You need to specify at least one of --clean, --create, --remove, or --purge.");
        return 1;
    }
    if o.ops & OP_PURGE != 0 && o.files.is_empty() {
        log_error!("Refusing --purge without specified files. This would delete too much.");
        return 1;
    }

    let spec = Specifiers::new(&root, true);
    let ids = IdDb::load(&root);
    let ctx = ParseCtx {
        boot: o.boot,
        graceful: o.graceful,
        include_prefixes: &o.include,
        exclude_prefixes: &o.exclude,
        specifiers: &spec,
        ids: &ids,
        credentials_dir: std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from),
    };
    let mut config = Config::default();
    let mut noted = false;
    if o.inline {
        for (n, line) in o.files.iter().enumerate() {
            config.read_text(&format!("<argument {}>", n + 1), line, &ctx, &mut noted);
        }
    } else {
        for f in &files {
            config.read_file(f, &ctx, &mut noted);
        }
    }
    config.link_parents();

    let mut ex = Executor::new(&root, &config, o.dry_run);
    ex.run(o.ops);

    if config.invalid {
        EX_DATAERR
    } else if ex.failed || config.unreadable || unreadable {
        EX_CANTCREAT
    } else {
        0
    }
}

#[cfg(test)]
mod tests;
