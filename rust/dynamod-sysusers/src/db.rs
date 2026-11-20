//! `/etc/passwd`, `/etc/group`, `/etc/shadow` and `/etc/gshadow` inside the
//! root: read as lines (kept verbatim), new entries appended before any NIS
//! `+`/`-` lines, written atomically under `/etc/.pwd.lock`.
use std::collections::HashMap;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dynamod_sdconf::accounts::{is_nis, parse_group_line, parse_passwd_line};
use dynamod_sdconf::{Root, log_debug, log_warning};

use crate::plan::Plan;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Which {
    Passwd,
    Group,
    Shadow,
    Gshadow,
}

impl Which {
    pub const ALL: [Which; 4] = [Which::Passwd, Which::Group, Which::Shadow, Which::Gshadow];
    /// The order in which the new files replace the old ones.
    pub const COMMIT_ORDER: [Which; 4] =
        [Which::Group, Which::Gshadow, Which::Passwd, Which::Shadow];

    pub fn path(self) -> &'static str {
        match self {
            Which::Passwd => "/etc/passwd",
            Which::Group => "/etc/group",
            Which::Shadow => "/etc/shadow",
            Which::Gshadow => "/etc/gshadow",
        }
    }

    /// Mode of a file that does not exist yet (systemd's choice).
    fn default_mode(self) -> u32 {
        match self {
            Which::Passwd | Which::Group => 0o644,
            Which::Shadow | Which::Gshadow => 0o000,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AccountFile {
    pub host: PathBuf,
    pub lines: Vec<String>,
    /// mode, uid, gid of the existing file.
    pub meta: Option<(u32, u32, u32)>,
}

#[derive(Debug, Clone, Default)]
pub struct Db {
    pub files: HashMap<Which, AccountFile>,
    uid_by_name: HashMap<String, u32>,
    name_by_uid: HashMap<u32, String>,
    gid_by_name: HashMap<String, u32>,
    name_by_gid: HashMap<u32, String>,
    members: HashMap<String, Vec<String>>,
}

fn split_lines(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text.split('\n').map(str::to_string).collect();
    if v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

impl Db {
    pub fn load(root: &Root) -> io::Result<Db> {
        let mut files = HashMap::new();
        for w in Which::ALL {
            let host = root.resolve(w.path(), false)?;
            let (lines, meta) = match fs::read(&host) {
                Ok(bytes) => {
                    let md = fs::metadata(&host)?;
                    let text = String::from_utf8(bytes).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("{}: not valid UTF-8", w.path()),
                        )
                    })?;
                    (
                        split_lines(&text),
                        Some((md.mode() & 0o7777, md.uid(), md.gid())),
                    )
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), None),
                Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", host.display()))),
            };
            files.insert(w, AccountFile { host, lines, meta });
        }
        Ok(Db::from_files(files))
    }

    /// From file contents (tests).
    #[cfg(test)]
    pub fn from_texts(passwd: &str, group: &str, shadow: &str, gshadow: &str) -> Db {
        let mut files = HashMap::new();
        for (w, t) in [
            (Which::Passwd, passwd),
            (Which::Group, group),
            (Which::Shadow, shadow),
            (Which::Gshadow, gshadow),
        ] {
            files.insert(
                w,
                AccountFile {
                    host: PathBuf::from(w.path()),
                    lines: split_lines(t),
                    meta: Some((0o644, 0, 0)),
                },
            );
        }
        Db::from_files(files)
    }

    fn from_files(files: HashMap<Which, AccountFile>) -> Db {
        let mut db = Db {
            files,
            ..Default::default()
        };
        for l in &db.files[&Which::Passwd].lines {
            if let Some(e) = parse_passwd_line(l) {
                db.uid_by_name.entry(e.name.clone()).or_insert(e.uid);
                db.name_by_uid.entry(e.uid).or_insert(e.name);
            }
        }
        for l in &db.files[&Which::Group].lines {
            if let Some(e) = parse_group_line(l) {
                db.gid_by_name.entry(e.name.clone()).or_insert(e.gid);
                db.name_by_gid.entry(e.gid).or_insert(e.name.clone());
                db.members.entry(e.name).or_insert(e.members);
            }
        }
        db
    }

    pub fn uid_by_name(&self, n: &str) -> Option<u32> {
        self.uid_by_name.get(n).copied()
    }
    pub fn name_by_uid(&self, u: u32) -> Option<&str> {
        self.name_by_uid.get(&u).map(String::as_str)
    }
    pub fn gid_by_name(&self, n: &str) -> Option<u32> {
        self.gid_by_name.get(n).copied()
    }
    pub fn name_by_gid(&self, g: u32) -> Option<&str> {
        self.name_by_gid.get(&g).map(String::as_str)
    }
    pub fn members(&self, group: &str) -> Option<&[String]> {
        self.members.get(group).map(Vec::as_slice)
    }
}

