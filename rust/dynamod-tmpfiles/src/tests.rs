//! End-to-end tests: configuration in a temporary root, run as the program
//! runs, results checked on disk.
use std::collections::BTreeMap;
use std::fs::{self, File, FileTimes};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::*;

struct T {
    dir: PathBuf,
}

fn euid() -> u32 {
    // SAFETY: geteuid() cannot fail.
    unsafe { libc::geteuid() }
}

impl T {
    fn new(name: &str) -> T {
        // SAFETY: umask() cannot fail. run() sets it to 0 for the whole
        // process; do the same before writing fixtures so modes agree.
        unsafe { libc::umask(0) };
        let dir = std::env::temp_dir().join(format!("tmpfiles-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("usr/lib/tmpfiles.d")).unwrap();
        fs::create_dir_all(dir.join("etc")).unwrap();
        fs::write(
            dir.join("etc/passwd"),
            "root:x:0:0::/root:/bin/sh\nsshd:x:50:50::/var/lib/sshd:/usr/bin/false\n",
        )
        .unwrap();
        fs::write(
            dir.join("etc/group"),
            "root:x:0:\nsys:x:2:\nsshd:x:50:\nsystemd-journal:x:23:\n",
        )
        .unwrap();
        T {
            dir: fs::canonicalize(&dir).unwrap(),
        }
    }
    fn p(&self, rel: &str) -> PathBuf {
        self.dir.join(rel.trim_start_matches('/'))
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    fn conf(&self, name: &str, text: &str) {
        self.write(&format!("usr/lib/tmpfiles.d/{name}"), text);
    }
    fn run(&self, args: &[&str]) -> i32 {
        let mut a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        a.push(format!("--root={}", self.dir.display()));
        run("dynamod-tmpfiles", &a)
    }
    fn mode(&self, rel: &str) -> u32 {
        fs::symlink_metadata(self.p(rel)).unwrap().mode() & 0o7777
    }
    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.p(rel)).unwrap()
    }
    fn exists(&self, rel: &str) -> bool {
        fs::symlink_metadata(self.p(rel)).is_ok()
    }
    fn age(&self, rel: &str, by: Duration) {
        let t = SystemTime::now() - by;
        let f = File::open(self.p(rel)).unwrap();
        f.set_times(FileTimes::new().set_accessed(t).set_modified(t))
            .unwrap();
    }
    /// Everything under the root: path -> (mode, mtime, content or link).
    fn snapshot(&self) -> BTreeMap<String, (u32, i64, i64, String)> {
        fn walk(base: &Path, p: &Path, out: &mut BTreeMap<String, (u32, i64, i64, String)>) {
            let md = fs::symlink_metadata(p).unwrap();
            let what = if md.file_type().is_symlink() {
                format!("-> {}", fs::read_link(p).unwrap().display())
            } else if md.is_file() {
                fs::read_to_string(p).unwrap_or_default()
            } else {
                String::new()
            };
            out.insert(
                p.strip_prefix(base).unwrap().display().to_string(),
                (md.mode(), md.mtime(), md.mtime_nsec(), what),
            );
            if md.is_dir() {
                for e in fs::read_dir(p).unwrap() {
                    walk(base, &e.unwrap().path(), out);
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.dir, &self.dir, &mut out);
        out
    }
}

impl Drop for T {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn arguments() {
    let a = |v: &[&str]| parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let Ok(Action::Run(o)) = a(&[
        "--create",
        "--exclude-prefix=/dev",
        "--exclude-prefix",
        "/proc",
        "-E",
        "--root",
        "/mnt",
        "x.conf",
    ]) else {
        panic!()
    };
    assert_eq!(o.ops, OP_CREATE);
    assert_eq!(
        o.exclude,
        ["/dev", "/proc", "/dev", "/proc", "/run", "/sys"]
    );
    assert_eq!(o.root, Some(PathBuf::from("/mnt")));
    assert_eq!(o.files, ["x.conf"]);
    assert_eq!(a(&["--help"]), Ok(Action::Help));
    assert_eq!(a(&["--version"]), Ok(Action::Version));
    assert!(a(&["--user"]).is_err());
    assert!(a(&["--bogus"]).is_err());
    assert!(a(&["--root"]).is_err());
    let Ok(Action::Run(o)) = a(&["--create", "--remove", "--boot", "-n", "--clean"]) else {
        panic!()
    };
    assert_eq!(
        (o.ops, o.boot, o.dry_run),
        (OP_CREATE | OP_REMOVE | OP_CLEAN, true, true)
    );
}

#[test]
fn needs_a_command() {
    let t = T::new("nocmd");
    assert_eq!(t.run(&[]), 1);
}

#[test]
fn creates_and_is_idempotent() {
    let t = T::new("create");
    t.conf(
        "a.conf",
        "\
# comment
d /var/lib/app 0750 - - -
d /var/lib/app/sub
f /var/lib/app/state 0600 - - - hello
f /var/lib/app/state 0600 - - - hello
L /var/lib/app/link - - - - ../target
p /run/app.fifo 0620
w /var/lib/app/state - - - - J
z /var/lib/app/state 0640
d /var/log/journal 2755 - - -
",
    );
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(t.mode("var/lib/app"), 0o750);
    assert_eq!(t.mode("var/lib/app/sub"), 0o755);
    assert_eq!(t.mode("var/lib"), 0o755);
    // f writes only when it creates; w does not truncate; z adjusts.
    assert_eq!(t.read("var/lib/app/state"), "Jello");
    assert_eq!(t.mode("var/lib/app/state"), 0o640);
    assert_eq!(
        fs::read_link(t.p("var/lib/app/link")).unwrap(),
        PathBuf::from("../target")
    );
    assert!(
        fs::symlink_metadata(t.p("run/app.fifo"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    assert_eq!(t.mode("run/app.fifo"), 0o620);
    // The set-group-id bit, which mkdir() does not set.
    assert_eq!(t.mode("var/log/journal"), 0o2755);
    // A second run changes nothing (except the "w" line rewriting its byte).
    fs::write(t.p("var/lib/app/state"), "Jello").unwrap();
    let before = t.snapshot();
    assert_eq!(t.run(&["--create"]), 0);
    let mut after = t.snapshot();
    let mut b2 = before.clone();
    b2.remove("var/lib/app/state");
    after.remove("var/lib/app/state");
    assert_eq!(b2, after);
}

use std::os::unix::fs::FileTypeExt;

#[test]
fn ownership_failures_are_reported_and_the_run_goes_on() {
    let t = T::new("owner");
    t.conf(
        "a.conf",
        "d /var/lib/sshd 0700 root sys -\nd /var/lib/after 0711 - - -\nz /var/lib/after - sshd -\n",
    );
    let code = t.run(&["--create"]);
    let md = fs::metadata(t.p("var/lib/sshd")).unwrap();
    assert_eq!(
        md.mode() & 0o7777,
        0o700,
        "the mode is applied even when chown fails"
    );
    assert_eq!(t.mode("var/lib/after"), 0o711);
    if euid() == 0 {
        assert_eq!(code, 0);
        assert_eq!((md.uid(), md.gid()), (0, 2));
    } else {
        assert_eq!(code, 73);
    }
}

#[test]
fn ownership_of_own_ids_works() {
    let t = T::new("ownok");
    let (uid, gid) = {
        let md = fs::metadata(&t.dir).unwrap();
        (md.uid(), md.gid())
    };
    t.conf(
        "a.conf",
        &format!(
            "d /x 0700 {uid} {gid} -\nf /x/y 0600 {uid} {gid} -\nd /z :0700 :{uid} :{gid} -\n"
        ),
    );
    assert_eq!(t.run(&["--create"]), 0);
    let md = fs::metadata(t.p("x/y")).unwrap();
    assert_eq!((md.uid(), md.gid(), md.mode() & 0o777), (uid, gid, 0o600));
    // ":" applies to new nodes only.
    fs::set_permissions(t.p("z"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(t.mode("z"), 0o755);
}

#[test]
fn parents_are_created_by_their_own_lines_first() {
    let t = T::new("parents");
    // The child comes first, but the parent's line creates the parent.
    t.conf("a.conf", "d /srv/a/b 0755\nd /srv/a :0700\n");
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(t.mode("srv/a"), 0o700);
}

#[test]
fn boot_only_lines_and_allowed_failures() {
    let t = T::new("boot");
    t.write("tmp/.X0-lock", "");
    t.write("blocker", "a file where a directory would be\n");
    t.conf("a.conf", "r! /tmp/.X[0-9]*-lock\nd- /blocker/sub\nf /ok\n");
    assert_eq!(t.run(&["--create", "--remove"]), 0, "d- may fail");
    assert!(t.exists("tmp/.X0-lock"));
    assert!(t.exists("ok"));
    assert_eq!(t.run(&["--create", "--remove", "--boot"]), 0);
    assert!(!t.exists("tmp/.X0-lock"));
    // Without "-" the same failure counts.
    t.conf("a.conf", "d /blocker/sub\n");
    assert_eq!(t.run(&["--create"]), 73);
}

#[test]
fn replace_wrong_types() {
    let t = T::new("replace");
    t.write("var/lib/x", "a file\n");
    t.write("var/cache/y", "a file\n");
    t.conf("a.conf", "d /var/lib/x 0700\nd= /var/cache/y 0700\n");
    // Without "=" the wrong type is reported, not an error.
    assert_eq!(t.run(&["--create"]), 0);
    assert!(fs::metadata(t.p("var/lib/x")).unwrap().is_file());
    assert!(fs::metadata(t.p("var/cache/y")).unwrap().is_dir());
    // "=" also replaces parents in the way.
    t.write("srv/file", "x");
    t.conf("a.conf", "d= /srv/file/below\n");
    assert_eq!(t.run(&["--create"]), 0);
    assert!(fs::metadata(t.p("srv/file/below")).unwrap().is_dir());
}

#[test]
fn symlinks() {
    let t = T::new("links");
    t.write("etc/machine-id", "0123456789abcdef0123456789abcdef\n");
    t.write("var/run", "not a link\n");
    fs::create_dir_all(t.p("var/mail")).unwrap();
    t.conf(
        "a.conf",
        "L /var/lib/dbus/machine-id - - - - /etc/machine-id\nL+ /var/run - - - - ../run\nL+ /var/mail - - - - spool/mail\nL? /etc/absent - - - - /nonexistent\nL? /etc/present - - - - machine-id\nL /etc/os-release\n",
    );
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(
        fs::read_link(t.p("var/lib/dbus/machine-id")).unwrap(),
        PathBuf::from("/etc/machine-id")
    );
    assert_eq!(
        fs::read_link(t.p("var/run")).unwrap(),
        PathBuf::from("../run")
    );
    assert_eq!(
        fs::read_link(t.p("var/mail")).unwrap(),
        PathBuf::from("spool/mail")
    );
    assert!(!t.exists("etc/absent"));
    assert_eq!(
        fs::read_link(t.p("etc/present")).unwrap(),
        PathBuf::from("machine-id")
    );
    assert_eq!(
        fs::read_link(t.p("etc/os-release")).unwrap(),
        PathBuf::from("/usr/share/factory/etc/os-release")
    );
}

#[test]
fn paths_resolve_inside_the_root() {
    let t = T::new("inside");
    fs::create_dir_all(t.p("run/lock")).unwrap();
    fs::create_dir_all(t.p("var")).unwrap();
    symlink("/run/lock", t.p("var/lock")).unwrap();
    t.conf("a.conf", "d /var/lock/subsys 0700\n");
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(t.mode("run/lock/subsys"), 0o700);
}

#[test]
fn files_truncate_write_append() {
    let t = T::new("files");
    t.write("etc/a", "old content\n");
    t.write("etc/b", "abcdef");
    t.write("etc/c", "first\n");
    t.conf(
        "a.conf",
        "f+ /etc/a 0644 - - - new\nw /etc/b - - - - XY\nw+ /etc/c - - - - second\\n\nw+ /etc/c - - - - third\\n\nw /etc/missing - - - - x\nf~ /etc/d 0600 - - - aGVsbG8K\nf /etc/e 0600 - - - %%m\n",
    );
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(t.read("etc/a"), "new");
    assert_eq!(t.read("etc/b"), "XYcdef");
    assert_eq!(t.read("etc/c"), "first\nsecond\nthird\n");
    assert!(!t.exists("etc/missing"));
    assert_eq!(t.read("etc/d"), "hello\n");
    assert_eq!(t.read("etc/e"), "%m");
}

#[test]
fn remove_lines() {
    let t = T::new("remove");
    t.write("run/cups/a", "x");
    t.write("run/cups/sub/b", "x");
    t.write("var/tmp/junk/deep/c", "x");
    t.write("var/tmp/one.pid", "x");
    t.write("var/tmp/two.pid", "x");
    t.write("var/tmp/full/d", "x");
    t.conf(
        "a.conf",
        "D /run/cups 0755\nR /var/tmp/junk\nr /var/tmp/*.pid\nr /var/tmp/full\n",
    );
    // --create alone removes nothing.
    assert_eq!(t.run(&["--create"]), 0);
    assert!(t.exists("run/cups/a"));
    // r on a non-empty directory fails; the rest is done.
    assert_eq!(t.run(&["--remove", "--create"]), 73);
    assert!(t.exists("run/cups") && !t.exists("run/cups/a") && !t.exists("run/cups/sub"));
    assert!(!t.exists("var/tmp/junk"));
    assert!(!t.exists("var/tmp/one.pid") && !t.exists("var/tmp/two.pid"));
    assert!(t.exists("var/tmp/full/d"));
}

#[test]
fn cleaning_by_age() {
    let t = T::new("clean");
    for f in [
        "tmp/old",
        "tmp/new",
        "tmp/keep-me/old",
        "tmp/keepdir/old",
        "tmp/sub/old",
        "tmp/.X11-unix/old",
        "cache/k/a",
        "cache/k/l1/old",
    ] {
        t.write(f, "x");
    }
    t.write("tmp/sticky", "x");
    fs::set_permissions(t.p("tmp/sticky"), fs::Permissions::from_mode(0o1644)).unwrap();
    for f in [
        "tmp/old",
        "tmp/keep-me/old",
        "tmp/keepdir/old",
        "tmp/sub/old",
        "tmp/.X11-unix/old",
        "tmp/sticky",
        "cache/k/a",
        "cache/k/l1/old",
    ] {
        t.age(f, Duration::from_secs(7200));
    }
    for d in ["tmp/sub", "tmp/keepdir", "cache/k/l1"] {
        t.age(d, Duration::from_secs(7200));
    }
    t.conf(
        "a.conf",
        // Fixtures are new: their birth and change times are recent, so the
        // age is checked against access and modification times only.
        "d /tmp 1777 - - amAM:1h\nx /tmp/keep-me\nX /tmp/keepdir\nd /tmp/.X11-unix 1777 - - -\nd /cache/k - - - ~amAM:1h\ne /empty-me - - - 0\n",
    );
    t.write("empty-me/a", "x");
    t.write("empty-me/b/c", "x");
    assert_eq!(t.run(&["--clean"]), 0);
    assert!(!t.exists("tmp/old"));
    assert!(t.exists("tmp/new"));
    assert!(t.exists("tmp/sticky"), "sticky files are kept");
    assert!(
        t.exists("tmp/keep-me/old"),
        "x excludes the directory and its contents"
    );
    assert!(
        t.exists("tmp/keepdir") && !t.exists("tmp/keepdir/old"),
        "X keeps the directory, cleans inside"
    );
    assert!(
        !t.exists("tmp/sub"),
        "an old directory emptied by cleaning goes too"
    );
    assert!(
        t.exists("tmp/.X11-unix/old"),
        "a path with its own line is left to it"
    );
    assert!(
        t.exists("cache/k/a") && t.exists("cache/k/l1") && !t.exists("cache/k/l1/old"),
        "~ keeps the first level"
    );
    assert!(
        t.exists("empty-me") && !t.exists("empty-me/a") && !t.exists("empty-me/b"),
        "e with age 0 empties"
    );
    // Dry run removes nothing.
    t.write("tmp/old2", "x");
    t.age("tmp/old2", Duration::from_secs(7200));
    assert_eq!(t.run(&["--clean", "--dry-run"]), 0);
    assert!(t.exists("tmp/old2"));
}

#[test]
fn copies() {
    let t = T::new("copy");
    t.write("usr/share/factory/etc/pam.d/login", "auth\n");
    t.write("usr/share/factory/etc/pam.d/sub/x", "x\n");
    t.write("usr/share/skel/.bashrc", "skel\n");
    t.write("usr/share/skel/.profile", "profile\n");
    t.write("home/u/.bashrc", "mine\n");
    fs::create_dir_all(t.p("srv/empty")).unwrap();
    t.conf("a.conf", "C /etc/pam.d\nC /srv/empty - - - - /usr/share/skel\nC /home/u - - - - /usr/share/skel\nC /missing - - - - /does/not/exist\n");
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(t.read("etc/pam.d/login"), "auth\n");
    assert_eq!(t.read("etc/pam.d/sub/x"), "x\n");
    assert_eq!(t.read("srv/empty/.bashrc"), "skel\n");
    assert!(
        !t.exists("home/u/.profile"),
        "C skips a non-empty directory"
    );
    assert!(!t.exists("missing"));
    t.conf("a.conf", "C+ /home/u - - - - /usr/share/skel\n");
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(t.read("home/u/.profile"), "profile\n");
    assert_eq!(t.read("home/u/.bashrc"), "mine\n", "C+ does not overwrite");
}

#[test]
fn recursive_adjust() {
    let t = T::new("zrec");
    t.write("srv/data/a", "x");
    t.write("srv/data/b/c", "x");
    fs::set_permissions(t.p("srv/data/a"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(t.p("srv/data/b/c"), fs::Permissions::from_mode(0o755)).unwrap();
    t.conf("a.conf", "Z /srv/data ~0750\n");
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(t.mode("srv/data"), 0o750);
    assert_eq!(t.mode("srv/data/b"), 0o750);
    // "~": no execute bits where there were none.
    assert_eq!(t.mode("srv/data/a"), 0o640);
    assert_eq!(t.mode("srv/data/b/c"), 0o750);
}

#[test]
fn precedence_masking_and_invalid_lines() {
    let t = T::new("prec");
    t.conf("pkg.conf", "d /from-vendor\n");
    t.write("etc/tmpfiles.d/pkg.conf", "d /from-admin\n");
    t.conf("masked.conf", "d /masked\n");
    fs::create_dir_all(t.p("etc/tmpfiles.d")).unwrap();
    symlink("/dev/null", t.p("etc/tmpfiles.d/masked.conf")).unwrap();
    t.conf("bad.conf", "d relative\nd /good-after-bad\n");
    t.conf("run.conf", "d /run/x\nd /dev/y\n");
    assert_eq!(t.run(&["--create", "-E"]), 65);
    assert!(t.exists("from-admin") && !t.exists("from-vendor"));
    assert!(!t.exists("masked"));
    assert!(t.exists("good-after-bad"));
    assert!(!t.exists("run/x") && !t.exists("dev/y"));
    // An explicit file name is looked up; a path is used as it is.
    assert_eq!(t.run(&["--create", "run.conf"]), 0);
    assert!(t.exists("run/x"));
}

#[test]
fn unresolvable_specifiers_skip_the_line() {
    let t = T::new("spec");
    t.conf("a.conf", "d /var/log/journal/%m 2755\nd /other\n");
    assert_eq!(t.run(&["--create"]), 0);
    assert!(t.exists("other") && !t.exists("var/log/journal"));
    t.write("etc/machine-id", "0123456789abcdef0123456789abcdef\n");
    assert_eq!(t.run(&["--create"]), 0);
    assert_eq!(
        t.mode("var/log/journal/0123456789abcdef0123456789abcdef"),
        0o2755
    );
}

#[test]
fn dry_run_changes_nothing() {
    let t = T::new("dry");
    t.write("var/tmp/x.pid", "x");
    t.conf(
        "a.conf",
        "d /a/b/c 0700\nf /a/f - - - - x\nL /a/l - - - - t\nr /var/tmp/*.pid\nz /var/tmp 0700\n",
    );
    let before = t.snapshot();
    assert_eq!(t.run(&["--create", "--remove", "--dry-run"]), 0);
    assert_eq!(before, t.snapshot());
}

#[test]
fn purge_needs_files_and_removes_marked_paths() {
    let t = T::new("purge");
    t.conf("a.conf", "d$ /var/lib/pkg\nf /var/lib/keep\n");
    assert_eq!(t.run(&["--create"]), 0);
    assert!(t.exists("var/lib/pkg"));
    assert_eq!(t.run(&["--purge"]), 1);
    assert_eq!(t.run(&["--purge", "a.conf"]), 0);
    assert!(!t.exists("var/lib/pkg") && t.exists("var/lib/keep"));
}
