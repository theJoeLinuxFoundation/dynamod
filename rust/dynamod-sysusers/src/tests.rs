//! End-to-end tests on a temporary root.
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;

const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash
bin:x:1:1:bin:/dev/null:/usr/bin/false
messagebus:x:18:18:D-Bus Message Daemon User:/run/dbus:/usr/bin/false
nobody:x:65534:65534:Unprivileged User:/dev/null:/usr/bin/false
";
const GROUP: &str = "root:x:0:
bin:x:1:daemon
sys:x:2:
tty:x:5:
audio:x:11:
messagebus:x:18:
systemd-journal:x:23:
users:x:999:
wheel:x:97:
nogroup:x:65534:
";

struct T {
    dir: PathBuf,
}

impl T {
    fn new(name: &str) -> T {
        let dir = std::env::temp_dir().join(format!("sysusers-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("etc")).unwrap();
        fs::create_dir_all(dir.join("usr/lib/sysusers.d")).unwrap();
        let t = T {
            dir: fs::canonicalize(&dir).unwrap(),
        };
        t.write("etc/passwd", PASSWD);
        t.write("etc/group", GROUP);
        let shadow: String = PASSWD
            .lines()
            .map(|l| format!("{}:!:19000:0:99999:7:::\n", l.split(':').next().unwrap()))
            .collect();
        let gshadow: String = GROUP
            .lines()
            .map(|l| format!("{}:!::\n", l.split(':').next().unwrap()))
            .collect();
        t.write("etc/shadow", &shadow);
        t.write("etc/gshadow", &gshadow);
        fs::set_permissions(t.p("etc/shadow"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(t.p("etc/gshadow"), fs::Permissions::from_mode(0o600)).unwrap();
        t
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
    fn write(&self, rel: &str, s: &str) {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }
    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.p(rel)).unwrap()
    }
    fn conf(&self, name: &str, s: &str) {
        self.write(&format!("usr/lib/sysusers.d/{name}"), s);
    }
    fn run(&self, extra: &[&str]) -> i32 {
        let mut a: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        a.push(format!("--root={}", self.dir.display()));
        run("dynamod-sysusers", &a)
    }
    fn state(&self) -> Vec<(String, u64, i64, i64, u32)> {
        [
            "etc/passwd",
            "etc/group",
            "etc/shadow",
            "etc/gshadow",
            "etc/passwd-",
            "etc/group-",
            "etc/shadow-",
            "etc/gshadow-",
        ]
        .iter()
        .filter_map(|f| {
            fs::metadata(self.p(f))
                .ok()
                .map(|m| (f.to_string(), m.ino(), m.mtime(), m.mtime_nsec(), m.mode()))
        })
        .collect()
    }
}

impl Drop for T {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn today() -> i64 {
    lstchg(None, std::time::SystemTime::now())
}

#[test]
fn arguments() {
    let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let Ok(Action::Run(o)) = a(&["--root", "/mnt", "-n", "x.conf"]) else {
        panic!()
    };
    assert_eq!(
        (o.root, o.dry_run, o.files),
        (
            Some(PathBuf::from("/mnt")),
            true,
            vec!["x.conf".to_string()]
        )
    );
    let Ok(Action::Run(o)) = a(&["--root=/r", "--cat-config"]) else {
        panic!()
    };
    assert_eq!((o.root, o.cat), (Some(PathBuf::from("/r")), Some(false)));
    assert!(a(&["--replace=/x"]).is_err());
    assert!(a(&["--root"]).is_err());
    assert_eq!(a(&["-h"]), Ok(Action::Help));
}

#[test]
fn source_date_epoch() {
    assert_eq!(
        lstchg(Some("1700000000"), std::time::SystemTime::now()),
        19675
    );
    assert_eq!(
        lstchg(
            Some("junk"),
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(86_400 * 3)
        ),
        3
    );
}

#[test]
fn creates_accounts_like_joe_packages_declare_them() {
    let t = T::new("joe");
    t.conf(
        "openssh.conf",
        "g sshd 50\nu sshd 50:50 \"sshd PrivSep\" /var/lib/sshd /usr/bin/false\n",
    );
    t.conf("brltty.conf", "g brltty 90\nu brltty 90:90 \"Braille Device Daemon\" /var/lib/brltty\ng brlapi 91\nm brltty tty\nm brltty audio\nm brltty brlapi\n");
    t.conf(
        "joe-live.conf",
        "u liveuser 1000:999 \"Live session user\" /home/liveuser /bin/bash\nm liveuser wheel\n",
    );
    t.conf("dynamic.conf", "u! radvd - \"radvd daemon\"\n");
    assert_eq!(t.run(&[]), 0);
    let d = today();
    assert_eq!(
        t.read("etc/passwd"),
        format!(
            "{PASSWD}brltty:x:90:90:Braille Device Daemon:/var/lib/brltty:/usr/sbin/nologin\n\
             radvd:x:998:998:radvd daemon:/:/usr/sbin/nologin\n\
             liveuser:x:1000:999:Live session user:/home/liveuser:/bin/bash\n\
             sshd:x:50:50:sshd PrivSep:/var/lib/sshd:/usr/bin/false\n"
        )
    );
    let group = t.read("etc/group");
    assert!(group.contains("\ntty:x:5:brltty\n"), "{group}");
    assert!(group.contains("\naudio:x:11:brltty\n"));
    assert!(group.contains("\nwheel:x:97:liveuser\n"));
    assert!(
        group.ends_with(
            "nogroup:x:65534:\nbrltty:x:90:\nbrlapi:x:91:brltty\nsshd:x:50:\nradvd:x:998:\n"
        ),
        "{group}"
    );
    let shadow = t.read("etc/shadow");
    assert!(
        shadow.contains(&format!("\nsshd:!*:{d}::::::\n")),
        "{shadow}"
    );
    assert!(
        shadow.contains(&format!("\nradvd:!*:{d}:::::1:\n")),
        "u! locks the account"
    );
    let gshadow = t.read("etc/gshadow");
    assert!(gshadow.contains("\ntty:!::brltty\n"), "{gshadow}");
    assert!(gshadow.contains("\nbrlapi:!*::brltty\n"));
    assert!(gshadow.contains("\nsshd:!*::\n"));
    // Modes are kept; the old versions are kept as FILE-.
    assert_eq!(
        fs::metadata(t.p("etc/shadow")).unwrap().mode() & 0o7777,
        0o600
    );
    assert_eq!(
        fs::metadata(t.p("etc/passwd")).unwrap().mode() & 0o7777,
        0o644
    );
    assert_eq!(t.read("etc/passwd-"), PASSWD);
    assert!(t.p("etc/.pwd.lock").exists());
    // No temporary files are left behind.
    let leftovers: Vec<_> = fs::read_dir(t.p("etc"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".#"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Running again changes nothing at all.
    let before = t.state();
    assert_eq!(t.run(&[]), 0);
    assert_eq!(before, t.state());
}

#[test]
fn existing_accounts_are_kept_as_they_are() {
    let t = T::new("keep");
    t.write(
        "etc/passwd",
        &format!("{PASSWD}sshd:x:22:22:old:/old:/bin/sh\n"),
    );
    t.write("etc/group", &format!("{GROUP}sshd:x:22:\n"));
    t.conf(
        "openssh.conf",
        "g sshd 50\nu sshd 50:50 \"sshd PrivSep\" /var/lib/sshd /usr/bin/false\n",
    );
    let before = t.state();
    assert_eq!(t.run(&[]), 0);
    assert_eq!(before, t.state());
    assert!(
        t.read("etc/passwd")
            .contains("sshd:x:22:22:old:/old:/bin/sh\n")
    );
}

#[test]
fn missing_files_are_created() {
    let t = T::new("missing");
    for f in ["etc/passwd", "etc/group", "etc/shadow", "etc/gshadow"] {
        fs::remove_file(t.p(f)).unwrap();
    }
    fs::remove_dir(t.p("etc")).unwrap();
    t.conf(
        "base.conf",
        "u root 0 \"Super User\" /root\ng wheel 97\nm root wheel\n",
    );
    assert_eq!(t.run(&[]), 0);
    assert_eq!(
        t.read("etc/passwd"),
        "root:x:0:0:Super User:/root:/bin/sh\n"
    );
    assert_eq!(t.read("etc/group"), "wheel:x:97:root\nroot:x:0:\n");
    assert_eq!(
        fs::metadata(t.p("etc/shadow")).unwrap().mode() & 0o7777,
        0o000
    );
    assert_eq!(
        fs::metadata(t.p("etc/group")).unwrap().mode() & 0o7777,
        0o644
    );
    assert!(!t.p("etc/passwd-").exists());
}

#[test]
fn invalid_configuration_changes_nothing() {
    let t = T::new("invalid");
    t.conf("a.conf", "u good 300\n");
    t.conf("b.conf", "u bad.name 301\n");
    let before = t.state();
    assert_eq!(t.run(&[]), 1);
    assert_eq!(before, t.state());
}

#[test]
fn entry_errors_do_not_stop_the_others() {
    let t = T::new("partial");
    t.conf("a.conf", "u lp 9:77 \"Print\"\nu ok 300\n");
    assert_eq!(t.run(&[]), 1);
    let passwd = t.read("etc/passwd");
    assert!(passwd.contains("ok:x:300:300::/:/usr/sbin/nologin\n"));
    assert!(!passwd.contains("lp:"));
}

#[test]
fn nis_entries_stay_last() {
    let t = T::new("nis");
    t.write("etc/passwd", &format!("{PASSWD}+@netusers::::::\n"));
    t.write("etc/group", &format!("{GROUP}+\n"));
    t.conf("a.conf", "u svc 300\n");
    assert_eq!(t.run(&[]), 0);
    assert!(
        t.read("etc/passwd")
            .ends_with("svc:x:300:300::/:/usr/sbin/nologin\n+@netusers::::::\n")
    );
    assert!(t.read("etc/group").ends_with("svc:x:300:\n+\n"));
}

#[test]
fn dry_run_and_cat_config() {
    let t = T::new("dry");
    t.conf("a.conf", "u svc 300\n");
    fs::create_dir_all(t.p("etc/sysusers.d")).unwrap();
    t.conf("masked.conf", "u masked 301\n");
    symlink("/dev/null", t.p("etc/sysusers.d/masked.conf")).unwrap();
    let before = t.state();
    assert_eq!(t.run(&["--dry-run"]), 0);
    assert_eq!(before, t.state());
    assert!(!t.p("etc/.pwd.lock").exists());
    assert_eq!(t.run(&["--cat-config"]), 0);
    // The masked file is not read.
    assert_eq!(t.run(&[]), 0);
    assert!(!t.read("etc/passwd").contains("masked"));
    assert!(t.read("etc/passwd").contains("svc:x:300"));
}

#[test]
fn explicit_files_and_inline_lines() {
    let t = T::new("explicit");
    t.conf("a.conf", "u from-a 300\n");
    t.conf("b.conf", "u from-b 301\n");
    assert_eq!(t.run(&["b.conf"]), 0);
    let p = t.read("etc/passwd");
    assert!(p.contains("from-b") && !p.contains("from-a"));
    assert_eq!(t.run(&["--inline", "g inl 302", "u inl 302:302"]), 0);
    assert!(
        t.read("etc/passwd")
            .contains("inl:x:302:302::/:/usr/sbin/nologin")
    );
}

#[test]
fn ids_from_file_owners() {
    let t = T::new("idpath");
    let md = fs::metadata(&t.dir).unwrap();
    t.write("usr/bin/helper", "");
    t.conf("a.conf", "u helper /usr/bin/helper\n");
    assert_eq!(t.run(&[]), 0);
    let line = t
        .read("etc/passwd")
        .lines()
        .find(|l| l.starts_with("helper:"))
        .unwrap()
        .to_string();
    // Our own uid is outside 1-999 when running as a normal user, so then
    // an id is allocated instead (999 belongs to "users").
    if (1..=999).contains(&md.uid()) {
        assert!(line.starts_with(&format!("helper:x:{}:", md.uid())));
    } else {
        assert!(line.starts_with("helper:x:998:998:"), "{line}");
    }
}
