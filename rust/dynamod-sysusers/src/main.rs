/// dynamod-sysusers: creates system users and groups as sysusers.d(5)
/// configuration declares them, compatible with systemd-sysusers (and
/// installed under that name too).
///
/// Run early at boot, and by the package manager after packages change
/// their sysusers.d files. Existing accounts are never changed; new entries
/// are added to `/etc/passwd`, `/etc/group`, `/etc/shadow` and
/// `/etc/gshadow` atomically, under `/etc/.pwd.lock`.
mod db;
mod parse;
mod plan;

use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::PathBuf;

use dynamod_sdconf::confdirs::{self, ConfFile};
use dynamod_sdconf::specifier::Specifiers;
use dynamod_sdconf::{Root, log_error, log_info, log_warning};

use crate::db::Db;
use crate::parse::Origin;
use crate::plan::Config;

#[derive(Debug, Default, PartialEq)]
struct Opts {
    root: Option<PathBuf>,
    dry_run: bool,
    /// `--cat-config` (`Some(false)`) or `--tldr` (`Some(true)`).
    cat: Option<bool>,
    inline: bool,
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

Create system users and groups (sysusers.d(5)).

  -h --help            Show this help
     --version         Show the version
     --cat-config      Show the configuration files
     --tldr            Show the non-comment parts of the configuration files
     --root=PATH       Operate on an alternate file system root
  -n --dry-run         Only print what would be done
     --inline          Treat the arguments as configuration lines
     --no-pager        Accepted and ignored

Without file arguments, *.conf files are read from /etc/sysusers.d,
/run/sysusers.d, /usr/local/lib/sysusers.d and /usr/lib/sysusers.d.
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
        match name.as_str() {
            "--root" => {
                let v = match inline_val {
                    Some(v) => v,
                    None => {
                        i += 1;
                        args.get(i - 1)
                            .cloned()
                            .ok_or("option '--root' requires an argument")?
                    }
                };
                o.root = Some(PathBuf::from(v));
            }
            "--dry-run" => o.dry_run = true,
            "--cat-config" => o.cat = Some(false),
            "--tldr" => o.cat = Some(true),
            "--inline" => o.inline = true,
            "--no-pager" => {}
            "--help" => return Ok(Action::Help),
            "--version" => return Ok(Action::Version),
            "--replace" | "--image" | "--image-policy" => {
                return Err(format!(
                    "option '{name}' is not supported by dynamod-sysusers"
                ));
            }
            s if s.starts_with("--") => return Err(format!("unknown option '{s}'")),
            s => {
                for c in s[1..].chars() {
                    match c {
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

/// The "last password change" day of new shadow entries: today, or the
/// day of `$SOURCE_DATE_EPOCH` for reproducible images.
fn lstchg(source_date_epoch: Option<&str>, now: std::time::SystemTime) -> i64 {
    let secs = source_date_epoch
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or_else(|| {
            now.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
    (secs / 86_400) as i64
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let prog = args
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
        .unwrap_or_else(|| "dynamod-sysusers".into());
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
                "dynamod-sysusers {} (compatible with systemd-sysusers)",
                env!("CARGO_PKG_VERSION")
            );
            return 0;
        }
        Err(e) => {
            log_error!("{e}");
            return 1;
        }
    };
    let root = match Root::new(o.root.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            log_error!("--root={}: {e}", o.root.as_ref().unwrap().display());
            return 1;
        }
    };

    let mut bad = false;
    let files: Vec<ConfFile> = if o.inline {
        Vec::new()
    } else if o.files.is_empty() {
        confdirs::list(&root, "sysusers.d")
    } else {
        o.files
            .iter()
            .filter_map(|a| match confdirs::from_argument(&root, "sysusers.d", a) {
                Ok(f) => Some(f),
                Err(e) => {
                    log_error!("Failed to open {e}");
                    bad = true;
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
        return if bad { 1 } else { 0 };
    }

    // Read everything first: with any invalid line nothing is changed (as
    // with systemd-sysusers), since a skipped line could otherwise let an
    // "m" line create its user with a different id.
    let spec = Specifiers::new(&root, false);
    let mut config = Config::default();
    let mut texts: Vec<(String, String)> = Vec::new();
    if o.inline {
        texts.extend(
            o.files
                .iter()
                .enumerate()
                .map(|(n, l)| (format!("<argument {}>", n + 1), l.clone())),
        );
    } else {
        for f in files.iter().filter(|f| !f.masked) {
            match f.read() {
                Ok(t) => texts.push((f.path.clone(), t)),
                Err(e) => {
                    log_error!("Failed to read {}: {e}", f.path);
                    bad = true;
                }
            }
        }
    }
    for (file, text) in &texts {
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let origin = Origin {
                file: file.clone(),
                line: n + 1,
            };
            match parse::parse_line(line, &spec) {
                Ok(mut item) => {
                    item.origin = origin;
                    if let Some(w) = config.add(item) {
                        log_warning!("{w}");
                    }
                }
                Err(e) => {
                    log_error!("{origin}: {e}");
                    bad = true;
                }
            }
        }
    }
    if bad {
        log_error!("Not changing any accounts because of the errors above.");
        return 1;
    }
    config.add_implicit();

    let _lock = if o.dry_run {
        None
    } else {
        if let Ok(etc) = root.resolve("/etc", true)
            && !etc.exists()
            && let Err(e) = std::fs::DirBuilder::new()
                .mode(0o755)
                .recursive(true)
                .create(&etc)
        {
            log_error!("Failed to create {}: {e}", etc.display());
            return 1;
        }
        match db::take_lock(&root) {
            Ok(l) => Some(l),
            Err(e) => {
                log_error!("{e}");
                return 1;
            }
        }
    };
    let db = match Db::load(&root) {
        Ok(d) => d,
        Err(e) => {
            log_error!("Failed to read the account files: {e}");
            return 1;
        }
    };
    let id_from_path = |p: &str| {
        root.resolve(p, true)
            .ok()
            .and_then(|h| std::fs::metadata(h).ok())
            .map(|m| (m.uid(), m.gid()))
    };
    let plan = plan::plan(&config, &db, &id_from_path);
    for e in &plan.errors {
        log_error!("{e}");
    }
    let day = lstchg(
        std::env::var("SOURCE_DATE_EPOCH").ok().as_deref(),
        std::time::SystemTime::now(),
    );
    let outputs = match db::render(&db, &plan, day) {
        Ok(o) => o,
        Err(e) => {
            log_error!("{e}");
            return 1;
        }
    };
    if o.dry_run {
        for (w, _) in &outputs {
            log_info!("Would write {}", root.prefix(w.path()).display());
        }
    } else if !outputs.is_empty()
        && let Err(e) = db::commit(&db, &outputs)
    {
        log_error!("{e}");
        return 1;
    }
    if plan.errors.is_empty() { 0 } else { 1 }
}

#[cfg(test)]
mod tests;
