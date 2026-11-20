//! Configuration files in `X.d` directories, as tmpfiles.d(5) and
//! sysusers.d(5) describe them.
//!
//! The directories are, from highest to lowest priority, `/etc/X.d`,
//! `/run/X.d`, `/usr/local/lib/X.d` and `/usr/lib/X.d`. Of several files
//! with the same name only the one in the highest-priority directory is
//! used; if it is a symlink to `/dev/null` (or empty) the name is masked.
//! The files are then read in lexical order of their names, whatever their
//! directories.
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::rootfs::Root;

pub const SEARCH_DIRS: [&str; 4] = ["/etc", "/run", "/usr/local/lib", "/usr/lib"];

/// The search directories for `subdir` (such as `"tmpfiles.d"`), highest
/// priority first, as paths inside the root.
pub fn dirs(subdir: &str) -> Vec<String> {
    SEARCH_DIRS
        .iter()
        .map(|d| format!("{d}/{subdir}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfFile {
    /// The file name, such as `openssh.conf`.
    pub name: String,
    /// Its path inside the root, for messages.
    pub path: String,
    /// Where to read it on the host (`None` for standard input).
    pub host: Option<PathBuf>,
    /// A symlink to `/dev/null` or an empty file: nothing to read.
    pub masked: bool,
}

impl ConfFile {
    /// Standard input, for the file argument `-`.
    pub fn stdin() -> ConfFile {
        ConfFile {
            name: "-".into(),
            path: "<stdin>".into(),
            host: None,
            masked: false,
        }
    }

    /// The file's contents (empty when masked).
    pub fn read(&self) -> io::Result<String> {
        if self.masked {
            return Ok(String::new());
        }
        let mut buf = Vec::new();
        match &self.host {
            None => {
                io::stdin().read_to_end(&mut buf)?;
            }
            Some(p) => {
                buf = std::fs::read(p)?;
            }
        }
        String::from_utf8(buf)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not valid UTF-8"))
    }
}

/// Classify the entry `name` of the search directory `dir` (a path inside
/// the root). `None` if it is not a configuration file (a directory, a
/// dangling symlink, ...).
fn classify(root: &Root, dir: &str, name: &str) -> Option<ConfFile> {
    let path = format!("{dir}/{name}");
    let lhost = root.resolve(&path, false).ok()?;
    let lmd = std::fs::symlink_metadata(&lhost).ok()?;
    if lmd.file_type().is_symlink()
        && let Ok(target) = std::fs::read_link(&lhost)
        && target == Path::new("/dev/null")
    {
        return Some(ConfFile {
            name: name.into(),
            path,
            host: Some(lhost),
            masked: true,
        });
    }
    let host = root.resolve(&path, true).ok()?;
    let md = std::fs::metadata(&host).ok()?;
    use std::os::unix::fs::FileTypeExt;
    if md.file_type().is_char_device() {
        return Some(ConfFile {
            name: name.into(),
            path,
            host: Some(host),
            masked: true,
        });
    }
    if !md.is_file() {
        return None;
    }
    Some(ConfFile {
        name: name.into(),
        path,
        host: Some(host),
        masked: md.len() == 0,
    })
}

fn wanted(name: &str) -> bool {
    name.ends_with(".conf") && !name.starts_with('.')
}

/// All configuration files for `subdir`, one per name (the one of highest
/// priority), sorted by name. Masked names are included, flagged.
pub fn list(root: &Root, subdir: &str) -> Vec<ConfFile> {
    let mut found: Vec<ConfFile> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs(subdir) {
        let Ok(host_dir) = root.resolve(&dir, true) else {
            continue;
        };
        let Ok(rd) = std::fs::read_dir(&host_dir) else {
            continue;
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| wanted(n))
            .collect();
        names.sort();
        for name in names {
            if seen.contains(&name) {
                continue;
            }
            if let Some(cf) = classify(root, &dir, &name) {
                seen.insert(name);
                found.push(cf);
            }
        }
    }
    found.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    found
}

/// The file `name` of highest priority in the search directories.
pub fn find(root: &Root, subdir: &str, name: &str) -> Option<ConfFile> {
    dirs(subdir)
        .iter()
        .find_map(|dir| classify(root, dir, name))
}

/// A file named on the command line: `-` is standard input, a name without
/// a slash is looked up in the search directories, anything else is a path
/// used as it is (not prefixed with the root, as systemd does).
pub fn from_argument(root: &Root, subdir: &str, arg: &str) -> io::Result<ConfFile> {
    if arg == "-" {
        return Ok(ConfFile::stdin());
    }
    if !arg.contains('/') {
        return find(root, subdir, arg).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{arg}: not found in any {subdir} directory"),
            )
        });
    }
    let p = PathBuf::from(arg);
    let md = std::fs::metadata(&p).map_err(|e| io::Error::new(e.kind(), format!("{arg}: {e}")))?;
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(ConfFile {
        name,
        path: arg.to_string(),
        masked: md.len() == 0 && md.is_file(),
        host: Some(p),
    })
}

