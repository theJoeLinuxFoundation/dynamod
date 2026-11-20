//! The root's `/etc/passwd` and `/etc/group`, read directly.
use std::collections::HashMap;

use crate::rootfs::Root;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdEntry {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupEntry {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

/// Whether a line is a NIS compat entry (`+name`, `-name`, `+`).
pub fn is_nis(line: &str) -> bool {
    line.starts_with('+') || line.starts_with('-')
}

/// `name:password:uid:gid:gecos:home:shell`; `None` for comments, NIS
/// entries and malformed lines.
pub fn parse_passwd_line(line: &str) -> Option<PasswdEntry> {
    if line.is_empty() || line.starts_with('#') || is_nis(line) {
        return None;
    }
    let f: Vec<&str> = line.split(':').collect();
    if f.len() < 7 || f[0].is_empty() {
        return None;
    }
    Some(PasswdEntry {
        name: f[0].to_string(),
        uid: f[2].parse().ok()?,
        gid: f[3].parse().ok()?,
    })
}

/// `name:password:gid:member,member`.
pub fn parse_group_line(line: &str) -> Option<GroupEntry> {
    if line.is_empty() || line.starts_with('#') || is_nis(line) {
        return None;
    }
    let f: Vec<&str> = line.split(':').collect();
    if f.len() < 4 || f[0].is_empty() {
        return None;
    }
    let members = f[3]
        .split(',')
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect();
    Some(GroupEntry {
        name: f[0].to_string(),
        gid: f[2].parse().ok()?,
        members,
    })
}

/// A valid user or group id: 32 bits, not 65535 or 4294967295 (both stand
/// for "no id"), written in decimal.
pub fn parse_id(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: u32 = s.parse().ok()?;
    if v == 65535 || v == u32::MAX {
        None
    } else {
        Some(v)
    }
}

/// Names to ids, from a root's account files.
#[derive(Debug, Default, Clone)]
pub struct IdDb {
    users: HashMap<String, u32>,
    groups: HashMap<String, u32>,
}

impl IdDb {
    /// Read `/etc/passwd` and `/etc/group` inside `root` (missing files are
    /// empty).
    pub fn load(root: &Root) -> IdDb {
        let read = |p: &str| {
            root.resolve(p, true)
                .ok()
                .and_then(|h| std::fs::read_to_string(h).ok())
                .unwrap_or_default()
        };
        IdDb::from_texts(&read("/etc/passwd"), &read("/etc/group"))
    }

    pub fn from_texts(passwd: &str, group: &str) -> IdDb {
        let mut db = IdDb::default();
        for e in passwd.lines().filter_map(parse_passwd_line) {
            db.users.entry(e.name).or_insert(e.uid);
        }
        for e in group.lines().filter_map(parse_group_line) {
            db.groups.entry(e.name).or_insert(e.gid);
        }
        db
    }

    /// A user name or numeric uid.
    pub fn uid(&self, user: &str) -> Option<u32> {
        if let Some(id) = parse_id(user) {
            return Some(id);
        }
        if user == "root" {
            return Some(0);
        }
        self.users.get(user).copied()
    }

    /// A group name or numeric gid.
    pub fn gid(&self, group: &str) -> Option<u32> {
        if let Some(id) = parse_id(group) {
            return Some(id);
        }
        if group == "root" {
            return Some(0);
        }
        self.groups.get(group).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries() {
        assert_eq!(
            parse_passwd_line("sshd:x:50:50:sshd PrivSep:/var/lib/sshd:/usr/bin/false"),
            Some(PasswdEntry {
                name: "sshd".into(),
                uid: 50,
                gid: 50
            })
        );
        assert_eq!(parse_passwd_line("+@nis::::::"), None);
        assert_eq!(parse_passwd_line("broken:x:1"), None);
        assert_eq!(
            parse_group_line("wheel:x:97:joe,liveuser"),
            Some(GroupEntry {
                name: "wheel".into(),
                gid: 97,
                members: vec!["joe".into(), "liveuser".into()]
            })
        );
        assert_eq!(
            parse_group_line("sshd:x:50:").unwrap().members,
            Vec::<String>::new()
        );
    }

    #[test]
    fn ids() {
        assert_eq!(parse_id("0"), Some(0));
        assert_eq!(parse_id("65534"), Some(65534));
        assert_eq!(parse_id("65535"), None);
        assert_eq!(parse_id("4294967295"), None);
        assert_eq!(parse_id("-1"), None);
        assert_eq!(parse_id("+5"), None);
        let db = IdDb::from_texts("sshd:x:50:50::/:/bin/false\n", "systemd-journal:x:23:\n");
        assert_eq!(db.uid("sshd"), Some(50));
        assert_eq!(db.uid("1000"), Some(1000));
        assert_eq!(db.uid("root"), Some(0));
        assert_eq!(db.uid("nobody"), None);
        assert_eq!(db.gid("systemd-journal"), Some(23));
    }
}