/// Lines up to the first NIS entry, and the rest.
fn split_nis(lines: &[String]) -> (&[String], &[String]) {
    let n = lines.iter().position(|l| is_nis(l)).unwrap_or(lines.len());
    lines.split_at(n)
}

fn add_members(line: &str, field: usize, add: &[String]) -> String {
    let mut f: Vec<String> = line.split(':').map(str::to_string).collect();
    while f.len() <= field {
        f.push(String::new());
    }
    let mut m: Vec<String> = f[field]
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    for a in add {
        if !m.contains(a) {
            m.push(a.clone());
        }
    }
    f[field] = m.join(",");
    f.join(":")
}

/// The new contents of the files that change.
pub fn render(db: &Db, plan: &Plan, lstchg: i64) -> Result<Vec<(Which, Vec<String>)>, String> {
    let mut out = Vec::new();
    let first_field = |l: &str| l.split(':').next().unwrap_or("").to_string();
    let add_for = |name: &str| {
        plan.add_members
            .iter()
            .find(|(g, _)| g == name)
            .map(|(_, m)| m.as_slice())
    };

    if !plan.users.is_empty() {
        let (head, nis) = split_nis(&db.files[&Which::Passwd].lines);
        for l in head {
            if let Some(e) = parse_passwd_line(l)
                && let Some(u) = plan
                    .users
                    .iter()
                    .find(|u| u.name == e.name || u.uid == e.uid)
            {
                return Err(format!(
                    "/etc/passwd: Detected collision for user '{}' (UID {}).",
                    u.name, u.uid
                ));
            }
        }
        let mut lines = head.to_vec();
        for u in &plan.users {
            lines.push(format!(
                "{}:x:{}:{}:{}:{}:{}",
                u.name, u.uid, u.gid, u.gecos, u.home, u.shell
            ));
        }
        lines.extend_from_slice(nis);
        out.push((Which::Passwd, lines));

        let (head, nis) = split_nis(&db.files[&Which::Shadow].lines);
        let mut lines = Vec::new();
        let mut stale = Vec::new();
        for l in head {
            let name = first_field(l);
            if plan.users.iter().any(|u| u.name == name) {
                // An entry left behind by an earlier account of that name:
                // kept, with today's date, as systemd-sysusers does.
                log_warning!("/etc/shadow already has an entry for new user '{name}', keeping it");
                let mut f: Vec<&str> = l.split(':').collect();
                let d = lstchg.to_string();
                if f.len() > 2 {
                    f[2] = &d;
                }
                lines.push(f.join(":"));
                stale.push(name);
            } else {
                lines.push(l.clone());
            }
        }
        for u in plan.users.iter().filter(|u| !stale.contains(&u.name)) {
            let expire = if u.locked { "1" } else { "" };
            lines.push(format!("{}:!*:{lstchg}:::::{expire}:", u.name));
        }
        lines.extend_from_slice(nis);
        out.push((Which::Shadow, lines));
    }

    if !plan.groups.is_empty() || !plan.add_members.is_empty() {
        let (head, nis) = split_nis(&db.files[&Which::Group].lines);
        let mut lines = Vec::new();
        for l in head {
            match parse_group_line(l) {
                Some(e) => {
                    if let Some(g) = plan
                        .groups
                        .iter()
                        .find(|g| g.name == e.name || g.gid == e.gid)
                    {
                        return Err(format!(
                            "/etc/group: Detected collision for group '{}' (GID {}).",
                            g.name, g.gid
                        ));
                    }
                    match add_for(&e.name) {
                        Some(add) => lines.push(add_members(l, 3, add)),
                        None => lines.push(l.clone()),
                    }
                }
                None => lines.push(l.clone()),
            }
        }
        for g in &plan.groups {
            lines.push(format!("{}:x:{}:{}", g.name, g.gid, g.members.join(",")));
        }
        lines.extend_from_slice(nis);
        out.push((Which::Group, lines));

        let (head, nis) = split_nis(&db.files[&Which::Gshadow].lines);
        let mut lines = Vec::new();
        let mut have = Vec::new();
        for l in head {
            let name = first_field(l);
            let add: Option<Vec<String>> = add_for(&name).map(|a| a.to_vec()).or_else(|| {
                plan.groups
                    .iter()
                    .find(|g| g.name == name)
                    .map(|g| g.members.clone())
            });
            if plan.groups.iter().any(|g| g.name == name) {
                have.push(name.clone());
            }
            match add {
                Some(a) if !a.is_empty() => lines.push(add_members(l, 3, &a)),
                _ => lines.push(l.clone()),
            }
        }
        for g in plan.groups.iter().filter(|g| !have.contains(&g.name)) {
            lines.push(format!("{}:!*::{}", g.name, g.members.join(",")));
        }
        lines.extend_from_slice(nis);
        out.push((Which::Gshadow, lines));
    }

    // Only what actually changes.
    out.retain(|(w, lines)| {
        *lines != db.files[w].lines || db.files[w].meta.is_none() && !lines.is_empty()
    });
    Ok(out)
}