/// Print files as `--cat-config` does (`tldr`: only the lines that matter).
pub fn cat(files: &[ConfFile], tldr: bool, out: &mut impl io::Write) -> io::Result<()> {
    let mut first = true;
    for f in files {
        if !first {
            writeln!(out)?;
        }
        first = false;
        if f.masked {
            writeln!(out, "# {} (masked)", f.path)?;
            continue;
        }
        writeln!(out, "# {}", f.path)?;
        let text = f.read()?;
        for line in text.lines() {
            let t = line.trim();
            if tldr && (t.is_empty() || t.starts_with('#')) {
                continue;
            }
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn precedence_masking_and_order() {
        let d = std::env::temp_dir().join(format!("sdconf-confdirs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        for sub in ["etc/x.d", "run/x.d", "usr/local/lib/x.d", "usr/lib/x.d"] {
            fs::create_dir_all(d.join(sub)).unwrap();
        }
        fs::write(d.join("usr/lib/x.d/b.conf"), "vendor b\n").unwrap();
        fs::write(d.join("etc/x.d/b.conf"), "admin b\n").unwrap();
        fs::write(d.join("usr/lib/x.d/a.conf"), "vendor a\n").unwrap();
        fs::write(d.join("run/x.d/a.conf"), "runtime a\n").unwrap();
        fs::write(d.join("usr/local/lib/x.d/c.conf"), "local c\n").unwrap();
        fs::write(d.join("usr/lib/x.d/c.conf"), "vendor c\n").unwrap();
        fs::write(d.join("usr/lib/x.d/masked.conf"), "vendor m\n").unwrap();
        symlink("/dev/null", d.join("etc/x.d/masked.conf")).unwrap();
        fs::write(d.join("usr/lib/x.d/notes.txt"), "ignored\n").unwrap();
        fs::write(d.join("usr/lib/x.d/.hidden.conf"), "ignored\n").unwrap();
        fs::create_dir(d.join("etc/x.d/dir.conf")).unwrap();
        fs::write(d.join("usr/lib/x.d/dir.conf"), "vendor dir\n").unwrap();
        // An absolute symlink is followed inside the root.
        fs::create_dir_all(d.join("usr/share/x")).unwrap();
        fs::write(d.join("usr/share/x/linked"), "linked\n").unwrap();
        symlink("/usr/share/x/linked", d.join("usr/lib/x.d/z.conf")).unwrap();

        let root = Root::new(Some(&d)).unwrap();
        let files = list(&root, "x.d");
        let names: Vec<_> = files
            .iter()
            .map(|f| (f.name.as_str(), f.path.as_str(), f.masked))
            .collect();
        assert_eq!(
            names,
            [
                ("a.conf", "/run/x.d/a.conf", false),
                ("b.conf", "/etc/x.d/b.conf", false),
                ("c.conf", "/usr/local/lib/x.d/c.conf", false),
                ("dir.conf", "/usr/lib/x.d/dir.conf", false),
                ("masked.conf", "/etc/x.d/masked.conf", true),
                ("z.conf", "/usr/lib/x.d/z.conf", false),
            ]
        );
        assert_eq!(files[1].read().unwrap(), "admin b\n");
        assert_eq!(files[4].read().unwrap(), "");
        assert_eq!(files[5].read().unwrap(), "linked\n");
        assert_eq!(
            find(&root, "x.d", "c.conf").unwrap().path,
            "/usr/local/lib/x.d/c.conf"
        );
        assert_eq!(
            from_argument(&root, "x.d", "a.conf").unwrap().path,
            "/run/x.d/a.conf"
        );
        assert!(from_argument(&root, "x.d", "nope.conf").is_err());

        let mut out = Vec::new();
        cat(&files[..2], false, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "# /run/x.d/a.conf\nruntime a\n\n# /etc/x.d/b.conf\nadmin b\n"
        );
        fs::remove_dir_all(&d).unwrap();
    }
}
