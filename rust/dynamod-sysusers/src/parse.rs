//! Parsing sysusers.d(5) lines.
use dynamod_sdconf::accounts::parse_id;
use dynamod_sdconf::rootfs::simplify;
use dynamod_sdconf::specifier::Specifiers;
use dynamod_sdconf::words::{self, Flags};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `u`: a user, and its group of the same name unless one is given.
    User,
    /// `g`: a group.
    Group,
    /// `m`: a user's membership of a group.
    Member,
    /// `r`: a range to allocate ids from.
    Range,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Origin {
    pub file: String,
    pub line: usize,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.file.is_empty() {
            write!(f, "(implicit)")
        } else {
            write!(f, "{}:{}", self.file, self.line)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: Kind,
    /// User or group name (empty for `r`).
    pub name: String,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// The id is taken from the owner of this file (inside the root).
    pub uid_path: Option<String>,
    pub gid_path: Option<String>,
    /// `u name uid:groupname`, or the group of an `m` line.
    pub group_name: Option<String>,
    /// The gid was given explicitly (`uid:gid`): that group must exist.
    pub gid_strict: bool,
    pub description: Option<String>,
    pub home: Option<String>,
    pub shell: Option<String>,
    /// `u!`: a fully locked account.
    pub locked: bool,
    pub range: Option<(u32, u32)>,
    pub origin: Origin,
}

impl Item {
    pub fn new(kind: Kind, name: &str) -> Item {
        Item {
            kind,
            name: name.to_string(),
            uid: None,
            gid: None,
            uid_path: None,
            gid_path: None,
            group_name: None,
            gid_strict: false,
            description: None,
            home: None,
            shell: None,
            locked: false,
            range: None,
            origin: Origin::default(),
        }
    }

    /// Whether two lines for the same name ask for the same account
    /// (systemd's `item_equivalent()`).
    pub fn equivalent(&self, o: &Item) -> bool {
        self.kind == o.kind
            && self.name == o.name
            && self.uid == o.uid
            && self.gid == o.gid
            && self.uid_path == o.uid_path
            && self.gid_path == o.gid_path
            && self.group_name == o.group_name
            && self.description == o.description
            && self.home == o.home
            && self.locked == o.locked
            && (self.shell == o.shell
                || (is_nologin(self.shell.as_deref()) && is_nologin(o.shell.as_deref())))
    }
}

fn is_nologin(s: Option<&str>) -> bool {
    match s {
        None => true,
        Some(s) => {
            let base = s.rsplit('/').next().unwrap_or(s);
            matches!(base, "nologin" | "false" | "true")
        }
    }
}

/// A user or group name: `[A-Za-z_][A-Za-z0-9_-]*`, at most 31 characters
/// (systemd's strict rule).
pub fn valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 31
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn valid_gecos(s: &str) -> bool {
    !s.contains(':') && !s.contains('\n')
}

/// An absolute, normalized path without colons or control characters
/// (systemd's `valid_home()`, also used for shells).
fn valid_home(s: &str) -> bool {
    if !s.starts_with('/') || s.contains(':') || s.chars().any(|c| c.is_control()) {
        return false;
    }
    if s.contains("//") {
        return false;
    }
    !s.split('/').any(|c| c == "." || c == "..")
}

fn opt(f: Option<String>) -> Option<String> {
    f.filter(|s| !s.is_empty() && s != "-")
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    let (a, b) = match s.split_once('-') {
        Some((a, b)) => (parse_id(a)?, parse_id(b)?),
        None => {
            let v = parse_id(s)?;
            (v, v)
        }
    };
    (a <= b).then_some((a, b))
}

/// Parse one line (not empty, not a comment).
pub fn parse_line(line: &str, spec: &Specifiers) -> Result<Item, String> {
    let flags = Flags {
        unquote: true,
        cunescape: false,
    };
    let mut rest = line;
    let mut f = Vec::with_capacity(6);
    for _ in 0..6 {
        f.push(words::next_word(&mut rest, flags).map_err(|e| format!("Syntax error: {e}."))?);
    }
    if !rest.is_empty() {
        return Err("Trailing garbage.".into());
    }
    let [action, name, id, description, home, shell]: [Option<String>; 6] = f.try_into().unwrap();
    let (Some(action), Some(name)) = (action, name) else {
        return Err("Missing action and name columns.".into());
    };
    let mut chars = action.chars();
    let letter = chars.next().unwrap_or(' ');
    let locked = match chars.as_str() {
        "" => false,
        "!" => true,
        _ => return Err(format!("Unknown modifier '{action}'.")),
    };
    let kind = match letter {
        'u' => Kind::User,
        'g' => Kind::Group,
        'm' => Kind::Member,
        'r' => Kind::Range,
        _ => return Err(format!("Unknown command type '{letter}'.")),
    };
    let expand = |v: Option<String>, what: &str| -> Result<Option<String>, String> {
        match opt(v) {
            None => Ok(None),
            Some(v) => spec
                .expand(&v)
                .map(Some)
                .map_err(|e| format!("Failed to replace specifiers in {what} '{v}': {e}.")),
        }
    };
    let name = expand(Some(name), "name")?;
    if let Some(n) = &name
        && !valid_name(n)
    {
        return Err(format!("'{n}' is not a valid user or group name."));
    }
    let id = expand(id, "ID")?;
    let description = expand(description, "GECOS")?;
    if let Some(d) = &description
        && !valid_gecos(d)
    {
        return Err(format!("'{d}' is not a valid GECOS field."));
    }
    let home = expand(home, "home directory")?;
    if let Some(h) = &home
        && !valid_home(h)
    {
        return Err(format!("'{h}' is not a valid home directory field."));
    }
    let shell = expand(shell, "shell")?;
    if let Some(s) = &shell
        && !valid_home(s)
    {
        return Err(format!("'{s}' is not a valid login shell field."));
    }
    if locked && kind != Kind::User {
        return Err(format!(
            "Flag '!' not permitted on lines of type '{letter}'."
        ));
    }

    let mut item = Item::new(kind, name.as_deref().unwrap_or(""));
    match kind {
        Kind::Range => {
            if name.is_some() {
                return Err("Lines of type 'r' don't take a name field.".into());
            }
            let Some(id) = id else {
                return Err("Lines of type 'r' require an ID range in the third field.".into());
            };
            if description.is_some() || home.is_some() || shell.is_some() {
                return Err(
                    "Lines of type 'r' don't take a GECOS, home directory or shell field.".into(),
                );
            }
            item.range = Some(parse_range(&id).ok_or_else(|| format!("Invalid UID range {id}."))?);
        }
        Kind::Member => {
            if name.is_none() {
                return Err("Lines of type 'm' require a user name in the second field.".into());
            }
            let Some(group) = id else {
                return Err("Lines of type 'm' require a group name in the third field.".into());
            };
            if !valid_name(&group) {
                return Err(format!("'{group}' is not a valid user or group name."));
            }
            if description.is_some() || home.is_some() || shell.is_some() {
                return Err(
                    "Lines of type 'm' don't take a GECOS, home directory or shell field.".into(),
                );
            }
            item.group_name = Some(group);
        }
        Kind::User => {
            if name.is_none() {
                return Err("Lines of type 'u' require a user name in the second field.".into());
            }
            if let Some(id) = id {
                if id.starts_with('/') {
                    item.uid_path = Some(simplify(&id));
                } else {
                    let (u, g) = match id.split_once(':') {
                        Some((u, g)) => (u.to_string(), Some(g.to_string())),
                        None => (id.clone(), None),
                    };
                    if let Some(g) = g {
                        if let Some(gid) = parse_id(&g) {
                            item.gid = Some(gid);
                            item.gid_strict = true;
                        } else if valid_name(&g) {
                            item.group_name = Some(g);
                        } else {
                            return Err(format!("Failed to parse GID: '{g}'."));
                        }
                    }
                    if u != "-" {
                        item.uid = Some(
                            parse_id(&u).ok_or_else(|| format!("Failed to parse UID: '{u}'."))?,
                        );
                    }
                }
            }
            item.description = description;
            item.home = home;
            item.shell = shell;
            item.locked = locked;
        }
        Kind::Group => {
            if name.is_none() {
                return Err("Lines of type 'g' require a user name in the second field.".into());
            }
            if description.is_some() {
                return Err("Lines of type 'g' don't take a GECOS field.".into());
            }
            if home.is_some() || shell.is_some() {
                return Err("Lines of type 'g' don't take a home directory or shell field.".into());
            }
            if let Some(id) = id {
                if id.starts_with('/') {
                    item.gid_path = Some(simplify(&id));
                } else {
                    item.gid =
                        Some(parse_id(&id).ok_or_else(|| format!("Failed to parse GID: '{id}'."))?);
                }
            }
        }
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynamod_sdconf::Root;

    fn spec() -> Specifiers {
        Specifiers::new(&Root::host(), false)
            .with_value('H', "box")
            .with_value('m', "0123456789abcdef0123456789abcdef")
    }

    fn p(line: &str) -> Item {
        parse_line(line, &spec()).unwrap()
    }

    fn e(line: &str) -> String {
        parse_line(line, &spec()).unwrap_err()
    }

    #[test]
    fn users() {
        let i = p(r#"u sshd 50:50 "sshd PrivSep" /var/lib/sshd /usr/bin/false"#);
        assert_eq!(
            (i.kind, i.name.as_str(), i.uid, i.gid, i.gid_strict),
            (Kind::User, "sshd", Some(50), Some(50), true)
        );
        assert_eq!(i.description.as_deref(), Some("sshd PrivSep"));
        assert_eq!(i.home.as_deref(), Some("/var/lib/sshd"));
        assert_eq!(i.shell.as_deref(), Some("/usr/bin/false"));
        let i = p("u brltty 90:90 \"Braille Device Daemon\" /var/lib/brltty");
        assert_eq!(i.shell, None);
        let i = p("u flatpak 35:35 \"Flatpak system helper\" - /usr/bin/false");
        assert_eq!((i.home, i.shell.as_deref()), (None, Some("/usr/bin/false")));
        let i = p(r#"u libvirt-qemu 64:libvirt-qemu "libvirt QEMU user" - -"#);
        assert_eq!(
            (i.uid, i.gid, i.group_name.as_deref()),
            (Some(64), None, Some("libvirt-qemu"))
        );
        let i = p("u radvd - \"radvd daemon\"");
        assert_eq!((i.uid, i.gid), (None, None));
        let i = p("u x -:100");
        assert_eq!((i.uid, i.gid), (None, Some(100)));
        let i = p("u _authd /usr/bin/authd \"Authorization user\"");
        assert_eq!(i.uid_path.as_deref(), Some("/usr/bin/authd"));
        let i = p("u! locked 400");
        assert!(i.locked);
        let i = p("u host-%H 401 \"on %H\"");
        assert_eq!(
            (i.name.as_str(), i.description.as_deref()),
            ("host-box", Some("on box"))
        );
        assert!(p("u x 1 \"\"").description.is_none());
    }

    #[test]
    fn groups_members_ranges() {
        let i = p("g sshd 50");
        assert_eq!(
            (i.kind, i.gid, i.gid_strict),
            (Kind::Group, Some(50), false)
        );
        assert_eq!(p("g input -").gid, None);
        assert_eq!(p("g input").gid, None);
        assert_eq!(
            p("g setgid /usr/bin/thing").gid_path.as_deref(),
            Some("/usr/bin/thing")
        );
        let i = p("m brltty tty");
        assert_eq!(
            (i.kind, i.name.as_str(), i.group_name.as_deref()),
            (Kind::Member, "brltty", Some("tty"))
        );
        assert_eq!(p("r - 500-900").range, Some((500, 900)));
        assert_eq!(p("r - 42").range, Some((42, 42)));
    }

    #[test]
    fn invalid() {
        assert!(e("u").contains("Missing action and name"));
        assert!(e("x foo").contains("Unknown command type"));
        assert!(e("u? foo").contains("Unknown modifier"));
        assert!(e("g! foo").contains("Flag '!' not permitted"));
        assert!(e("u 1foo").contains("not a valid user or group name"));
        assert!(e("u foo.bar").contains("not a valid user or group name"));
        assert!(e("u abcdefghijklmnopqrstuvwxyz012345").contains("not a valid"));
        assert!(e("u foo 65535").contains("Failed to parse UID"));
        assert!(e("u foo 4294967295").contains("Failed to parse UID"));
        assert!(e("u foo abc").contains("Failed to parse UID"));
        assert!(e("u foo 1:2:3").contains("Failed to parse GID"));
        assert!(e("u foo 1 \"a:b\"").contains("GECOS"));
        assert!(e("u foo 1 x relative").contains("home directory"));
        assert!(e("u foo 1 x /a/../b").contains("home directory"));
        assert!(e("u foo 1 x / bin/sh").contains("login shell"));
        assert!(e("u foo 1 x / /bin/sh extra").contains("Trailing garbage"));
        assert!(e("u foo 1 \"unterminated").contains("Syntax error"));
        assert!(e("g foo 1 gecos").contains("don't take a GECOS"));
        assert!(e("g foo x").contains("Failed to parse GID"));
        assert!(e("m foo").contains("require a group name"));
        assert!(e("m foo bad.group").contains("not a valid"));
        assert!(e("r foo 1-2").contains("don't take a name"));
        assert!(e("r - 9-2").contains("Invalid UID range"));
        assert!(e("r -").contains("require an ID range"));
        assert!(e("u foo 1 \"%h\"").contains("specifiers"));
    }

    #[test]
    fn equivalence() {
        let a = p("u foo 10 \"x\"");
        let mut b = p("u foo 10 \"x\" / /usr/sbin/nologin");
        b.home = None;
        assert!(a.equivalent(&b));
        assert!(!a.equivalent(&p("u foo 11 \"x\"")));
    }
}
