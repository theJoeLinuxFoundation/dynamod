//! Applying the configuration to the file system (inside the root).
//!
//! Failures are reported and the run goes on; [`Executor::failed`] tells the
//! caller to exit with status 73 at the end. A path that exists with the
//! wrong type is reported but is not a failure (unless `=` or `+` asks for
//! it to be replaced), as with systemd-tmpfiles.
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dynamod_sdconf::{Root, log_debug, log_error, log_info, log_notice, log_warning};

use crate::config::{ArrayRef, Config, Map};
use crate::glob::{self, Match};
use crate::item::{Age, AgeBy, Item, Kind};

pub const OP_CREATE: u8 = 1;
pub const OP_REMOVE: u8 = 2;
pub const OP_CLEAN: u8 = 4;
pub const OP_PURGE: u8 = 8;

const MAX_DEPTH: usize = 256;

// mode_t is not 32 bits everywhere (16 on 32-bit Android), hence the casts.
#[allow(clippy::unnecessary_cast)]
const S_IFMT: u32 = libc::S_IFMT as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFBLK: u32 = libc::S_IFBLK as u32;
#[allow(clippy::unnecessary_cast)]
const S_IFCHR: u32 = libc::S_IFCHR as u32;
#[allow(clippy::unnecessary_cast)]
const S_ISVTX: u32 = libc::S_ISVTX as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Creation {
    /// We created it.
    Normal,
    /// It was there.
    Existing,
    /// We replaced or truncated it.
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Excl {
    None,
    /// A line exists for this path (or `x` matches it): leave it alone.
    Skip,
    /// `X` matches it: do not remove it, but clean inside it.
    KeepSelf,
}

pub struct Executor<'a> {
    root: &'a Root,
    config: &'a Config,
    dry_run: bool,
    now: SystemTime,
    euid: u32,
    done: HashMap<(Map, usize, usize), u8>,
    in_create: bool,
    have_mknod: Option<bool>,
    globs_for_exclusion: Vec<(String, bool)>,
    sockets: Option<HashSet<PathBuf>>,
    mounts: Option<HashSet<PathBuf>>,
    /// Something failed: exit status 73.
    pub failed: bool,
}

