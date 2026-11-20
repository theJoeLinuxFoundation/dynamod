//! Deciding what to create, as systemd-sysusers does: groups first (from
//! `g` lines, then the groups of `u` lines), then users; nothing that exists
//! is changed, except that `m` lines add members to existing groups.
//!
//! An id given in the configuration is used if it is free; otherwise (and
//! for `-`) one is allocated downwards from the top of the ranges (`r`
//! lines, or 1-999), the same number for a user and its group when
//! possible.
use std::collections::HashMap;

use dynamod_sdconf::{log_debug, log_info};

use crate::db::Db;
use crate::parse::{Item, Kind, Origin};

pub const DEFAULT_RANGE: (u32, u32) = (1, 999);

/// The lines of all files, by name; the first line for a name wins.
#[derive(Debug, Default)]
pub struct Config {
    pub users: Vec<Item>,
    pub groups: Vec<Item>,
    /// group -> users, in order of appearance.
    pub members: Vec<(String, Vec<String>, Origin)>,
    pub ranges: Vec<(u32, u32)>,
}

impl Config {
    /// Add a line; returns a warning if it conflicts with an earlier one.
    pub fn add(&mut self, item: Item) -> Option<String> {
        let list = match item.kind {
            Kind::Range => {
                self.ranges.push(item.range.unwrap());
                return None;
            }
            Kind::Member => {
                let group = item.group_name.clone().unwrap();
                match self.members.iter_mut().find(|(g, _, _)| *g == group) {
                    Some((_, users, _)) => {
                        if !users.contains(&item.name) {
                            users.push(item.name);
                        }
                    }
                    None => self.members.push((group, vec![item.name], item.origin)),
                }
                return None;
            }
            Kind::User => &mut self.users,
            Kind::Group => &mut self.groups,
        };
        if let Some(e) = list.iter().find(|e| e.name == item.name) {
            if e.equivalent(&item) {
                return None;
            }
            let what = if item.kind == Kind::User {
                "user"
            } else {
                "group"
            };
            return Some(format!(
                "{}: Conflict with earlier configuration for {what} '{}' in {}, ignoring line.",
                item.origin, item.name, e.origin
            ));
        }
        list.push(item);
        None
    }

