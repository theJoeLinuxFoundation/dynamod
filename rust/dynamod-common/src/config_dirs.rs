//! Where definitions come from, and whether a service is enabled.
//!
//! Definitions are `*.toml` files in a list of directories, lowest
//! precedence first (`/usr/lib/dynamod/...` for packages, then
//! `/etc/dynamod/...` for the administrator). Files are matched by file
//! name: a file in a later directory replaces the earlier one entirely, and
//! one that is a symlink to `/dev/null` or empty masks it (nothing is loaded).
//!
//! Whether a service starts at boot is its definition's `enabled` field,
//! overridden by the existence of `/etc/dynamod/disabled/NAME` (disabled) or
//! else `/etc/dynamod/enabled/NAME` (enabled). `disabled` wins when both
//! exist.
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::paths;

/// One definition file name after merging the directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedFile {
    /// File name, e.g. `sshd.toml`.
    pub file_name: String,
    /// The winning file (the highest-precedence directory that has it).
    pub path: PathBuf,
    /// The winning file masks the name: load nothing.
    pub masked: bool,
    /// Lower-precedence files of the same name that the winner replaces.
    pub replaces: Vec<PathBuf>,
}

/// Merge the `*.toml` files of `dirs` (lowest precedence first) by file name.
/// Missing directories are skipped. The result is sorted by file name.
pub fn merge_dirs(dirs: &[PathBuf]) -> Vec<MergedFile> {
    let mut by_name: BTreeMap<String, MergedFile> = BTreeMap::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".toml") && !n.starts_with('.'))
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let masked = is_mask(&path);
            match by_name.get_mut(&name) {
                Some(existing) => {
                    let old = std::mem::replace(&mut existing.path, path);
                    existing.replaces.push(old);
                    existing.masked = masked;
                }
                None => {
                    by_name.insert(
                        name.clone(),
                        MergedFile {
                            file_name: name,
                            path,
                            masked,
                            replaces: Vec::new(),
                        },
                    );
                }
            }
        }
    }
    by_name.into_values().collect()
}

/// A mask: a symlink to `/dev/null`, or an empty file.
pub fn is_mask(path: &Path) -> bool {
    if let Ok(target) = fs::read_link(path) {
        if target == Path::new("/dev/null") {
            return true;
        }
    }
    if let Ok(canonical) = fs::canonicalize(path) {
        if canonical == Path::new("/dev/null") {
            return true;
        }
    }
    matches!(fs::metadata(path), Ok(m) if m.is_file() && m.len() == 0)
}

/// Why a service is (not) started at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnablementSource {
    /// The definition's `enabled` field (default `true`).
    Definition,
    /// `/etc/dynamod/enabled/NAME` exists.
    EnabledMarker,
    /// `/etc/dynamod/disabled/NAME` exists.
    DisabledMarker,
}

/// Whether a service is started at boot, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enablement {
    pub enabled: bool,
    pub source: EnablementSource,
}

impl Enablement {
    /// "enabled" or "disabled".
    pub fn state(&self) -> &'static str {
        if self.enabled { "enabled" } else { "disabled" }
    }

    /// Short reason, for status output.
    pub fn source_name(&self) -> &'static str {
        match self.source {
            EnablementSource::Definition => "definition",
            EnablementSource::EnabledMarker => "enabled marker",
            EnablementSource::DisabledMarker => "disabled marker",
        }
    }
}

/// Check that `name` can be used as a marker file name.
pub fn validate_service_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.len() > 255 {
        return Err(format!("invalid service name: '{name}'"));
    }
    if name.contains('/') || name.contains('\0') {
        return Err(format!("invalid service name: '{name}'"));
    }
    Ok(())
}