/// An error message without Rust's " (os error N)" suffix.
pub fn errstr(e: &io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn last_err(r: libc::c_int) -> io::Result<()> {
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn mkfifo(p: &Path, mode: u32) -> io::Result<()> {
    let c = cstr(p)?;
    // SAFETY: c is a valid NUL-terminated path.
    last_err(unsafe { libc::mkfifo(c.as_ptr(), mode as libc::mode_t) })
}

pub fn makedev(major: u32, minor: u32) -> u64 {
    let (ma, mi) = (u64::from(major), u64::from(minor));
    ((ma & 0xffff_f000) << 32) | ((ma & 0xfff) << 8) | ((mi & 0xffff_ff00) << 12) | (mi & 0xff)
}

fn mknod(p: &Path, mode: u32, dev: u64) -> io::Result<()> {
    let c = cstr(p)?;
    // SAFETY: c is a valid NUL-terminated path.
    last_err(unsafe { libc::mknod(c.as_ptr(), mode as libc::mode_t, dev as libc::dev_t) })
}

fn lsetxattr(p: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    let c = cstr(p)?;
    let n = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: valid NUL-terminated strings and a buffer of the given length.
    last_err(unsafe {
        libc::lsetxattr(
            c.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    })
}

fn ns(secs: i64, nsec: i64) -> i128 {
    i128::from(secs) * 1_000_000_000 + i128::from(nsec)
}

fn btime_ns(md: &Metadata) -> Option<i128> {
    let t = md.created().ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i128)
}

/// Remove `path` and everything below it, not following symlinks and not
/// descending into other file systems. With `remove_self` false, only the
/// contents of a directory are removed.
pub fn rm_rf(path: &Path, remove_self: bool) -> io::Result<()> {
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if md.is_dir() {
        rm_children(path, md.dev())?;
        if remove_self {
            ignore_missing(fs::remove_dir(path))?;
        }
    } else if remove_self {
        ignore_missing(fs::remove_file(path))?;
    }
    Ok(())
}

fn ignore_missing(r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

fn rm_children(dir: &Path, dev: u64) -> io::Result<()> {
    let mut first = Ok(());
    for e in fs::read_dir(dir)? {
        let p = match e {
            Ok(e) => e.path(),
            Err(e) => {
                first = first.and(Err(e));
                continue;
            }
        };
        let r = match fs::symlink_metadata(&p) {
            Ok(md) if md.is_dir() => {
                if md.dev() != dev {
                    continue;
                }
                rm_children(&p, dev).and_then(|_| ignore_missing(fs::remove_dir(&p)))
            }
            Ok(_) => ignore_missing(fs::remove_file(&p)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
        if first.is_ok() {
            first = r;
        }
    }
    first
}

fn is_empty_dir(p: &Path) -> bool {
    fs::read_dir(p)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
}

fn type_name(md: &Metadata) -> &'static str {
    let ft = md.file_type();
    if ft.is_dir() {
        "directory"
    } else if ft.is_file() {
        "regular file"
    } else if ft.is_symlink() {
        "symlink"
    } else if ft.is_fifo() {
        "fifo"
    } else if ft.is_char_device() {
        "character device"
    } else if ft.is_block_device() {
        "block device"
    } else if ft.is_socket() {
        "socket"
    } else {
        "file"
    }
}

impl<'a> Executor<'a> {
    pub fn new(root: &'a Root, config: &'a Config, dry_run: bool) -> Executor<'a> {
        let globs_for_exclusion = config
            .globs
            .iter()
            .flat_map(|a| a.items.iter())
            .map(|i| (i.path.clone(), i.kind == Kind::IgnoreDirectoryPath))
            .collect();
        Executor {
            root,
            config,
            dry_run,
            now: SystemTime::now(),
            // SAFETY: geteuid() cannot fail.
            euid: unsafe { libc::geteuid() },
            done: HashMap::new(),
            in_create: false,
            have_mknod: None,
            globs_for_exclusion,
            sockets: None,
            mounts: None,
            failed: false,
        }
    }

    /// Run the requested operations: purge, then remove and clean, then
    /// create, each over the plain paths first and the globs second.
    pub fn run(&mut self, ops: u8) {
        let config = self.config;
        for phase in [OP_PURGE, OP_REMOVE | OP_CLEAN, OP_CREATE] {
            let op = ops & phase;
            if op == 0 {
                continue;
            }
            for index in 0..config.items.len() {
                self.process_array(
                    ArrayRef {
                        map: Map::Items,
                        index,
                    },
                    op,
                );
            }
            for index in 0..config.globs.len() {
                self.process_array(
                    ArrayRef {
                        map: Map::Globs,
                        index,
                    },
                    op,
                );
            }
        }
    }

    fn process_array(&mut self, r: ArrayRef, op: u8) {
        let config = self.config;
        let a = config.array(r);
        if op & OP_CREATE != 0
            && let Some(p) = a.parent
        {
            self.process_array(p, op & OP_CREATE);
        }
        if op & (OP_REMOVE | OP_CLEAN) != 0 {
            for &c in &a.children {
                self.process_array(c, op & (OP_REMOVE | OP_CLEAN));
            }
        }
        for (n, item) in a.items.iter().enumerate() {
            let done = self.done.entry((r.map, r.index, n)).or_insert(0);
            let todo = op & !*done;
            if todo == 0 {
                continue;
            }
            *done |= op;
            if todo & OP_CREATE != 0 {
                self.in_create = true;
                self.create(item);
                self.in_create = false;
            }
            if todo & OP_REMOVE != 0 {
                self.remove(item);
            }
            if todo & OP_CLEAN != 0 {
                self.clean(item);
            }
            if todo & OP_PURGE != 0 {
                self.purge(item);
            }
        }
    }

    fn fail(&mut self, item: &Item, msg: String) {
        if self.in_create && item.allow_failure {
            log_notice!("{msg} (ignored, the line has '-')");
        } else {
            log_error!("{msg}");
            self.failed = true;
        }
    }

    fn id_hint(&self, e: &io::Error) -> &'static str {
        match e.raw_os_error() {
            Some(libc::EINVAL) => " (an id that is not mapped in this user namespace?)",
            Some(libc::EPERM) if self.euid != 0 => " (not running as root)",
            Some(libc::EPERM) => " (an id that is not mapped in this user namespace?)",
            _ => "",
        }
    }

    fn glob(&self, item: &Item) -> Vec<Match> {
        glob::glob(self.root, &item.path)
    }

    // ---- creation ------------------------------------------------------

    fn create(&mut self, item: &Item) {
        match item.kind {
            Kind::IgnorePath
            | Kind::IgnoreDirectoryPath
            | Kind::RemovePath
            | Kind::RecursiveRemovePath => {}
            Kind::CreateFile => self.create_file(item),
            Kind::WriteFile => {
                for m in self.glob(item) {
                    self.write_file(item, &m);
                }
            }
            Kind::CreateDirectory | Kind::TruncateDirectory | Kind::CreateSubvolume => {
                self.create_directory(item)
            }
            Kind::EmptyDirectory => {
                for m in self.glob(item) {
                    self.adjust_existing(item, &m.host, true);
                }
            }
            Kind::CreateFifo => self.create_fifo(item),
            Kind::CreateSymlink => self.create_symlink(item),
            Kind::CreateCharDevice | Kind::CreateBlockDevice => self.create_device(item),
            Kind::CopyFiles => self.copy_files(item),
            Kind::AdjustMode => {
                for m in self.glob(item) {
                    self.adjust_existing(item, &m.host, false);
                }
            }
            Kind::RecursiveAdjustMode => {
                for m in self.glob(item) {
                    self.adjust_recursive(item, &m.host, 0);
                }
            }
            Kind::SetXattr | Kind::RecursiveSetXattr => {
                for m in self.glob(item) {
                    self.set_xattrs(item, &m.host, item.kind == Kind::RecursiveSetXattr, 0);
                }
            }
        }
    }

    /// Create the missing parent directories of `item`'s path (0755) and
    /// return where the path itself is on the host. With `=`, what stands
    /// in the way of a parent directory is removed.
    fn mkdir_parents(&mut self, item: &Item) -> Option<PathBuf> {
        let comps: Vec<&str> = item.path.split('/').filter(|c| !c.is_empty()).collect();
        let Some((last, parents)) = comps.split_last() else {
            return Some(self.root.dir().to_path_buf());
        };
        let mut host = self.root.dir().to_path_buf();
        let mut cfg = String::new();
        let mut imaginary = false;
        for c in parents {
            cfg.push('/');
            cfg.push_str(c);
            let cand = host.join(c);
            if imaginary {
                host = cand;
                continue;
            }
            match fs::symlink_metadata(&cand) {
                Ok(md) if md.is_dir() => host = cand,
                Ok(md) => {
                    if md.file_type().is_symlink()
                        && let Ok(r) = self.root.resolve(&cfg, true)
                        && r.is_dir()
                    {
                        host = r;
                        continue;
                    }
                    if !item.try_replace {
                        self.fail(
                            item,
                            format!(
                                "Cannot create {}: {} exists and is not a directory",
                                self.root.prefix(&item.path).display(),
                                cand.display()
                            ),
                        );
                        return None;
                    }
                    if self.dry_run {
                        log_info!(
                            "Would remove {} ({}) and create a directory in its place",
                            cand.display(),
                            type_name(&md)
                        );
                        imaginary = true;
                    } else {
                        let r = rm_rf(&cand, true)
                            .and_then(|_| fs::DirBuilder::new().mode(0o755).create(&cand));
                        if let Err(e) = r {
                            self.fail(
                                item,
                                format!(
                                    "Failed to replace {} with a directory: {}",
                                    cand.display(),
                                    errstr(&e)
                                ),
                            );
                            return None;
                        }
                    }
                    host = cand;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if self.dry_run {
                        log_info!("Would create directory {}", cand.display());
                        imaginary = true;
                    } else {
                        match fs::DirBuilder::new().mode(0o755).create(&cand) {
                            Ok(()) => log_debug!("Created directory {}", cand.display()),
                            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(e) => {
                                self.fail(
                                    item,
                                    format!(
                                        "Failed to create directory {}: {}",
                                        cand.display(),
                                        errstr(&e)
                                    ),
                                );
                                return None;
                            }
                        }
                    }
                    host = cand;
                }
                Err(e) => {
                    self.fail(
                        item,
                        format!(
                            "Failed to create parent directories of {}: {}: {}",
                            item.path,
                            cand.display(),
                            errstr(&e)
                        ),
                    );
                    return None;
                }
            }
        }
        Some(host.join(last))
    }

    /// What is at `host` now; with `=`, something of the wrong type
    /// (`right` says false) is first removed.
    fn existing(
        &mut self,
        item: &Item,
        host: &Path,
        right: impl Fn(&Metadata) -> bool,
    ) -> Option<Metadata> {
        let md = fs::symlink_metadata(host).ok()?;
        if !item.try_replace || right(&md) {
            return Some(md);
        }
        if host == self.root.dir() {
            self.fail(
                item,
                format!("Refusing to remove the root directory {}", host.display()),
            );
            return Some(md);
        }
        if self.dry_run {
            log_info!(
                "Would remove {} ({}), it has the wrong type",
                host.display(),
                type_name(&md)
            );
            return None;
        }
        if let Err(e) = rm_rf(host, true) {
            self.fail(
                item,
                format!(
                    "Failed to remove {} ({}): {}",
                    host.display(),
                    type_name(&md),
                    errstr(&e)
                ),
            );
            return Some(md);
        }
        log_debug!(
            "Removed {} ({}), it had the wrong type",
            host.display(),
            type_name(&md)
        );
        None
    }

    /// Apply the line's mode and ownership (systemd's `fd_set_perms()`).
    fn set_perms(&mut self, item: &Item, host: &Path, md: &Metadata, creation: Creation) {
        if !item.mode_set && item.uid.is_none() && item.gid.is_none() {
            return;
        }
        let is_link = md.file_type().is_symlink();
        let (cur_uid, cur_gid, cur_mode) = (md.uid(), md.gid(), md.mode() & 0o7777);
        let new_uid = match item.uid {
            Some(u) if creation != Creation::Existing || !item.uid_only_create => u,
            _ => cur_uid,
        };
        let new_gid = match item.gid {
            Some(g) if creation != Creation::Existing || !item.gid_only_create => g,
            _ => cur_gid,
        };
        let do_chown = new_uid != cur_uid || new_gid != cur_gid;
        let mut new_mode = None;
        if item.mode_set && !is_link && !(creation == Creation::Existing && item.mode_only_create) {
            let mut m = item.mode;
            if item.mode_mask {
                let old = md.mode();
                if old & 0o111 == 0 {
                    m &= !0o111;
                }
                if old & 0o222 == 0 {
                    m &= !0o222;
                }
                if old & 0o444 == 0 {
                    m &= !0o444;
                }
                if !md.is_dir() {
                    m &= !0o7000;
                }
            }
            if m != cur_mode || do_chown {
                new_mode = Some(m);
            }
        }
        if self.dry_run {
            if do_chown {
                log_info!(
                    "Would change owner of {} from {cur_uid}:{cur_gid} to {new_uid}:{new_gid}",
                    host.display()
                );
            }
            if let Some(m) = new_mode.filter(|&m| m != cur_mode) {
                log_info!(
                    "Would change mode of {} from {cur_mode:04o} to {m:04o}",
                    host.display()
                );
            }
            return;
        }
        if do_chown {
            // Until the owner is right, allow no more than both modes allow.
            if let Some(m) = new_mode {
                let _ = fs::set_permissions(host, fs::Permissions::from_mode(m & cur_mode));
            }
            let (u, g) = (
                (new_uid != cur_uid).then_some(new_uid),
                (new_gid != cur_gid).then_some(new_gid),
            );
            match std::os::unix::fs::lchown(host, u, g) {
                Ok(()) => log_debug!("Changed owner of {} to {new_uid}:{new_gid}", host.display()),
                Err(e) => {
                    let hint = self.id_hint(&e);
                    self.fail(
                        item,
                        format!(
                            "Failed to change owner of {} to {new_uid}:{new_gid}: {}{hint}",
                            host.display(),
                            errstr(&e)
                        ),
                    );
                }
            }
        }
        // chown() clears the set-id bits, so the mode comes last (and is
        // applied even when the owner could not be changed).
        if let Some(m) = new_mode {
            match fs::set_permissions(host, fs::Permissions::from_mode(m)) {
                Ok(()) => log_debug!("Changed mode of {} to {m:04o}", host.display()),
                Err(e) => self.fail(
                    item,
                    format!(
                        "Failed to change mode of {} to {m:04o}: {}",
                        host.display(),
                        errstr(&e)
                    ),
                ),
            }
        }
    }

    fn create_directory(&mut self, item: &Item) {
        let Some(host) = self.mkdir_parents(item) else {
            return;
        };
        match self.existing(item, &host, |m| m.is_dir()) {
            Some(md) if md.is_dir() => self.set_perms(item, &host, &md, Creation::Existing),
            Some(md) => log_warning!(
                "\"{}\" already exists and is not a directory ({}).",
                host.display(),
                type_name(&md)
            ),
            None => {
                if self.dry_run {
                    log_info!(
                        "Would create directory {} (mode {:04o})",
                        host.display(),
                        item.mode
                    );
                    return self.dry_owner(item, &host);
                }
                let creation = match fs::DirBuilder::new()
                    .mode(item.mode & 0o777 | item.mode & 0o1000)
                    .create(&host)
                {
                    Ok(()) => {
                        log_debug!("Created directory {}", host.display());
                        Creation::Normal
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Creation::Existing,
                    Err(e) => {
                        return self.fail(
                            item,
                            format!(
                                "Failed to create directory {}: {}",
                                host.display(),
                                errstr(&e)
                            ),
                        );
                    }
                };
                match fs::symlink_metadata(&host) {
                    Ok(md) if md.is_dir() => self.set_perms(item, &host, &md, creation),
                    Ok(md) => log_warning!(
                        "\"{}\" already exists and is not a directory ({}).",
                        host.display(),
                        type_name(&md)
                    ),
                    Err(e) => self.fail(
                        item,
                        format!("Failed to stat {}: {}", host.display(), errstr(&e)),
                    ),
                }
            }
        }
    }

    fn dry_owner(&self, item: &Item, host: &Path) {
        if item.uid.is_some() || item.gid.is_some() {
            let show = |v: Option<u32>| v.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
            log_info!(
                "Would change owner of {} to {}:{}",
                host.display(),
                show(item.uid),
                show(item.gid)
            );
        }
    }

    fn create_file(&mut self, item: &Item) {
        let Some(host) = self.mkdir_parents(item) else {
            return;
        };
        let md = self.existing(item, &host, |m| m.is_file());
        if let Some(md) = &md
            && !md.is_file()
        {
            return log_warning!(
                "\"{}\" already exists and is not a regular file ({}).",
                host.display(),
                type_name(md)
            );
        }
        let arg = item.argument.as_deref().unwrap_or_default();
        if !item.plus {
            if let Some(md) = md {
                return self.set_perms(item, &host, &md, Creation::Existing);
            }
            if self.dry_run {
                log_info!(
                    "Would create file {} (mode {:04o}, {} bytes)",
                    host.display(),
                    item.mode,
                    arg.len()
                );
                return self.dry_owner(item, &host);
            }
            let f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(item.mode)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
                .open(&host);
            match f {
                Ok(f) => self.write_new(item, &host, f, arg, Creation::Normal),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if let Ok(md) = fs::symlink_metadata(&host) {
                        self.set_perms(item, &host, &md, Creation::Existing);
                    }
                }
                Err(e) => self.fail(
                    item,
                    format!("Failed to create file {}: {}", host.display(), errstr(&e)),
                ),
            }
        } else {
            if self.dry_run {
                let verb = if md.is_some() { "truncate" } else { "create" };
                log_info!(
                    "Would {verb} file {} and write {} bytes",
                    host.display(),
                    arg.len()
                );
                return self.dry_owner(item, &host);
            }
            let creation = if md.is_some() {
                Creation::Force
            } else {
                Creation::Normal
            };
            let f = OpenOptions::new()
                .write(true)
                .create(true)
                .mode(item.mode)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
                .open(&host);
            match f {
                Ok(f) => match f.metadata() {
                    Ok(m) if m.is_file() => match f.set_len(0) {
                        Ok(()) => self.write_new(item, &host, f, arg, creation),
                        Err(e) => self.fail(
                            item,
                            format!("Failed to truncate {}: {}", host.display(), errstr(&e)),
                        ),
                    },
                    Ok(m) => log_warning!(
                        "\"{}\" already exists and is not a regular file ({}).",
                        host.display(),
                        type_name(&m)
                    ),
                    Err(e) => self.fail(
                        item,
                        format!("Failed to stat {}: {}", host.display(), errstr(&e)),
                    ),
                },
                Err(e) => self.fail(
                    item,
                    format!(
                        "Failed to create or truncate file {}: {}",
                        host.display(),
                        errstr(&e)
                    ),
                ),
            }
        }
    }

    fn write_new(
        &mut self,
        item: &Item,
        host: &Path,
        mut f: File,
        data: &[u8],
        creation: Creation,
    ) {
        if let Err(e) = f.write_all(data) {
            return self.fail(
                item,
                format!("Failed to write file {}: {}", host.display(), errstr(&e)),
            );
        }
        log_debug!(
            "{} file {}",
            if creation == Creation::Normal {
                "Created"
            } else {
                "Truncated"
            },
            host.display()
        );
        match f.metadata() {
            Ok(md) => self.set_perms(item, host, &md, creation),
            Err(e) => self.fail(
                item,
                format!("Failed to stat {}: {}", host.display(), errstr(&e)),
            ),
        }
    }

    fn write_file(&mut self, item: &Item, m: &Match) {
        // w follows symlinks (inside the root).
        let host = match self.root.resolve(&m.path, true) {
            Ok(h) => h,
            Err(e) => {
                return self.fail(
                    item,
                    format!("Failed to resolve {}: {}", m.host.display(), errstr(&e)),
                );
            }
        };
        let arg = item.argument.as_deref().unwrap_or_default();
        if self.dry_run {
            return log_info!(
                "Would {} {} bytes to {}",
                if item.plus { "append" } else { "write" },
                arg.len(),
                host.display()
            );
        }
        let f = OpenOptions::new()
            .write(true)
            .append(item.plus)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(&host);
        match f {
            Ok(mut f) => {
                if let Err(e) = f.write_all(arg) {
                    return self.fail(
                        item,
                        format!("Failed to write file {}: {}", host.display(), errstr(&e)),
                    );
                }
                log_debug!("Wrote {} bytes to {}", arg.len(), host.display());
                if let Ok(md) = f.metadata() {
                    self.set_perms(item, &host, &md, Creation::Existing);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                log_debug!("Not writing missing file {}", host.display())
            }
            Err(e) => self.fail(
                item,
                format!("Failed to open file {}: {}", host.display(), errstr(&e)),
            ),
        }
    }

    fn create_fifo(&mut self, item: &Item) {
        let Some(host) = self.mkdir_parents(item) else {
            return;
        };
        let is_fifo = |m: &Metadata| m.file_type().is_fifo();
        let md = self.existing(item, &host, is_fifo);
        let creation = match &md {
            Some(m) if is_fifo(m) => return self.set_perms(item, &host, m, Creation::Existing),
            Some(m) if !item.plus => {
                return log_warning!(
                    "\"{}\" already exists and is not a fifo ({}).",
                    host.display(),
                    type_name(m)
                );
            }
            Some(_) => Creation::Force,
            None => Creation::Normal,
        };
        if self.dry_run {
            log_info!(
                "Would {} fifo {} (mode {:04o})",
                if creation == Creation::Force {
                    "replace with a"
                } else {
                    "create"
                },
                host.display(),
                item.mode
            );
            return self.dry_owner(item, &host);
        }
        if creation == Creation::Force
            && let Err(e) = rm_rf(&host, true)
        {
            return self.fail(
                item,
                format!("Failed to remove {}: {}", host.display(), errstr(&e)),
            );
        }
        if let Err(e) = mkfifo(&host, item.mode) {
            return self.fail(
                item,
                format!("Failed to create fifo {}: {}", host.display(), errstr(&e)),
            );
        }
        if let Ok(md) = fs::symlink_metadata(&host) {
            self.set_perms(item, &host, &md, creation);
        }
    }

    fn create_symlink(&mut self, item: &Item) {
        let Some(host) = self.mkdir_parents(item) else {
            return;
        };
        let target_bytes = item.argument.as_deref().unwrap_or_default();
        let target = OsStr::from_bytes(target_bytes);
        if item.only_if_target_exists {
            let t = String::from_utf8_lossy(target_bytes);
            let inside = if t.starts_with('/') {
                t.into_owned()
            } else {
                let parent = item.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                format!("{parent}/{t}")
            };
            let exists = self
                .root
                .resolve(&inside, true)
                .is_ok_and(|h| fs::metadata(h).is_ok());
            if !exists {
                return log_debug!(
                    "Not creating {}: the symlink target {} does not exist",
                    host.display(),
                    inside
                );
            }
        }
        match fs::symlink_metadata(&host) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if self.dry_run {
                    return log_info!(
                        "Would create symlink {} -> {}",
                        host.display(),
                        target.to_string_lossy()
                    );
                }
                match std::os::unix::fs::symlink(target, &host) {
                    Ok(()) => log_debug!(
                        "Created symlink {} -> {}",
                        host.display(),
                        target.to_string_lossy()
                    ),
                    Err(e) => self.fail(
                        item,
                        format!(
                            "Failed to create symlink {} -> {}: {}",
                            host.display(),
                            target.to_string_lossy(),
                            errstr(&e)
                        ),
                    ),
                }
            }
            Err(e) => self.fail(
                item,
                format!("Failed to stat {}: {}", host.display(), errstr(&e)),
            ),
            Ok(md) => {
                if md.file_type().is_symlink()
                    && fs::read_link(&host).is_ok_and(|t| t.as_os_str() == target)
                {
                    return;
                }
                if !item.plus {
                    return log_debug!(
                        "\"{}\" is not a symlink or does not point to the correct path.",
                        host.display()
                    );
                }
                if self.dry_run {
                    return log_info!(
                        "Would replace {} ({}) with a symlink to {}",
                        host.display(),
                        type_name(&md),
                        target.to_string_lossy()
                    );
                }
                let r = if md.is_dir() {
                    rm_rf(&host, true).and_then(|_| std::os::unix::fs::symlink(target, &host))
                } else {
                    // Replace atomically: a temporary symlink renamed over it.
                    let name = host
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let tmp = host.with_file_name(format!(".#{name}.{}", std::process::id()));
                    let _ = fs::remove_file(&tmp);
                    std::os::unix::fs::symlink(target, &tmp).and_then(|_| {
                        fs::rename(&tmp, &host).inspect_err(|_| {
                            let _ = fs::remove_file(&tmp);
                        })
                    })
                };
                match r {
                    Ok(()) => log_debug!(
                        "Replaced {} with a symlink to {}",
                        host.display(),
                        target.to_string_lossy()
                    ),
                    Err(e) => self.fail(
                        item,
                        format!(
                            "Failed to replace {} with a symlink to {}: {}",
                            host.display(),
                            target.to_string_lossy(),
                            errstr(&e)
                        ),
                    ),
                }
            }
        }
    }

    fn have_mknod(&mut self) -> bool {
        *self.have_mknod.get_or_insert_with(|| {
            let Ok(status) = fs::read_to_string("/proc/self/status") else {
                return true;
            };
            let cap = status
                .lines()
                .find_map(|l| l.strip_prefix("CapEff:"))
                .and_then(|v| u64::from_str_radix(v.trim(), 16).ok());
            cap.is_none_or(|c| c & (1 << 27) != 0)
        })
    }

    fn create_device(&mut self, item: &Item) {
        if !self.have_mknod() {
            return log_debug!(
                "We lack CAP_MKNOD, skipping creation of device node {}.",
                item.path
            );
        }
        let Some(host) = self.mkdir_parents(item) else {
            return;
        };
        let block = item.kind == Kind::CreateBlockDevice;
        let (maj, min) = item.major_minor.unwrap_or_default();
        let dev = makedev(maj, min);
        let what = if block { "block" } else { "character" };
        let right_type = move |m: &Metadata| {
            if block {
                m.file_type().is_block_device()
            } else {
                m.file_type().is_char_device()
            }
        };
        let md = self.existing(item, &host, right_type);
        let creation = match &md {
            Some(m) if right_type(m) && m.rdev() == dev => {
                return self.set_perms(item, &host, m, Creation::Existing);
            }
            Some(m) if !item.plus => {
                return log_warning!(
                    "\"{}\" already exists and is not a {what} device node {maj}:{min} ({}).",
                    host.display(),
                    type_name(m)
                );
            }
            Some(_) => Creation::Force,
            None => Creation::Normal,
        };
        if self.dry_run {
            return log_info!(
                "Would create {what} device node {} {maj}:{min} (mode {:04o})",
                host.display(),
                item.mode
            );
        }
        if creation == Creation::Force
            && let Err(e) = rm_rf(&host, true)
        {
            return self.fail(
                item,
                format!("Failed to remove {}: {}", host.display(), errstr(&e)),
            );
        }
        let ty = if block { S_IFBLK } else { S_IFCHR };
        match mknod(&host, ty | item.mode, dev) {
            Ok(()) => {}
            Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) => {
                return log_debug!(
                    "We lack permissions, skipping creation of device node {}: {}",
                    host.display(),
                    errstr(&e)
                );
            }
            Err(e) => {
                return self.fail(
                    item,
                    format!(
                        "Failed to create device node {}: {}",
                        host.display(),
                        errstr(&e)
                    ),
                );
            }
        }
        if let Ok(md) = fs::symlink_metadata(&host) {
            self.set_perms(item, &host, &md, creation);
        }
    }

    fn copy_files(&mut self, item: &Item) {
        let src_inside =
            String::from_utf8_lossy(item.argument.as_deref().unwrap_or_default()).into_owned();
        let src = match self.root.resolve(&src_inside, true) {
            Ok(s) => s,
            Err(_) => {
                return log_debug!(
                    "Copy source path '{src_inside}' cannot be resolved, skipping line."
                );
            }
        };
        let Ok(src_md) = fs::symlink_metadata(&src) else {
            return log_debug!("Copy source path '{src_inside}' does not exist, skipping line.");
        };
        let Some(host) = self.mkdir_parents(item) else {
            return;
        };
        let before = fs::symlink_metadata(&host).ok();
        if self.dry_run {
            let would = match &before {
                None => true,
                Some(m) => m.is_dir() && src_md.is_dir() && (item.plus || is_empty_dir(&host)),
            };
            if would {
                log_info!("Would copy {} to {}", src.display(), host.display());
            }
            return;
        }
        self.copy_tree(item, &src, &host, !item.plus, 0);
        let Ok(md) = fs::symlink_metadata(&host) else {
            return;
        };
        if (md.mode() ^ src_md.mode()) & S_IFMT != 0 {
            return log_debug!(
                "Can't copy to {}, file exists already and is of different type",
                host.display()
            );
        }
        self.set_perms(
            item,
            &host,
            &md,
            if before.is_some() {
                Creation::Existing
            } else {
                Creation::Normal
            },
        );
    }

    /// Copy `src` to `dst`. An existing directory is copied into if it is
    /// empty (or always, without `only_into_empty`, the `C+` case, where
    /// existing files are left as they are); anything else that exists is
    /// left alone.
    fn copy_tree(
        &mut self,
        item: &Item,
        src: &Path,
        dst: &Path,
        only_into_empty: bool,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return log_warning!("Reached max depth copying to {}", dst.display());
        }
        let smd = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => {
                return self.fail(
                    item,
                    format!("Failed to stat {}: {}", src.display(), errstr(&e)),
                );
            }
        };
        match fs::symlink_metadata(dst) {
            Ok(dmd) => {
                if !(smd.is_dir() && dmd.is_dir()) || (only_into_empty && !is_empty_dir(dst)) {
                    return log_debug!(
                        "{} already exists, not copying {} there",
                        dst.display(),
                        src.display()
                    );
                }
                self.copy_children(item, src, dst, depth);
                return;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return self.fail(
                    item,
                    format!("Failed to stat {}: {}", dst.display(), errstr(&e)),
                );
            }
        }
        let ft = smd.file_type();
        let mode = smd.mode() & 0o7777;
        let r = if ft.is_dir() {
            fs::DirBuilder::new()
                .mode(mode & 0o777)
                .create(dst)
                .map(|_| {
                    self.copy_children(item, src, dst, depth);
                })
        } else if ft.is_file() {
            File::open(src).and_then(|mut from| {
                let mut to = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(mode & 0o777)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(dst)?;
                io::copy(&mut from, &mut to).map(|_| ())
            })
        } else if ft.is_symlink() {
            fs::read_link(src).and_then(|t| std::os::unix::fs::symlink(t, dst))
        } else if ft.is_fifo() {
            mkfifo(dst, mode)
        } else if ft.is_char_device() || ft.is_block_device() {
            let ty = if ft.is_block_device() {
                S_IFBLK
            } else {
                S_IFCHR
            };
            mknod(dst, ty | mode, smd.rdev())
        } else {
            return log_debug!("Not copying socket {}", src.display());
        };
        if let Err(e) = r {
            return self.fail(
                item,
                format!(
                    "Failed to copy {} to {}: {}",
                    src.display(),
                    dst.display(),
                    errstr(&e)
                ),
            );
        }
        self.copy_attrs(item, &smd, dst);
    }

    fn copy_children(&mut self, item: &Item, src: &Path, dst: &Path, depth: usize) {
        let mut names: Vec<_> = match fs::read_dir(src) {
            Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect(),
            Err(e) => {
                return self.fail(
                    item,
                    format!("Failed to read directory {}: {}", src.display(), errstr(&e)),
                );
            }
        };
        names.sort();
        for n in names {
            self.copy_tree(item, &src.join(&n), &dst.join(&n), false, depth + 1);
        }
    }

    /// Owner (the line's if given, else the source's), mode and times of a
    /// copy.
    fn copy_attrs(&mut self, item: &Item, smd: &Metadata, dst: &Path) {
        let Ok(dmd) = fs::symlink_metadata(dst) else {
            return;
        };
        let uid = item.uid.unwrap_or(smd.uid());
        let gid = item.gid.unwrap_or(smd.gid());
        if (uid, gid) != (dmd.uid(), dmd.gid())
            && let Err(e) = std::os::unix::fs::lchown(dst, Some(uid), Some(gid))
        {
            let hint = self.id_hint(&e);
            self.fail(
                item,
                format!(
                    "Failed to change owner of {} to {uid}:{gid}: {}{hint}",
                    dst.display(),
                    errstr(&e)
                ),
            );
        }
        if dmd.file_type().is_symlink() {
            return;
        }
        let mode = smd.mode() & 0o7777;
        if let Err(e) = fs::set_permissions(dst, fs::Permissions::from_mode(mode)) {
            self.fail(
                item,
                format!(
                    "Failed to change mode of {} to {mode:04o}: {}",
                    dst.display(),
                    errstr(&e)
                ),
            );
        }
        if (dmd.is_file() || dmd.is_dir())
            && let (Ok(f), Ok(a), Ok(m)) = (File::open(dst), smd.accessed(), smd.modified())
        {
            let _ = f.set_times(FileTimes::new().set_accessed(a).set_modified(m));
        }
    }

    /// `z`, `e`: adjust what exists.
    fn adjust_existing(&mut self, item: &Item, host: &Path, must_be_dir: bool) {
        let Ok(md) = fs::symlink_metadata(host) else {
            return log_debug!("{} does not exist, not adjusting it", host.display());
        };
        if must_be_dir && !md.is_dir() {
            return log_warning!(
                "'{}' already exists and is not a directory.",
                host.display()
            );
        }
        self.set_perms(item, host, &md, Creation::Existing);
    }

    fn adjust_recursive(&mut self, item: &Item, host: &Path, depth: usize) {
        let Ok(md) = fs::symlink_metadata(host) else {
            return;
        };
        self.set_perms(item, host, &md, Creation::Existing);
        if md.is_dir() && depth < MAX_DEPTH {
            match fs::read_dir(host) {
                Ok(rd) => {
                    let mut names: Vec<_> =
                        rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
                    names.sort();
                    for n in names {
                        self.adjust_recursive(item, &host.join(n), depth + 1);
                    }
                }
                Err(e) => self.fail(
                    item,
                    format!(
                        "Failed to read directory {}: {}",
                        host.display(),
                        errstr(&e)
                    ),
                ),
            }
        }
    }

    fn set_xattrs(&mut self, item: &Item, host: &Path, recursive: bool, depth: usize) {
        let Ok(md) = fs::symlink_metadata(host) else {
            return;
        };
        for (name, value) in &item.xattrs {
            if self.dry_run {
                log_info!("Would set extended attribute {name} on {}", host.display());
                continue;
            }
            if let Err(e) = lsetxattr(host, name, value) {
                self.fail(
                    item,
                    format!(
                        "Failed to set extended attribute {name} on {}: {}",
                        host.display(),
                        errstr(&e)
                    ),
                );
            }
        }
        if recursive
            && md.is_dir()
            && depth < MAX_DEPTH
            && let Ok(rd) = fs::read_dir(host)
        {
            let mut names: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
            names.sort();
            for n in names {
                self.set_xattrs(item, &host.join(n), true, depth + 1);
            }
        }
    }

    // ---- removal -------------------------------------------------------

    fn remove(&mut self, item: &Item) {
        match item.kind {
            Kind::TruncateDirectory | Kind::RemovePath | Kind::RecursiveRemovePath => {}
            _ => return,
        }
        for m in self.glob(item) {
            if m.host == self.root.dir() {
                self.fail(
                    item,
                    format!("Refusing to remove the root directory {}", m.host.display()),
                );
                continue;
            }
            match item.kind {
                Kind::TruncateDirectory => {
                    let Ok(md) = fs::symlink_metadata(&m.host) else {
                        continue;
                    };
                    if !md.is_dir() {
                        continue;
                    }
                    if self.dry_run {
                        log_info!("Would remove the contents of {}", m.host.display());
                    } else if let Err(e) = rm_rf(&m.host, false) {
                        self.fail(
                            item,
                            format!(
                                "Failed to remove the contents of {}: {}",
                                m.host.display(),
                                errstr(&e)
                            ),
                        );
                    }
                }
                Kind::RemovePath => {
                    let Ok(md) = fs::symlink_metadata(&m.host) else {
                        continue;
                    };
                    if self.dry_run {
                        log_info!("Would remove {}", m.host.display());
                        continue;
                    }
                    let r = if md.is_dir() {
                        fs::remove_dir(&m.host)
                    } else {
                        fs::remove_file(&m.host)
                    };
                    match ignore_missing(r) {
                        Ok(()) => log_debug!("Removed {}", m.host.display()),
                        Err(e) => {
                            self.fail(item, format!("rm({}): {}", m.host.display(), errstr(&e)))
                        }
                    }
                }
                _ => {
                    if self.dry_run {
                        log_info!("Would remove {} recursively", m.host.display());
                    } else if let Err(e) = rm_rf(&m.host, true) {
                        self.fail(item, format!("rm -rf {}: {}", m.host.display(), errstr(&e)));
                    }
                }
            }
        }
    }

    fn purge(&mut self, item: &Item) {
        if !item.purge {
            return;
        }
        match item.kind {
            Kind::CreateFile
            | Kind::WriteFile
            | Kind::CreateDirectory
            | Kind::TruncateDirectory
            | Kind::EmptyDirectory
            | Kind::CreateSubvolume
            | Kind::CreateFifo
            | Kind::CreateSymlink
            | Kind::CreateCharDevice
            | Kind::CreateBlockDevice
            | Kind::CopyFiles => {}
            _ => return,
        }
        for m in self.glob(item) {
            if m.host == self.root.dir() {
                continue;
            }
            if self.dry_run {
                log_info!("Would remove {} (purge)", m.host.display());
            } else if let Err(e) = rm_rf(&m.host, true) {
                self.fail(
                    item,
                    format!("Failed to purge {}: {}", m.host.display(), errstr(&e)),
                );
            }
        }
    }

    // ---- cleaning ------------------------------------------------------

    fn clean(&mut self, item: &Item) {
        match item.kind {
            Kind::CreateDirectory
            | Kind::TruncateDirectory
            | Kind::CreateSubvolume
            | Kind::CopyFiles => self.clean_instance(item, &item.path),
            Kind::EmptyDirectory | Kind::IgnorePath | Kind::IgnoreDirectoryPath => {
                for m in self.glob(item) {
                    self.clean_instance(item, &m.path);
                }
            }
            _ => {}
        }
    }

    fn clean_instance(&mut self, item: &Item, inside: &str) {
        let Some(age) = &item.age else { return };
        let now = self
            .now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        if now < age.usec {
            return;
        }
        let cutoff = i128::from(now - age.usec) * 1000;
        let Ok(host) = self.root.resolve(inside, true) else {
            return;
        };
        let md = match fs::metadata(&host) {
            Ok(m) => m,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => {
                return log_debug!("Directory \"{}\": {}", host.display(), errstr(&e));
            }
            Err(e) => {
                return self.fail(
                    item,
                    format!(
                        "Failed to open directory {}: {}",
                        host.display(),
                        errstr(&e)
                    ),
                );
            }
        };
        if !md.is_dir() {
            return log_warning!("{} is not a directory, not cleaning it.", host.display());
        }
        let mountpoint = fs::metadata(host.join(".."))
            .map(|p| p.dev() != md.dev() || p.ino() == md.ino())
            .unwrap_or(false);
        log_debug!(
            "Cleaning {} (entries older than {}s)",
            host.display(),
            age.usec / 1_000_000
        );
        self.dir_cleanup(
            inside,
            &host,
            &md,
            cutoff,
            md.dev(),
            mountpoint,
            MAX_DEPTH,
            age.keep_first_level,
            age,
        );
    }

    fn exclusion(&self, inside: &str) -> Excl {
        if self.config.has_item_path(inside) {
            return Excl::Skip;
        }
        let mut keep = false;
        for (pat, upper_x) in &self.globs_for_exclusion {
            if glob::path_match(pat, inside) {
                if *upper_x {
                    keep = true;
                } else {
                    return Excl::Skip;
                }
            }
        }
        if keep { Excl::KeepSelf } else { Excl::None }
    }

    fn live_socket(&mut self, p: &Path) -> bool {
        let set = self.sockets.get_or_insert_with(|| {
            let text = fs::read_to_string("/proc/net/unix").unwrap_or_default();
            text.lines()
                .skip(1)
                .filter_map(|l| l.split_whitespace().nth(7))
                .filter(|s| s.starts_with('/'))
                .map(PathBuf::from)
                .collect()
        });
        set.contains(p)
    }

    fn is_mount_point(&mut self, p: &Path) -> bool {
        let set = self.mounts.get_or_insert_with(|| {
            let text = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
            text.lines()
                .filter_map(|l| l.split(' ').nth(4))
                .map(|m| PathBuf::from(unescape_mountinfo(m)))
                .collect()
        });
        set.contains(p)
    }

    #[allow(clippy::too_many_arguments)]
    fn dir_cleanup(
        &mut self,
        inside: &str,
        host: &Path,
        self_md: &Metadata,
        cutoff: i128,
        rootdev: u64,
        mountpoint: bool,
        depth: usize,
        keep_this_level: bool,
        age: &Age,
    ) {
        let mut names: Vec<_> = match fs::read_dir(host) {
            Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect(),
            Err(e) => {
                return log_warning!(
                    "Failed to read directory {}, not cleaning it: {}",
                    host.display(),
                    errstr(&e)
                );
            }
        };
        names.sort();
        let mut deleted = false;
        for name in names {
            let sub_host = host.join(&name);
            let name_s = name.to_string_lossy();
            let sub_inside = if inside == "/" {
                format!("/{name_s}")
            } else {
                format!("{inside}/{name_s}")
            };
            let md = match fs::symlink_metadata(&sub_host) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    log_debug!("stat({}) failed: {}", sub_host.display(), errstr(&e));
                    continue;
                }
            };
            if md.dev() != rootdev {
                log_debug!("Ignoring \"{}\": different filesystem.", sub_host.display());
                continue;
            }
            let excl = self.exclusion(&sub_inside);
            if excl == Excl::Skip {
                log_debug!(
                    "Ignoring \"{}\": a separate line covers it.",
                    sub_host.display()
                );
                continue;
            }
            let (atime, mtime, ctime, btime) = (
                ns(md.atime(), md.atime_nsec()),
                ns(md.mtime(), md.mtime_nsec()),
                ns(md.ctime(), md.ctime_nsec()),
                btime_ns(&md),
            );
            if md.is_dir() {
                if self.is_mount_point(&sub_host) {
                    log_debug!(
                        "Ignoring \"{}\": different mount of the same filesystem.",
                        sub_host.display()
                    );
                    continue;
                }
                if mountpoint && name_s == "lost+found" && md.uid() == 0 {
                    continue;
                }
                let _lock;
                if depth == 0 {
                    log_warning!("Reached max depth on \"{}\".", sub_host.display());
                } else {
                    match self.try_lock(&sub_host, true) {
                        Err(()) => {
                            log_debug!(
                                "Couldn't acquire BSD lock on directory \"{}\", skipping.",
                                sub_host.display()
                            );
                            continue;
                        }
                        Ok(l) => _lock = l,
                    }
                    self.dir_cleanup(
                        &sub_inside,
                        &sub_host,
                        &md,
                        cutoff,
                        rootdev,
                        false,
                        depth - 1,
                        false,
                        age,
                    );
                }
                if keep_this_level || excl == Excl::KeepSelf {
                    continue;
                }
                if !needs_cleanup(atime, btime, ctime, mtime, cutoff, age.by_dir) {
                    continue;
                }
                if self.dry_run {
                    log_info!("Would remove directory \"{}\"", sub_host.display());
                    continue;
                }
                match fs::remove_dir(&sub_host) {
                    Ok(()) => {
                        log_debug!("Removed directory \"{}\"", sub_host.display());
                        deleted = true;
                    }
                    Err(e)
                        if matches!(
                            e.raw_os_error(),
                            Some(libc::ENOENT) | Some(libc::ENOTEMPTY)
                        ) => {}
                    Err(e) => log_warning!(
                        "Failed to remove directory \"{}\", ignoring: {}",
                        sub_host.display(),
                        errstr(&e)
                    ),
                }
            } else {
                let ft = md.file_type();
                if md.mode() & S_ISVTX != 0 {
                    log_debug!("Skipping \"{}\": sticky bit set.", sub_host.display());
                    continue;
                }
                if mountpoint
                    && ft.is_file()
                    && md.uid() == 0
                    && [".journal", "aquota.user", "aquota.group"].contains(&name_s.as_ref())
                {
                    continue;
                }
                if ft.is_socket() && self.live_socket(&sub_host) {
                    log_debug!("Skipping \"{}\": live socket.", sub_host.display());
                    continue;
                }
                if ft.is_char_device() || ft.is_block_device() {
                    continue;
                }
                if keep_this_level || excl == Excl::KeepSelf {
                    continue;
                }
                if !needs_cleanup(atime, btime, ctime, mtime, cutoff, age.by_file) {
                    continue;
                }
                if self.dry_run {
                    log_info!("Would remove \"{}\"", sub_host.display());
                    continue;
                }
                let Ok(_lock) = self.try_lock(&sub_host, false) else {
                    log_debug!(
                        "Couldn't acquire BSD lock on \"{}\", skipping.",
                        sub_host.display()
                    );
                    continue;
                };
                match fs::remove_file(&sub_host) {
                    Ok(()) => {
                        log_debug!("Removed \"{}\"", sub_host.display());
                        deleted = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => log_warning!(
                        "Failed to remove \"{}\", ignoring: {}",
                        sub_host.display(),
                        errstr(&e)
                    ),
                }
            }
        }
        if deleted {
            // Cleaning should not make the directory itself look recent.
            if let (Ok(f), Ok(a), Ok(m)) =
                (File::open(host), self_md.accessed(), self_md.modified())
            {
                let _ = f.set_times(FileTimes::new().set_accessed(a).set_modified(m));
            }
        }
    }

    /// Take an exclusive BSD lock, as cleaning does to stay away from what
    /// others have locked. `Err` if someone else holds a lock.
    fn try_lock(&self, p: &Path, dir: bool) -> Result<Option<File>, ()> {
        if self.dry_run {
            return Ok(None);
        }
        let flags = libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_NOCTTY
            | if dir { libc::O_DIRECTORY } else { 0 };
        let Ok(f) = OpenOptions::new().read(true).custom_flags(flags).open(p) else {
            return Ok(None);
        };
        // SAFETY: f is an open file descriptor.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(());
            }
            return Ok(None);
        }
        Ok(Some(f))
    }
}

/// Whether an entry is old enough to go: none of the timestamps chosen by
/// `by` is newer than the cutoff.
fn needs_cleanup(
    atime: i128,
    btime: Option<i128>,
    ctime: i128,
    mtime: i128,
    cutoff: i128,
    by: AgeBy,
) -> bool {
    let too_new = |flag: u8, t: Option<i128>| by.has(flag) && t.is_some_and(|t| t >= cutoff);
    !(too_new(AgeBy::MTIME, Some(mtime))
        || too_new(AgeBy::ATIME, Some(atime))
        || too_new(AgeBy::CTIME, Some(ctime))
        || too_new(AgeBy::BTIME, btime))
}

fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