/// The lock that shadow-utils' lckpwdf() and systemd take: a write lock on
/// `/etc/.pwd.lock`, waited for up to 15 seconds.
pub struct PwdLock {
    _file: File,
}

pub fn take_lock(root: &Root) -> Result<PwdLock, String> {
    let path = root
        .resolve("/etc/.pwd.lock", false)
        .map_err(|e| format!("/etc/.pwd.lock: {e}"))?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NOCTTY)
        .open(&path)
        .map_err(|e| format!("Failed to open lock file {}: {e}", path.display()))?;
    // SAFETY: a zeroed struct flock is valid; the fields are set below.
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = libc::F_WRLCK as _;
    fl.l_whence = libc::SEEK_SET as _;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        // SAFETY: file is open; fl is a valid struct flock.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &fl) } == 0 {
            return Ok(PwdLock { _file: file });
        }
        let e = io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EAGAIN)) {
            return Err(format!("Failed to lock {}: {e}", path.display()));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Timed out waiting for the lock on {} (another program is changing the account files)",
                path.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn temp_name(target: &Path, tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".#{name}{tag}{:x}{nanos:x}", std::process::id()))
}

/// Write `content` to a new temporary file next to `target`, with the given
/// mode and owner, synced to disk.
fn write_temp(
    target: &Path,
    content: &[u8],
    mode: u32,
    owner: Option<(u32, u32)>,
    tag: &str,
) -> Result<PathBuf, String> {
    let tmp = temp_name(target, tag);
    let r = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(content)?;
        if let Some((uid, gid)) = owner {
            let md = f.metadata()?;
            if (md.uid(), md.gid()) != (uid, gid) {
                std::os::unix::fs::fchown(&f, Some(uid), Some(gid)).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "cannot give it the owner {uid}:{gid} of the file it replaces: {e}"
                        ),
                    )
                })?;
            }
        }
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()
    })();
    match r {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(format!("Failed to write a new {}: {e}", target.display()))
        }
    }
}

/// Keep the previous version as `FILE-`, as shadow-utils and systemd do.
fn make_backup(f: &AccountFile) {
    let Some((mode, uid, gid)) = f.meta else {
        return;
    };
    let backup = PathBuf::from(format!("{}-", f.host.display()));
    let content = match fs::read(&f.host) {
        Ok(c) => c,
        Err(e) => return log_warning!("Failed to back up {}: {e}", f.host.display()),
    };
    // Ownership of the backup is best effort.
    let owner = match write_temp(&backup, &content, mode, Some((uid, gid)), "-") {
        Ok(t) => Ok(t),
        Err(_) => write_temp(&backup, &content, mode, None, "-"),
    };
    let tmp = match owner {
        Ok(t) => t,
        Err(e) => return log_warning!("Failed to back up {}: {e}", f.host.display()),
    };
    if let (Ok(md), Ok(file)) = (fs::metadata(&f.host), File::open(&tmp))
        && let (Ok(a), Ok(m)) = (md.accessed(), md.modified())
    {
        let _ = file.set_times(FileTimes::new().set_accessed(a).set_modified(m));
    }
    if let Err(e) = fs::rename(&tmp, &backup) {
        let _ = fs::remove_file(&tmp);
        log_warning!("Failed to back up {}: {e}", f.host.display());
    }
}

/// Replace the files: all new versions are written first; only if every
/// one could be written (with the old file's mode and owner) are they
/// renamed into place.
pub fn commit(db: &Db, outputs: &[(Which, Vec<String>)]) -> Result<(), String> {
    let mut temps: Vec<(Which, PathBuf)> = Vec::new();
    for (w, lines) in outputs {
        let f = &db.files[w];
        let mut content = lines.join("\n").into_bytes();
        if !content.is_empty() {
            content.push(b'\n');
        }
        let (mode, owner) = match f.meta {
            Some((m, u, g)) => (m, Some((u, g))),
            None => (w.default_mode(), None),
        };
        match write_temp(&f.host, &content, mode, owner, "") {
            Ok(t) => temps.push((*w, t)),
            Err(e) => {
                for (_, t) in &temps {
                    let _ = fs::remove_file(t);
                }
                return Err(e);
            }
        }
    }
    for (w, _) in outputs {
        make_backup(&db.files[w]);
    }
    for w in Which::COMMIT_ORDER {
        let Some((_, tmp)) = temps.iter().find(|(t, _)| *t == w) else {
            continue;
        };
        let target = &db.files[&w].host;
        if let Err(e) = fs::rename(tmp, target) {
            for (_, t) in &temps {
                let _ = fs::remove_file(t);
            }
            return Err(format!("Failed to replace {}: {e}", target.display()));
        }
        log_debug!("Wrote {}", target.display());
    }
    if let Some(dir) = temps
        .first()
        .and_then(|_| db.files[&Which::Passwd].host.parent())
        && let Ok(d) = File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}