fn marker(root: Option<&Path>, dir: &str, name: &str) -> PathBuf {
    paths::rooted(root, dir).join(name)
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Resolve whether service `name`, whose definition says `default`, is enabled.
pub fn enablement(root: Option<&Path>, name: &str, default: bool) -> Enablement {
    if validate_service_name(name).is_ok() {
        if exists(&marker(root, paths::DISABLED_DIR, name)) {
            return Enablement {
                enabled: false,
                source: EnablementSource::DisabledMarker,
            };
        }
        if exists(&marker(root, paths::ENABLED_DIR, name)) {
            return Enablement {
                enabled: true,
                source: EnablementSource::EnabledMarker,
            };
        }
    }
    Enablement {
        enabled: default,
        source: EnablementSource::Definition,
    }
}

/// Write the administrator's choice for `name`: create `enabled/NAME` and
/// remove `disabled/NAME` (or the reverse). Returns whether anything changed.
pub fn set_enabled(root: Option<&Path>, name: &str, enabled: bool) -> io::Result<bool> {
    validate_service_name(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let (keep_dir, drop_dir) = if enabled {
        (paths::ENABLED_DIR, paths::DISABLED_DIR)
    } else {
        (paths::DISABLED_DIR, paths::ENABLED_DIR)
    };
    let mut changed = false;

    let drop = marker(root, drop_dir, name);
    match fs::remove_file(&drop) {
        Ok(()) => changed = true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let keep = marker(root, keep_dir, name);
    if !exists(&keep) {
        fs::create_dir_all(paths::rooted(root, keep_dir))?;
        fs::File::create(&keep)?;
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh, empty directory under the system temp dir.
    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dynamod-common-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn etc_replaces_vendor_file_of_same_name() {
        let root = temp_dir("merge");
        let dirs = paths::service_dirs(Some(&root));
        write(&dirs[0].join("a.toml"), "vendor a");
        write(&dirs[0].join("b.toml"), "vendor b");
        write(&dirs[1].join("b.toml"), "admin b");
        write(&dirs[1].join("c.toml"), "admin c");
        write(&dirs[1].join("notes.txt"), "ignored");

        let merged = merge_dirs(&dirs);
        let names: Vec<&str> = merged.iter().map(|m| m.file_name.as_str()).collect();
        assert_eq!(names, vec!["a.toml", "b.toml", "c.toml"]);
        assert_eq!(merged[0].path, dirs[0].join("a.toml"));
        assert_eq!(merged[1].path, dirs[1].join("b.toml"));
        assert_eq!(merged[1].replaces, vec![dirs[0].join("b.toml")]);
        assert_eq!(merged[2].path, dirs[1].join("c.toml"));
        assert!(merged.iter().all(|m| !m.masked));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn devnull_symlink_and_empty_file_mask() {
        let root = temp_dir("mask");
        let dirs = paths::service_dirs(Some(&root));
        write(&dirs[0].join("a.toml"), "vendor a");
        write(&dirs[0].join("b.toml"), "vendor b");
        write(&dirs[0].join("c.toml"), "vendor c");
        fs::create_dir_all(&dirs[1]).unwrap();
        std::os::unix::fs::symlink("/dev/null", dirs[1].join("a.toml")).unwrap();
        write(&dirs[1].join("b.toml"), "");

        let merged = merge_dirs(&dirs);
        assert_eq!(merged.len(), 3);
        assert!(merged[0].masked, "symlink to /dev/null masks");
        assert!(merged[1].masked, "empty file masks");
        assert!(!merged[2].masked);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_directories_are_skipped() {
        let root = temp_dir("missing");
        assert!(merge_dirs(&paths::service_dirs(Some(&root))).is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn enablement_resolution() {
        let root = temp_dir("enable");
        let r = Some(root.as_path());

        // No markers: the definition decides.
        assert_eq!(
            enablement(r, "svc", true),
            Enablement {
                enabled: true,
                source: EnablementSource::Definition
            }
        );
        assert!(!enablement(r, "svc", false).enabled);

        // enabled marker turns a disabled-by-default service on.
        write(&root.join("etc/dynamod/enabled/svc"), "");
        assert_eq!(
            enablement(r, "svc", false),
            Enablement {
                enabled: true,
                source: EnablementSource::EnabledMarker
            }
        );

        // disabled marker wins over the enabled marker.
        write(&root.join("etc/dynamod/disabled/svc"), "");
        assert_eq!(
            enablement(r, "svc", true),
            Enablement {
                enabled: false,
                source: EnablementSource::DisabledMarker
            }
        );

        // A dangling symlink counts: existence is what matters.
        fs::create_dir_all(root.join("etc/dynamod/disabled")).unwrap();
        std::os::unix::fs::symlink("/nonexistent", root.join("etc/dynamod/disabled/other"))
            .unwrap();
        assert!(!enablement(r, "other", true).enabled);

        // Names that are not plain file names are never looked up.
        assert!(enablement(r, "../x", true).enabled);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn set_enabled_writes_and_removes_markers() {
        let root = temp_dir("setenable");
        let r = Some(root.as_path());

        assert!(set_enabled(r, "svc", true).unwrap());
        assert!(root.join("etc/dynamod/enabled/svc").exists());
        assert!(!set_enabled(r, "svc", true).unwrap(), "idempotent");

        assert!(set_enabled(r, "svc", false).unwrap());
        assert!(!root.join("etc/dynamod/enabled/svc").exists());
        assert!(root.join("etc/dynamod/disabled/svc").exists());
        assert!(!enablement(r, "svc", true).enabled);

        assert!(set_enabled(r, "svc", true).unwrap());
        assert!(!root.join("etc/dynamod/disabled/svc").exists());
        assert!(enablement(r, "svc", false).enabled);

        assert!(set_enabled(r, "a/b", true).is_err());
        assert!(set_enabled(r, "..", true).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