    /// Users and groups named only in `m` lines are created too.
    pub fn add_implicit(&mut self) {
        let members = self.members.clone();
        for (group, users, origin) in members {
            for u in users {
                if !self.users.iter().any(|i| i.name == u) {
                    log_debug!("Adding implicit user '{u}' due to m line");
                    let mut i = Item::new(Kind::User, &u);
                    i.origin = origin.clone();
                    self.users.push(i);
                }
            }
            if !self.users.iter().any(|i| i.name == group)
                && !self.groups.iter().any(|i| i.name == group)
            {
                log_debug!("Adding implicit group '{group}' due to m line");
                let mut i = Item::new(Kind::Group, &group);
                i.origin = origin.clone();
                self.groups.push(i);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGroup {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub gecos: String,
    pub home: String,
    pub shell: String,
    pub locked: bool,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub groups: Vec<NewGroup>,
    pub users: Vec<NewUser>,
    /// Existing groups and the members to add to them.
    pub add_members: Vec<(String, Vec<String>)>,
    pub errors: Vec<String>,
}

impl Plan {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.users.is_empty() && self.add_members.is_empty()
    }
}

/// The ids a file's owner gives (uid, gid), for `u name /path` and
/// `g name /path`.
pub type IdFromPath<'a> = &'a dyn Fn(&str) -> Option<(u32, u32)>;

struct Work {
    name: String,
    uid: Option<u32>,
    gid: Option<u32>,
    strict: bool,
    uid_path: Option<String>,
    gid_path: Option<String>,
    todo_group: bool,
}

impl Work {
    fn of(i: &Item) -> Work {
        Work {
            name: i.name.clone(),
            uid: i.uid,
            gid: i.gid,
            strict: i.gid_strict,
            uid_path: i.uid_path.clone(),
            gid_path: i.gid_path.clone(),
            todo_group: false,
        }
    }
}

struct State<'a> {
    db: &'a Db,
    ranges: Vec<(u32, u32)>,
    todo_uids: HashMap<u32, String>,
    todo_gids: HashMap<u32, String>,
    search: u32,
    id_from_path: IdFromPath<'a>,
    plan: Plan,
}

impl State<'_> {
    fn in_ranges(&self, id: u32) -> bool {
        self.ranges.iter().any(|&(a, b)| a <= id && id <= b)
    }

    /// The next lower id in the ranges, below the last one handed out.
    fn next_lower(&mut self) -> Option<u32> {
        if self.search == 0 {
            return None;
        }
        let best = self.ranges.iter().filter_map(|&(a, b)| {
            let c = b.min(self.search - 1);
            (c >= a).then_some(c)
        });
        let c = best.max()?;
        self.search = c;
        Some(c)
    }

    fn uid_is_ok(&self, uid: u32, name: &str, check_with_gid: bool) -> bool {
        if self.todo_uids.contains_key(&uid) {
            return false;
        }
        if check_with_gid && self.todo_gids.get(&uid).is_some_and(|g| g != name) {
            return false;
        }
        if self.db.name_by_uid(uid).is_some() {
            return false;
        }
        if check_with_gid && self.db.name_by_gid(uid).is_some_and(|g| g != name) {
            return false;
        }
        true
    }

    fn gid_is_ok(&self, gid: u32, name: &str, check_with_uid: bool) -> bool {
        if self.todo_gids.contains_key(&gid) {
            return false;
        }
        if check_with_uid && self.todo_uids.get(&gid).is_some_and(|u| u != name) {
            return false;
        }
        if self.db.name_by_gid(gid).is_some() {
            return false;
        }
        if check_with_uid && self.db.name_by_uid(gid).is_some_and(|u| u != name) {
            return false;
        }
        true
    }

    fn path_ids(&self, w: &Work) -> (Option<u32>, Option<u32>) {
        let g = w.gid_path.as_deref().and_then(|p| (self.id_from_path)(p));
        let u = w.uid_path.as_deref().and_then(|p| (self.id_from_path)(p));
        let uid = u.map(|(u, _)| u).or(g.map(|(_, g)| g));
        let gid = g.map(|(_, g)| g).or(u.map(|(_, g)| g));
        (uid, gid)
    }

    fn add_group(&mut self, w: &mut Work) -> Result<(), String> {
        // A group of that name that exists is used as it is.
        if let Some(g) = self.db.gid_by_name(&w.name) {
            log_debug!("Group {} already exists.", w.name);
            w.gid = Some(g);
            return Ok(());
        }
        if let Some(g) = w.gid {
            let free = self.gid_is_ok(g, &w.name, false);
            if w.strict {
                return if free {
                    Err(format!(
                        "Failed to create {}: please create GID {g}",
                        w.name
                    ))
                } else {
                    Ok(())
                };
            }
            if !free {
                log_info!("Suggested group ID {g} for {} already used.", w.name);
                w.gid = None;
            }
        }
        if w.gid.is_none()
            && let Some(u) = w.uid
            && self.gid_is_ok(u, &w.name, true)
        {
            w.gid = Some(u);
        }
        if w.gid.is_none()
            && let (_, Some(c)) = self.path_ids(w)
        {
            if c > 0 && self.in_ranges(c) && self.gid_is_ok(c, &w.name, true) {
                w.gid = Some(c);
            } else {
                log_debug!("Group ID {c} of file not suitable for {}.", w.name);
            }
        }
        if w.gid.is_none() {
            loop {
                let c = self
                    .next_lower()
                    .ok_or_else(|| format!("No free group ID available for {}.", w.name))?;
                if self.gid_is_ok(c, &w.name, true) {
                    w.gid = Some(c);
                    break;
                }
            }
        }
        let gid = w.gid.unwrap();
        self.todo_gids.insert(gid, w.name.clone());
        w.todo_group = true;
        log_info!("Creating group '{}' with GID {gid}.", w.name);
        self.plan.groups.push(NewGroup {
            name: w.name.clone(),
            gid,
            members: Vec::new(),
        });
        Ok(())
    }

    fn add_user(&mut self, w: &mut Work, item: &Item) -> Result<(), String> {
        if let Some(u) = self.db.uid_by_name(&w.name) {
            log_debug!("User {} already exists.", w.name);
            w.uid = Some(u);
            return Ok(());
        }
        if let Some(u) = w.uid
            && !self.uid_is_ok(u, &w.name, !w.strict)
        {
            log_info!("Suggested user ID {u} for {} already used.", w.name);
            w.uid = None;
        }
        if w.uid.is_none()
            && let (Some(c), _) = self.path_ids(w)
        {
            if c > 0 && self.in_ranges(c) && self.uid_is_ok(c, &w.name, true) {
                w.uid = Some(c);
            } else {
                log_debug!("User ID {c} of file not suitable for {}.", w.name);
            }
        }
        if w.uid.is_none()
            && let Some(g) = w.gid
            && self.uid_is_ok(g, &w.name, true)
        {
            w.uid = Some(g);
        }
        if w.uid.is_none() {
            loop {
                let c = self
                    .next_lower()
                    .ok_or_else(|| format!("No free user ID available for {}.", w.name))?;
                if self.uid_is_ok(c, &w.name, true) {
                    w.uid = Some(c);
                    break;
                }
            }
        }
        let (uid, gid) = (w.uid.unwrap(), w.gid.unwrap());
        self.todo_uids.insert(uid, w.name.clone());
        let gecos = item.description.clone().unwrap_or_default();
        log_info!(
            "Creating user '{}' ({}) with UID {uid} and GID {gid}.",
            w.name,
            if gecos.is_empty() { "n/a" } else { &gecos }
        );
        self.plan.users.push(NewUser {
            name: w.name.clone(),
            uid,
            gid,
            gecos,
            home: item.home.clone().unwrap_or_else(|| "/".into()),
            shell: item.shell.clone().unwrap_or_else(|| {
                if uid == 0 {
                    "/bin/sh".into()
                } else {
                    "/usr/sbin/nologin".into()
                }
            }),
            locked: item.locked,
        });
        Ok(())
    }
}

/// Work out what to create. Errors for single entries are collected in
/// [`Plan::errors`]; the rest is still planned.
pub fn plan(cfg: &Config, db: &Db, id_from_path: IdFromPath) -> Plan {
    let ranges = if cfg.ranges.is_empty() {
        vec![DEFAULT_RANGE]
    } else {
        cfg.ranges.clone()
    };
    let mut st = State {
        db,
        ranges,
        todo_uids: HashMap::new(),
        todo_gids: HashMap::new(),
        search: u32::MAX,
        id_from_path,
        plan: Plan::default(),
    };

    // name -> (gid, created now)
    let mut group_res: HashMap<String, (u32, bool)> = HashMap::new();
    for g in &cfg.groups {
        let mut w = Work::of(g);
        match st.add_group(&mut w) {
            Ok(()) => {
                group_res.insert(w.name.clone(), (w.gid.unwrap(), w.todo_group));
            }
            Err(e) => st.plan.errors.push(format!("{}: {e}", g.origin)),
        }
    }

    let mut user_groups: HashMap<String, (u32, bool)> = HashMap::new();
    for u in &cfg.users {
        let mut w = Work::of(u);
        // An existing user is left alone entirely (systemd would still
        // create a missing group of the same name).
        if let Some(uid) = db.uid_by_name(&u.name) {
            log_debug!("User {} already exists.", u.name);
            let gid = db.files[&crate::db::Which::Passwd]
                .lines
                .iter()
                .filter_map(|l| dynamod_sdconf::accounts::parse_passwd_line(l))
                .find(|e| e.uid == uid && e.name == u.name)
                .map(|e| e.gid)
                .unwrap_or(uid);
            user_groups.insert(u.name.clone(), (gid, false));
            continue;
        }
        let r = (|| -> Result<(), String> {
            if w.gid.is_none() {
                let key = u.group_name.as_deref().unwrap_or(&u.name);
                let queued = group_res
                    .get(key)
                    .or_else(|| u.group_name.as_ref().and_then(|g| user_groups.get(g)));
                match (queued, &u.group_name) {
                    (Some(&(gid, true)), _) => {
                        w.gid = Some(gid);
                        w.strict = true;
                    }
                    (_, Some(gn)) => {
                        let gid = st
                            .db
                            .gid_by_name(gn)
                            .or_else(|| group_res.get(gn).map(|r| r.0))
                            .ok_or_else(|| format!("Group {gn} not found."))?;
                        w.gid = Some(gid);
                        w.strict = true;
                    }
                    _ => st.add_group(&mut w)?,
                }
            } else {
                st.add_group(&mut w)?;
            }
            st.add_user(&mut w, u)
        })();
        match r {
            Ok(()) => {
                user_groups.insert(w.name.clone(), (w.gid.unwrap(), w.todo_group));
            }
            Err(e) => st.plan.errors.push(format!("{}: {e}", u.origin)),
        }
    }

    for (group, users, origin) in &cfg.members {
        if let Some(ng) = st.plan.groups.iter_mut().find(|g| g.name == *group) {
            for u in users {
                if !ng.members.contains(u) {
                    ng.members.push(u.clone());
                }
            }
        } else if let Some(existing) = db.members(group) {
            let add: Vec<String> = users
                .iter()
                .filter(|u| !existing.contains(u))
                .cloned()
                .collect();
            if !add.is_empty() {
                log_info!(
                    "Adding {} to group '{group}'.",
                    add.iter()
                        .map(|u| format!("'{u}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                st.plan.add_members.push((group.clone(), add));
            }
        } else {
            st.plan.errors.push(format!(
                "{origin}: Group '{group}' does not exist, cannot add members to it."
            ));
        }
    }
    st.plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_line;
    use dynamod_sdconf::Root;
    use dynamod_sdconf::specifier::Specifiers;

    fn cfg(text: &str) -> Config {
        let spec = Specifiers::new(&Root::host(), false);
        let mut c = Config::default();
        for (n, l) in text.lines().enumerate() {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let mut i = parse_line(l, &spec).unwrap();
            i.origin = Origin {
                file: "t.conf".into(),
                line: n + 1,
            };
            assert!(c.add(i).is_none());
        }
        c.add_implicit();
        c
    }

    fn base() -> Db {
        Db::from_texts(
            "root:x:0:0:root:/root:/bin/bash\nbin:x:1:1:bin:/dev/null:/usr/bin/false\nnobody:x:65534:65534::/:/bin/false\n",
            "root:x:0:\nbin:x:1:daemon\nsys:x:2:\ntty:x:5:\nusers:x:999:\nwheel:x:97:\nnogroup:x:65534:\n",
            "",
            "",
        )
    }

    fn no_paths(_: &str) -> Option<(u32, u32)> {
        None
    }

    fn run(text: &str, db: &Db) -> Plan {
        plan(&cfg(text), db, &no_paths)
    }

    #[test]
    fn fixed_ids() {
        let p = run(
            "g sshd 50\nu sshd 50:50 \"sshd PrivSep\" /var/lib/sshd /usr/bin/false\n",
            &base(),
        );
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert_eq!(
            p.groups,
            vec![NewGroup {
                name: "sshd".into(),
                gid: 50,
                members: vec![]
            }]
        );
        assert_eq!(
            p.users,
            vec![NewUser {
                name: "sshd".into(),
                uid: 50,
                gid: 50,
                gecos: "sshd PrivSep".into(),
                home: "/var/lib/sshd".into(),
                shell: "/usr/bin/false".into(),
                locked: false
            }]
        );
    }

    #[test]
    fn user_without_gid_gets_a_group_of_its_own() {
        let p = run("u chrony 53 \"chrony daemon\"\n", &base());
        assert_eq!(
            p.groups[0],
            NewGroup {
                name: "chrony".into(),
                gid: 53,
                members: vec![]
            }
        );
        assert_eq!((p.users[0].uid, p.users[0].gid), (53, 53));
        assert_eq!(
            (p.users[0].home.as_str(), p.users[0].shell.as_str()),
            ("/", "/usr/sbin/nologin")
        );
        // uid:gid with an existing gid: no group of the user's name.
        let p = run("u joe 1000:999 \"joe\" /home/joe /bin/bash\n", &base());
        assert!(p.groups.is_empty());
        assert_eq!((p.users[0].uid, p.users[0].gid), (1000, 999));
        // uid:groupname
        let p = run(
            "g libvirt-qemu 64\nu libvirt-qemu 64:libvirt-qemu \"libvirt QEMU user\" - -\n",
            &base(),
        );
        assert_eq!((p.users[0].uid, p.users[0].gid), (64, 64));
        assert_eq!(p.groups.len(), 1);
    }

    #[test]
    fn dynamic_allocation_avoids_used_ids() {
        let db = Db::from_texts(
            "root:x:0:0::/root:/bin/sh\ntaken:x:999:999::/:/bin/false\n",
            "root:x:0:\ntaken:x:999:\nother:x:998:\n",
            "",
            "",
        );
        let p = run("u a -\nu b -\ng c -\n", &db);
        // g lines first: c gets 997 (998 and 999 are used); then a and b
        // each get a user and group pair below that.
        assert_eq!(
            p.groups
                .iter()
                .map(|g| (g.name.as_str(), g.gid))
                .collect::<Vec<_>>(),
            [("c", 997), ("a", 996), ("b", 995)]
        );
        assert_eq!(
            p.users
                .iter()
                .map(|u| (u.name.as_str(), u.uid, u.gid))
                .collect::<Vec<_>>(),
            [("a", 996, 996), ("b", 995, 995)]
        );
        // Ranges.
        let p = run("r - 500-501\nu a -\nu b -\nu c -\n", &db);
        assert_eq!(
            p.users.iter().map(|u| u.uid).collect::<Vec<_>>(),
            [501, 500]
        );
        assert!(p.errors[0].contains("No free group ID available for c"));
    }

    #[test]
    fn suggested_id_in_use_is_replaced() {
        let p = run("u newbie 1 \"x\"\n", &base());
        // uid 1 belongs to bin: another is allocated.
        assert_eq!(p.users[0].uid, 999 - 1);
        assert_eq!(p.users[0].gid, 998);
    }

    #[test]
    fn existing_accounts_are_never_changed() {
        let db = Db::from_texts(
            "root:x:0:0::/root:/bin/sh\nsshd:x:22:22::/old:/bin/sh\n",
            "root:x:0:\nsshd:x:22:\n",
            "",
            "",
        );
        let p = run(
            "g sshd 50\nu sshd 50:50 \"sshd PrivSep\" /var/lib/sshd /usr/bin/false\n",
            &db,
        );
        assert!(p.is_empty(), "{p:?}");
        assert!(p.errors.is_empty());
    }

    #[test]
    fn missing_explicit_group_is_an_error() {
        let p = run(
            "u lp 9:9 \"Print\"\nu ok 20\n",
            &Db::from_texts("", "", "", ""),
        );
        assert!(p.errors[0].contains("please create GID 9"));
        assert_eq!(p.users.len(), 1);
        let p = run("u q 70:nosuchgroup\n", &base());
        assert!(p.errors[0].contains("Group nosuchgroup not found"));
    }

    #[test]
    fn members() {
        let p = run(
            "g brlapi 91\nu brltty 90:90 \"Braille\"\ng brltty 90\nm brltty tty\nm brltty brlapi\nm brltty tty\nm joe wheel\n",
            &base(),
        );
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        assert_eq!(
            p.groups
                .iter()
                .find(|g| g.name == "brlapi")
                .unwrap()
                .members,
            ["brltty"]
        );
        // An m line for a user nobody declared creates it.
        assert!(p.users.iter().any(|u| u.name == "joe"));
        assert_eq!(
            p.add_members,
            vec![
                ("tty".into(), vec!["brltty".into()]),
                ("wheel".into(), vec!["joe".into()])
            ]
        );
        // Already a member: nothing to do.
        let db = Db::from_texts("joe:x:1000:999::/:/bin/sh\n", "wheel:x:97:joe\n", "", "");
        assert!(run("m joe wheel\n", &db).is_empty());
        // An m line for a group nobody declared creates it.
        let p = run("m joe newgroup\n", &db);
        assert_eq!(p.groups[0].name, "newgroup");
        assert_eq!(p.groups[0].members, ["joe"]);
    }

    #[test]
    fn conflicts_keep_the_first_line() {
        let spec = Specifiers::new(&Root::host(), false);
        let mut c = Config::default();
        assert!(c.add(parse_line("u a 10", &spec).unwrap()).is_none());
        assert!(c.add(parse_line("u a 10", &spec).unwrap()).is_none());
        assert!(
            c.add(parse_line("u a 11", &spec).unwrap())
                .unwrap()
                .contains("Conflict")
        );
        assert_eq!(c.users.len(), 1);
        assert_eq!(c.users[0].uid, Some(10));
    }

    #[test]
    fn ids_from_paths() {
        let from = |p: &str| {
            if p == "/usr/bin/authd" {
                Some((420, 421))
            } else {
                None
            }
        };
        let p = plan(
            &cfg("u _authd /usr/bin/authd \"Authorization user\"\n"),
            &base(),
            &from,
        );
        assert_eq!((p.users[0].uid, p.users[0].gid), (420, 421));
    }

    #[test]
    fn root_gets_a_shell() {
        let p = run(
            "u root 0 \"Superuser\" /root\n",
            &Db::from_texts("", "", "", ""),
        );
        assert_eq!(p.users[0].shell, "/bin/sh");
    }
}
