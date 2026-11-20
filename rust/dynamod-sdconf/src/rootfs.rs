//! The root the tools operate on: `/`, or the directory given with `--root`.
//!
//! Paths in configuration are absolute paths inside that root. When the root
//! is not `/`, symlinks met on the way must be followed inside it: an image's
//! `/var/run -> /run` points at the image's `/run`, not the host's. [`Root::resolve`]
//! does that, the way systemd's `chase()` does with a root.
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

const MAX_SYMLINKS: usize = 40;

#[derive(Debug, Clone)]
pub struct Root {
    dir: PathBuf,
}

impl Root {
    /// The running system's root.
    pub fn host() -> Root {
        Root {
            dir: PathBuf::from("/"),
        }
    }

    /// `dir` (made absolute, symlinks resolved), or `/` for `None`.
    pub fn new(dir: Option<&Path>) -> io::Result<Root> {
        match dir {
            None => Ok(Root::host()),
            Some(d) => Ok(Root {
                dir: std::fs::canonicalize(d)?,
            }),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn is_host(&self) -> bool {
        self.dir == Path::new("/")
    }

    /// The root's directory joined with `p` (an absolute path inside the
    /// root), without following anything.
    pub fn prefix(&self, p: impl AsRef<Path>) -> PathBuf {
        let p = p.as_ref();
        if self.is_host() {
            return p.to_path_buf();
        }
        let rel = p.strip_prefix("/").unwrap_or(p);
        if rel.as_os_str().is_empty() {
            self.dir.clone()
        } else {
            self.dir.join(rel)
        }
    }

    /// Where `p` (an absolute path inside the root) is on the host: every
    /// symlink on the way is followed inside the root, the last component
    /// only if `follow_last`. Components that do not exist are kept as they
    /// are, so the result may name something that does not exist yet.
    pub fn resolve(&self, p: impl AsRef<Path>, follow_last: bool) -> io::Result<PathBuf> {
        let p = p.as_ref();
        if self.is_host() {
            // The kernel resolves the same way; magic links (/proc/self/fd/N)
            // only work this way.
            return Ok(p.to_path_buf());
        }
        let mut done = self.dir.clone();
        let mut todo: VecDeque<OsString> = components(p);
        let mut hops = 0;
        while let Some(c) = todo.pop_front() {
            if c == ".." {
                if done != self.dir {
                    done.pop();
                }
                continue;
            }
            let cand = done.join(&c);
            if todo.is_empty() && !follow_last {
                done = cand;
                break;
            }
            match std::fs::symlink_metadata(&cand) {
                Ok(md) if md.file_type().is_symlink() => {
                    hops += 1;
                    if hops > MAX_SYMLINKS {
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                    let target = std::fs::read_link(&cand)?;
                    if target.is_absolute() {
                        done = self.dir.clone();
                    }
                    let mut next = components(&target);
                    next.extend(todo);
                    todo = next;
                }
                Ok(_) => done = cand,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    done = cand;
                    for c in todo.drain(..) {
                        if c == ".." {
                            if done != self.dir {
                                done.pop();
                            }
                        } else {
                            done.push(c);
                        }
                    }
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(done)
    }

    /// `host` as a path inside the root, for messages (`/` stays as it is).
    pub fn inside(&self, host: &Path) -> PathBuf {
        if self.is_host() {
            return host.to_path_buf();
        }
        match host.strip_prefix(&self.dir) {
            Ok(rel) => Path::new("/").join(rel),
            Err(_) => host.to_path_buf(),
        }
    }
}

fn components(p: &Path) -> VecDeque<OsString> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            _ => None,
        })
        .collect()
}

/// Normalize an absolute path the way systemd's `path_simplify()` does:
/// repeated slashes and `.` components are dropped, as is a trailing slash.
/// `..` is kept (it is resolved against the file system later).
pub fn simplify(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for c in p.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        out.push('/');
        out.push_str(c);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Whether `path` is `prefix` or below it, component-wise (systemd's
/// `path_startswith()`): `/run` covers `/run` and `/run/x`, not `/runx`.
pub fn path_starts_with(path: &str, prefix: &str) -> bool {
    let prefix = simplify(prefix);
    let path = simplify(path);
    if prefix == "/" {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(&prefix)
            .is_some_and(|r| r.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sdconf-rootfs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn simplify_paths() {
        assert_eq!(simplify("/var//lib/./x/"), "/var/lib/x");
        assert_eq!(simplify("/"), "/");
        assert_eq!(simplify("/a/../b"), "/a/../b");
    }

    #[test]
    fn prefixes() {
        assert!(path_starts_with("/run/x", "/run"));
        assert!(path_starts_with("/run", "/run/"));
        assert!(!path_starts_with("/runx", "/run"));
        assert!(path_starts_with("/anything", "/"));
    }

    #[test]
    fn absolute_symlinks_stay_inside_the_root() {
        let d = tmp("abs");
        std::fs::create_dir_all(d.join("run/lock")).unwrap();
        std::fs::create_dir_all(d.join("var")).unwrap();
        symlink("/run", d.join("var/run")).unwrap();
        symlink("../run/lock", d.join("var/lock")).unwrap();
        let root = Root::new(Some(&d)).unwrap();
        let dir = root.dir().to_path_buf();
        assert_eq!(
            root.resolve("/var/run/x", false).unwrap(),
            dir.join("run/x")
        );
        assert_eq!(
            root.resolve("/var/lock", true).unwrap(),
            dir.join("run/lock")
        );
        assert_eq!(
            root.resolve("/var/lock", false).unwrap(),
            dir.join("var/lock")
        );
        // ".." cannot leave the root.
        assert_eq!(root.resolve("/../../etc", false).unwrap(), dir.join("etc"));
        assert_eq!(root.inside(&dir.join("run/x")), PathBuf::from("/run/x"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn symlink_loops_are_detected() {
        let d = tmp("loop");
        symlink("b", d.join("a")).unwrap();
        symlink("a", d.join("b")).unwrap();
        let root = Root::new(Some(&d)).unwrap();
        assert!(root.resolve("/a/x", false).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
