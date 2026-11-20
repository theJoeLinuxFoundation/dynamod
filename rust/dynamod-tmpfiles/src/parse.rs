//! Parsing tmpfiles.d(5) lines into [`Item`]s. Nothing here touches the
//! file system except reading a credential for `^` lines.
use dynamod_sdconf::accounts::IdDb;
use dynamod_sdconf::rootfs::{path_starts_with, simplify};
use dynamod_sdconf::specifier::{SpecError, Specifiers};
use dynamod_sdconf::words::{self, Flags};

use crate::item::{Age, AgeBy, Item, Kind};

/// What a line needs from its surroundings.
pub struct ParseCtx<'a> {
    /// `--boot`: lines with `!` count.
    pub boot: bool,
    /// `--graceful`: lines naming unknown users or groups are skipped.
    pub graceful: bool,
    pub include_prefixes: &'a [String],
    pub exclude_prefixes: &'a [String],
    pub specifiers: &'a Specifiers,
    pub ids: &'a IdDb,
    /// `$CREDENTIALS_DIRECTORY`, for `^` lines.
    pub credentials_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineError {
    /// Invalid configuration: reported, the line ignored, exit status 65.
    Invalid(String),
    /// A specifier without a value yet (`%m` before there is a machine ID):
    /// the line is skipped with a notice, as systemd-tmpfiles does.
    Unresolvable(String),
}

/// A line that was understood.
#[derive(Debug, Default)]
pub struct Parsed {
    /// `None` when the line does not apply (`!` without `--boot`, outside
    /// the prefixes, a `^` credential that is not set, ...).
    pub item: Option<Item>,
    /// Warnings about the line that do not make it invalid.
    pub warnings: Vec<String>,
    /// Why it was skipped, for debug output.
    pub skipped: Option<String>,
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, LineError> {
    Err(LineError::Invalid(msg.into()))
}

fn spec_err(e: SpecError, what: &str) -> LineError {
    match e {
        SpecError::Unresolvable(..) => {
            LineError::Unresolvable(format!("{e} (in '{what}'), skipping line"))
        }
        _ => LineError::Invalid(format!("{e} in '{what}'")),
    }
}

fn empty_or_dash(s: &Option<String>) -> bool {
    matches!(s.as_deref(), None | Some("") | Some("-"))
}

fn kind_of(letter: char) -> Result<(Kind, char), LineError> {
    Ok(match letter {
        'f' => (Kind::CreateFile, 'f'),
        'F' => (Kind::CreateFile, 'f'),
        'w' => (Kind::WriteFile, 'w'),
        'd' => (Kind::CreateDirectory, 'd'),
        'D' => (Kind::TruncateDirectory, 'D'),
        'e' => (Kind::EmptyDirectory, 'e'),
        'v' | 'q' | 'Q' => (Kind::CreateSubvolume, letter),
        'p' => (Kind::CreateFifo, 'p'),
        'L' => (Kind::CreateSymlink, 'L'),
        'c' => (Kind::CreateCharDevice, 'c'),
        'b' => (Kind::CreateBlockDevice, 'b'),
        'C' => (Kind::CopyFiles, 'C'),
        'x' => (Kind::IgnorePath, 'x'),
        'X' => (Kind::IgnoreDirectoryPath, 'X'),
        'r' => (Kind::RemovePath, 'r'),
        'R' => (Kind::RecursiveRemovePath, 'R'),
        'z' | 'm' => (Kind::AdjustMode, 'z'),
        'Z' => (Kind::RecursiveAdjustMode, 'Z'),
        't' => (Kind::SetXattr, 't'),
        'T' => (Kind::RecursiveSetXattr, 'T'),
        'h' | 'H' => {
            return invalid(format!(
                "line type '{letter}' (file attributes, chattr) is not supported by dynamod-tmpfiles"
            ));
        }
        'a' | 'A' => {
            return invalid(format!(
                "line type '{letter}' (POSIX ACLs) is not supported by dynamod-tmpfiles"
            ));
        }
        'k' | 'K' => {
            return invalid(format!(
                "line type '{letter}' (file capabilities) is not supported by dynamod-tmpfiles"
            ));
        }
        _ => return invalid(format!("Unknown command type '{letter}'.")),
    })
}

/// Parse one line (already known not to be empty or a comment).
pub fn parse_line(line: &str, ctx: &ParseCtx) -> Result<Parsed, LineError> {
    let flags = Flags {
        unquote: true,
        cunescape: true,
    };
    let mut rest = line;
    let mut fields: Vec<Option<String>> = Vec::with_capacity(6);
    for _ in 0..6 {
        match words::next_word(&mut rest, flags) {
            Ok(w) => fields.push(w),
            Err(e) => return invalid(format!("Failed to parse line: {e}")),
        }
    }
    let [action, path, mode, user, group, age]: [Option<String>; 6] = fields.try_into().unwrap();
    let (Some(action), Some(path)) = (action, path) else {
        return invalid("Syntax error.");
    };
    let raw_argument = if rest.is_empty() || rest == "-" {
        None
    } else {
        Some(rest.to_string())
    };

    let mut out = Parsed::default();
    let mut chars = action.chars();
    let Some(letter) = chars.next() else {
        return invalid(format!("Command too short '{action}'."));
    };
    let (kind, letter_norm) = kind_of(letter)?;
    if letter == 'F' {
        out.warnings
            .push("line type 'F' is obsolete, treating it as 'f+'".into());
    }
    if letter == 'm' {
        out.warnings
            .push("line type 'm' is obsolete, treating it as 'z'".into());
    }
    let mut item = Item::new(kind, letter_norm, "/");
    item.plus = letter == 'F';
    let (mut unbase64, mut from_cred) = (false, false);
    for m in chars {
        let flag = match m {
            '+' => &mut item.plus,
            '!' => &mut item.boot,
            '-' => &mut item.allow_failure,
            '=' => &mut item.try_replace,
            '~' => &mut unbase64,
            '^' => &mut from_cred,
            '$' => &mut item.purge,
            '?' if kind == Kind::CreateSymlink => &mut item.only_if_target_exists,
            _ => return invalid(format!("Unknown modifiers in command '{action}'")),
        };
        if *flag {
            return invalid(format!("Unknown modifiers in command '{action}'"));
        }
        *flag = true;
    }

    if item.boot && !ctx.boot {
        out.skipped = Some(format!("'{action}' line for {path}: --boot not given"));
        return Ok(out);
    }

    let mut p = ctx
        .specifiers
        .expand(&path)
        .map_err(|e| spec_err(e, &path))?;
    if let Some(rest) = p.strip_prefix("/var/run/")
        && !rest.is_empty()
    {
        let np = format!("/run/{rest}");
        out.warnings.push(format!("Line references path below legacy directory /var/run/, updating {p} \u{2192} {np}; please update the tmpfiles.d/ drop-in file accordingly."));
        p = np;
    }
    if !p.starts_with('/') {
        return invalid(format!("Path '{p}' not absolute."));
    }
    item.path = simplify(&p);

    // The argument column.
    let mut argument = raw_argument;
    match kind {
        Kind::CreateDirectory
        | Kind::CreateSubvolume
        | Kind::EmptyDirectory
        | Kind::TruncateDirectory
        | Kind::CreateFifo
        | Kind::IgnorePath
        | Kind::IgnoreDirectoryPath
        | Kind::RemovePath
        | Kind::RecursiveRemovePath
        | Kind::AdjustMode
        | Kind::RecursiveAdjustMode => {
            if argument.is_some() {
                out.warnings.push(format!(
                    "{letter} lines don't take argument fields, ignoring."
                ));
                argument = None;
            }
        }
        Kind::CreateFile => {}
        Kind::WriteFile => {
            if argument.is_none() && !from_cred {
                return invalid("Write file requires argument.");
            }
        }
        Kind::CreateSymlink | Kind::CopyFiles => {
            if unbase64 {
                return invalid(format!(
                    "{letter} lines don't support base64 decoding (the '~' modifier)."
                ));
            }
            if argument.is_none() && !from_cred {
                // No argument: the same path below /usr/share/factory.
                argument = Some(
                    format!("/usr/share/factory{}", item.path)
                        .replace('\\', "\\\\")
                        .replace('%', "%%"),
                );
            }
        }
        Kind::CreateCharDevice | Kind::CreateBlockDevice => {
            if unbase64 || from_cred {
                return invalid(format!(
                    "{letter} lines don't support the '~' and '^' modifiers."
                ));
            }
            let Some(a) = &argument else {
                return invalid("Device file requires argument.");
            };
            item.major_minor = Some(parse_devnum(a).ok_or_else(|| {
                LineError::Invalid(format!("Can't parse device file major/minor '{a}'."))
            })?);
        }
        Kind::SetXattr | Kind::RecursiveSetXattr => {
            if unbase64 || from_cred {
                return invalid(format!(
                    "{letter} lines don't support the '~' and '^' modifiers."
                ));
            }
            let Some(a) = &argument else {
                return invalid("Set extended attribute requires argument.");
            };
            item.xattrs = parse_xattrs(a, ctx.specifiers)?;
        }
    }
    if (unbase64 || from_cred) && !matches!(kind, Kind::CreateFile | Kind::WriteFile) {
        return invalid(format!(
            "The '~' and '^' modifiers are only supported on f and w lines, not '{action}'."
        ));
    }

    if !should_include(&item.path, ctx) {
        out.skipped = Some(format!("{} is outside the selected prefixes", item.path));
        return Ok(out);
    }

    // C-unescape and expand specifiers in what f, w, L and C write or use.
    if from_cred {
        let Some(name) = argument.as_deref() else {
            return invalid("The '^' modifier requires a credential name in the argument field.");
        };
        let data = ctx
            .credentials_dir
            .as_ref()
            .and_then(|d| std::fs::read(d.join(name)).ok());
        let Some(data) = data else {
            out.skipped = Some(format!("credential '{name}' is not set"));
            return Ok(out);
        };
        item.argument = Some(if unbase64 {
            words::unbase64(&String::from_utf8_lossy(&data)).ok_or_else(|| {
                LineError::Invalid(format!("Failed to base64 decode credential '{name}'."))
            })?
        } else {
            data
        });
    } else if let Some(a) = argument {
        match kind {
            Kind::CreateFile | Kind::WriteFile | Kind::CreateSymlink | Kind::CopyFiles => {
                if unbase64 {
                    item.argument = Some(words::unbase64(&a).ok_or_else(|| {
                        LineError::Invalid(format!("Failed to base64 decode '{a}'."))
                    })?);
                } else {
                    let unescaped = words::cunescape(&a).map_err(|e| {
                        LineError::Invalid(format!(
                            "Failed to unescape parameter to write: {a}: {e}"
                        ))
                    })?;
                    let expanded = ctx
                        .specifiers
                        .expand_bytes(&unescaped)
                        .map_err(|e| spec_err(e, &a))?;
                    item.argument = Some(expanded);
                }
            }
            _ => item.argument = Some(a.into_bytes()),
        }
    }
    if kind == Kind::CopyFiles {
        let src = item.argument.as_deref().unwrap_or_default();
        if !src.starts_with(b"/") {
            return invalid(format!(
                "Source path '{}' is not absolute.",
                String::from_utf8_lossy(src)
            ));
        }
    }

    // User and group.
    if !empty_or_dash(&user) {
        let u = user.unwrap();
        let (name, only) = match u.strip_prefix(':') {
            Some(n) => (n.to_string(), true),
            None => (u.clone(), false),
        };
        match ctx.ids.uid(&name) {
            Some(id) => item.uid = Some(id),
            None if ctx.graceful => {
                out.skipped = Some(format!("{}: user '{name}' not found", item.path));
                return Ok(out);
            }
            None => {
                return invalid(format!(
                    "Failed to resolve user '{name}': no such user in /etc/passwd."
                ));
            }
        }
        item.uid_only_create = only;
    }
    if !empty_or_dash(&group) {
        let g = group.unwrap();
        let (name, only) = match g.strip_prefix(':') {
            Some(n) => (n.to_string(), true),
            None => (g.clone(), false),
        };
        match ctx.ids.gid(&name) {
            Some(id) => item.gid = Some(id),
            None if ctx.graceful => {
                out.skipped = Some(format!("{}: group '{name}' not found", item.path));
                return Ok(out);
            }
            None => {
                return invalid(format!(
                    "Failed to resolve group '{name}': no such group in /etc/group."
                ));
            }
        }
        item.gid_only_create = only;
    }

    // Mode.
    if !empty_or_dash(&mode) {
        let m = mode.unwrap();
        let mut s = m.as_str();
        loop {
            if let Some(r) = s.strip_prefix('~') {
                item.mode_mask = true;
                s = r;
            } else if let Some(r) = s.strip_prefix(':') {
                item.mode_only_create = true;
                s = r;
            } else {
                break;
            }
        }
        item.mode =
            parse_mode(s).ok_or_else(|| LineError::Invalid(format!("Invalid mode '{m}'.")))?;
        item.mode_set = true;
    }

    // Age.
    if !empty_or_dash(&age) {
        let a = age.unwrap();
        item.age =
            Some(parse_age(&a).ok_or_else(|| LineError::Invalid(format!("Invalid age '{a}'.")))?);
    }

    out.item = Some(item);
    Ok(out)
}

fn should_include(path: &str, ctx: &ParseCtx) -> bool {
    if ctx
        .exclude_prefixes
        .iter()
        .any(|p| path_starts_with(path, p))
    {
        return false;
    }
    ctx.include_prefixes.is_empty()
        || ctx
            .include_prefixes
            .iter()
            .any(|p| path_starts_with(path, p))
}

/// An octal mode up to 07777.
pub fn parse_mode(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    let m = u32::from_str_radix(s, 8).ok()?;
    (m <= 0o7777).then_some(m)
}

/// `major:minor`.
pub fn parse_devnum(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.trim().split_once(':')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// `name=value name="value with spaces"`.
fn parse_xattrs(s: &str, spec: &Specifiers) -> Result<Vec<(String, Vec<u8>)>, LineError> {
    let mut rest = s;
    let mut out = Vec::new();
    let flags = Flags {
        unquote: true,
        cunescape: true,
    };
    while let Some(w) = words::next_word(&mut rest, flags).map_err(|e| {
        LineError::Invalid(format!("Failed to parse extended attributes '{s}': {e}"))
    })? {
        let Some((name, value)) = w.split_once('=') else {
            return invalid(format!(
                "Extended attribute '{w}' is not of the form name=value."
            ));
        };
        if name.is_empty() {
            return invalid(format!("Extended attribute '{w}' has no name."));
        }
        let value = spec.expand(value).map_err(|e| spec_err(e, value))?;
        out.push((name.to_string(), value.into_bytes()));
    }
    if out.is_empty() {
        return invalid("Set extended attribute requires argument.");
    }
    Ok(out)
}

/// systemd's `parse_sec()`: `10d`, `1h30min`, `1.5h`, `500ms`, a plain
/// number of seconds, `infinity`. Returns microseconds.
pub fn parse_sec(s: &str) -> Option<u64> {
    const UNITS: &[(&str, f64)] = &[
        ("seconds", 1e6),
        ("second", 1e6),
        ("sec", 1e6),
        ("s", 1e6),
        ("minutes", 60e6),
        ("minute", 60e6),
        ("min", 60e6),
        ("m", 60e6),
        ("months", 2_629_800e6),
        ("month", 2_629_800e6),
        ("M", 2_629_800e6),
        ("msec", 1e3),
        ("ms", 1e3),
        ("hours", 3_600e6),
        ("hour", 3_600e6),
        ("hr", 3_600e6),
        ("h", 3_600e6),
        ("days", 86_400e6),
        ("day", 86_400e6),
        ("d", 86_400e6),
        ("weeks", 604_800e6),
        ("week", 604_800e6),
        ("w", 604_800e6),
        ("years", 31_557_600e6),
        ("year", 31_557_600e6),
        ("y", 31_557_600e6),
        ("usec", 1.0),
        ("us", 1.0),
        ("\u{3bc}s", 1.0),
    ];
    let s = s.trim();
    if s == "infinity" {
        return Some(u64::MAX);
    }
    if s.is_empty() {
        return None;
    }
    let mut total: f64 = 0.0;
    let mut rest = s;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if num_len == 0 {
            return None;
        }
        let num: f64 = rest[..num_len].parse().ok()?;
        rest = rest[num_len..].trim_start();
        let unit_len = rest
            .find(|c: char| !(c.is_alphabetic()))
            .unwrap_or(rest.len());
        let unit = &rest[..unit_len];
        let mult = if unit.is_empty() {
            1e6
        } else {
            UNITS.iter().find(|(u, _)| *u == unit)?.1
        };
        total += num * mult;
        rest = &rest[unit_len..];
    }
    Some(total.round() as u64)
}

/// The age column: `[~][age-by:]time`.
pub fn parse_age(s: &str) -> Option<Age> {
    let mut a = s;
    let keep_first_level = if let Some(r) = a.strip_prefix('~') {
        a = r;
        true
    } else {
        false
    };
    let (mut by_file, mut by_dir) = (AgeBy::DEFAULT_FILE, AgeBy::DEFAULT_DIR);
    if let Some((by, t)) = a.split_once(':') {
        let (mut f, mut d) = (0u8, 0u8);
        for c in by.chars() {
            let flag = match c.to_ascii_lowercase() {
                'a' => AgeBy::ATIME,
                'b' => AgeBy::BTIME,
                'c' => AgeBy::CTIME,
                'm' => AgeBy::MTIME,
                ' ' | '\t' => continue,
                _ => return None,
            };
            if c.is_ascii_uppercase() {
                d |= flag
            } else {
                f |= flag
            }
        }
        if f == 0 && d == 0 {
            return None;
        }
        by_file = AgeBy(f);
        by_dir = AgeBy(d);
        a = t;
    }
    Some(Age {
        usec: parse_sec(a)?,
        keep_first_level,
        by_file,
        by_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynamod_sdconf::Root;

    struct Env {
        spec: Specifiers,
        ids: IdDb,
        include: Vec<String>,
        exclude: Vec<String>,
        boot: bool,
        graceful: bool,
    }

    fn env() -> Env {
        let spec = Specifiers::new(&Root::host(), true)
            .with_value('m', "0123456789abcdef0123456789abcdef")
            .with_value('H', "box")
            .with_value('v', "6.12.0")
            .with_value('b', "fedcba9876543210fedcba9876543210");
        let ids = IdDb::from_texts(
            "sshd:x:50:50::/:/bin/false\nnobody:x:65534:65534::/:/bin/false\n",
            "systemd-journal:x:23:\nsys:x:2:\nnogroup:x:65534:\n",
        );
        Env {
            spec,
            ids,
            include: vec![],
            exclude: vec![],
            boot: false,
            graceful: false,
        }
    }

    fn parse_with(e: &Env, line: &str) -> Result<Parsed, LineError> {
        let ctx = ParseCtx {
            boot: e.boot,
            graceful: e.graceful,
            include_prefixes: &e.include,
            exclude_prefixes: &e.exclude,
            specifiers: &e.spec,
            ids: &e.ids,
            credentials_dir: None,
        };
        parse_line(line, &ctx)
    }

    fn item(line: &str) -> Item {
        parse_with(&env(), line).unwrap().item.unwrap()
    }

    fn err(line: &str) -> String {
        match parse_with(&env(), line) {
            Err(LineError::Invalid(m)) => m,
            other => panic!("expected an invalid line for {line:?}, got {other:?}"),
        }
    }

    #[test]
    fn directory_lines() {
        let i = item("d /var/log/journal 2755 root systemd-journal -");
        assert_eq!(
            (i.kind, i.letter, i.path.as_str()),
            (Kind::CreateDirectory, 'd', "/var/log/journal")
        );
        assert_eq!(
            (i.mode, i.mode_set, i.uid, i.gid, i.age.clone()),
            (0o2755, true, Some(0), Some(23), None)
        );
        let i = item("d /run/x");
        assert_eq!(
            (i.mode, i.mode_set, i.uid, i.gid),
            (0o755, false, None, None)
        );
        let i = item("D /run/cups - - - -");
        assert_eq!(i.kind, Kind::TruncateDirectory);
        for t in ["v", "q", "Q"] {
            let i = item(&format!("{t} /var/lib/machines 0700 - - -"));
            assert_eq!((i.kind, i.mode), (Kind::CreateSubvolume, 0o700));
        }
        let i = item("e /var/cache/krb5rcache - - - 0");
        assert_eq!((i.kind, i.age.unwrap().usec), (Kind::EmptyDirectory, 0));
        let i = item("d /tmp 1777 root root 10d");
        assert_eq!(i.age.unwrap().usec, 10 * 86_400_000_000);
    }

    #[test]
    fn file_lines() {
        let i = item("f /home/liveuser/.config/gnome-initial-setup-done 0644 sshd sys - yes");
        assert_eq!(
            (i.kind, i.plus, i.uid, i.gid),
            (Kind::CreateFile, false, Some(50), Some(2))
        );
        assert_eq!(i.argument.as_deref(), Some(&b"yes"[..]));
        let i = item("f+ /etc/x 0600 - - - a b  c");
        assert!(i.plus);
        assert_eq!(i.argument.as_deref(), Some(&b"a b  c"[..]));
        let i = item("f /etc/empty");
        assert_eq!((i.mode, i.argument), (0o644, None));
        let mut e = env();
        e.boot = true;
        let i = parse_with(&e, r"w+! /etc/doas.conf - - - - permit nopass liveuser\n")
            .unwrap()
            .item
            .unwrap();
        assert_eq!((i.kind, i.plus, i.boot), (Kind::WriteFile, true, true));
        assert_eq!(
            i.argument.as_deref(),
            Some(&b"permit nopass liveuser\n"[..])
        );
        let i = item("w- /proc/sys/vm/swappiness - - - - 10");
        assert!(i.allow_failure);
        let i = item("f~ /root/.ssh/authorized_keys 0600 root root - aGVsbG8K");
        assert_eq!(i.argument.as_deref(), Some(&b"hello\n"[..]));
        // Quotes in the argument are kept: only the first six fields unquote.
        let i = item(r#"f /x - - - - "quoted""#);
        assert_eq!(i.argument.as_deref(), Some(&br#""quoted""#[..]));
        let i = item("F /x - - - - y");
        assert!(i.plus);
    }

    #[test]
    fn link_copy_device_fifo_lines() {
        let i = item("L /var/lib/dbus/machine-id - - - - /etc/machine-id");
        assert_eq!(
            (i.kind, i.argument.as_deref()),
            (Kind::CreateSymlink, Some(&b"/etc/machine-id"[..]))
        );
        let i = item("L+ /var/run - - - - ../run");
        assert!(i.plus);
        let i = item("L? /etc/os-release");
        assert!(i.only_if_target_exists);
        assert_eq!(
            i.argument.as_deref(),
            Some(&b"/usr/share/factory/etc/os-release"[..])
        );
        let i = item("C /etc/pam.d - - - -");
        assert_eq!(
            i.argument.as_deref(),
            Some(&b"/usr/share/factory/etc/pam.d"[..])
        );
        let i = item("C+ /etc/skel - - - - /usr/share/skel");
        assert_eq!(
            (i.kind, i.plus, i.argument.as_deref()),
            (Kind::CopyFiles, true, Some(&b"/usr/share/skel"[..]))
        );
        let i = item("c /dev/fuse 0666 - - - 10:229");
        assert_eq!(
            (i.kind, i.major_minor, i.mode),
            (Kind::CreateCharDevice, Some((10, 229)), 0o666)
        );
        let i = item("b /dev/loop0 0660 - - - 7:0");
        assert_eq!(i.major_minor, Some((7, 0)));
        let i = item("p+ /run/fifo 0600 - - -");
        assert_eq!((i.kind, i.plus), (Kind::CreateFifo, true));
    }

    #[test]
    fn adjust_remove_ignore_xattr_lines() {
        let i = item("z /usr/sbin/postdrop 2755 root systemd-journal -");
        assert_eq!(
            (i.kind, i.mode, i.gid),
            (Kind::AdjustMode, 0o2755, Some(23))
        );
        let i = item("z /etc/x - - -");
        assert!(!i.mode_set && i.uid.is_none());
        let i = item("m /etc/x 0600");
        assert_eq!((i.kind, i.letter), (Kind::AdjustMode, 'z'));
        assert_eq!(
            item("Z /var/lib/x ~0640 - -").kind,
            Kind::RecursiveAdjustMode
        );
        assert!(item("Z /var/lib/x ~0640 - -").mode_mask);
        assert_eq!(item("r /tmp/.X[0-9]*-lock").kind, Kind::RemovePath);
        assert_eq!(item("R /var/tmp/junk").kind, Kind::RecursiveRemovePath);
        assert_eq!(item("x /tmp/systemd-private-*").kind, Kind::IgnorePath);
        assert_eq!(item("X /tmp/keep").kind, Kind::IgnoreDirectoryPath);
        let i = item(
            r#"t /run/cups - - - - security.SMACK64=printing user.attr-with-spaces="foo bar""#,
        );
        assert_eq!(
            i.xattrs,
            vec![
                ("security.SMACK64".into(), b"printing".to_vec()),
                ("user.attr-with-spaces".into(), b"foo bar".to_vec())
            ]
        );
        assert_eq!(item("T /x - - - - user.a=b").kind, Kind::RecursiveSetXattr);
    }

    #[test]
    fn modifiers() {
        let i = item("d=- /var/lib/x");
        assert!(i.try_replace && i.allow_failure);
        assert!(item("f$ /x").purge);
        assert!(err("d!! /x").contains("Unknown modifiers"));
        assert!(err("d& /x").contains("Unknown modifiers"));
        assert!(err("d? /x").contains("Unknown modifiers"));
        assert!(err("d~ /x").contains("only supported on f and w"));
        assert!(err("L~ /x - - - - eA==").contains("base64"));
        // "!" lines are skipped without --boot.
        let p = parse_with(&env(), "r! /tmp/.X0-lock").unwrap();
        assert!(p.item.is_none() && p.skipped.is_some());
    }

    #[test]
    fn mode_owner_and_age_prefixes() {
        let i = item("d /x :0700 :sshd :systemd-journal");
        assert!(i.mode_only_create && i.uid_only_create && i.gid_only_create);
        assert_eq!((i.mode, i.uid, i.gid), (0o700, Some(50), Some(23)));
        let i = item("d /x ~:0755");
        assert!(i.mode_mask && i.mode_only_create);
        assert_eq!(item("d /x - 1000 2000").uid, Some(1000));
        let a = item("d /x - - - ~1h").age.unwrap();
        assert!(a.keep_first_level);
        assert_eq!(a.usec, 3_600_000_000);
        let a = item("d /x - - - bmA:1h").age.unwrap();
        assert_eq!(
            (a.by_file, a.by_dir),
            (AgeBy(AgeBy::BTIME | AgeBy::MTIME), AgeBy(AgeBy::ATIME))
        );
        assert!(err("d /x 0999").contains("Invalid mode"));
        assert!(err("d /x 17777").contains("Invalid mode"));
        assert!(err("d /x - - - 10fortnights").contains("Invalid age"));
        assert!(err("d /x - - - q:1h").contains("Invalid age"));
        assert!(err("d /x - nosuchuser").contains("no such user"));
        assert!(err("d /x - - nosuchgroup").contains("no such group"));
        let mut e = env();
        e.graceful = true;
        assert!(parse_with(&e, "d /x - nosuchuser").unwrap().item.is_none());
    }

    #[test]
    fn ages() {
        assert_eq!(
            parse_sec("10d12h"),
            Some((10 * 86_400 + 12 * 3_600) * 1_000_000)
        );
        assert_eq!(parse_sec("1h30min"), Some(5_400_000_000));
        assert_eq!(parse_sec("1.5h"), Some(5_400_000_000));
        assert_eq!(parse_sec("90"), Some(90_000_000));
        assert_eq!(parse_sec("500ms"), Some(500_000));
        assert_eq!(parse_sec("2w"), Some(14 * 86_400_000_000));
        assert_eq!(parse_sec("0"), Some(0));
        assert_eq!(parse_sec("1 h"), Some(3_600_000_000));
        assert_eq!(parse_sec("infinity"), Some(u64::MAX));
        assert_eq!(parse_sec("5 fortnights"), None);
        assert_eq!(parse_sec(""), None);
    }

    #[test]
    fn specifiers_in_path_and_argument() {
        assert_eq!(
            item("d /var/log/journal/%m 2755 root systemd-journal -").path,
            "/var/log/journal/0123456789abcdef0123456789abcdef"
        );
        let i = item("f /run/%H-%v-%b - - - - %h 100%%");
        assert_eq!(i.path, "/run/box-6.12.0-fedcba9876543210fedcba9876543210");
        assert_eq!(i.argument.as_deref(), Some(&b"/root 100%"[..]));
        let i = item("L /run/x - - - - %t/target");
        assert_eq!(i.argument.as_deref(), Some(&b"/run/target"[..]));
        assert!(err("d /run/%Y").contains("unknown or unsupported specifier '%Y'"));
        // %m before /etc/machine-id exists: the line is skipped, not invalid.
        let tmp = std::env::temp_dir().join(format!("tmpfiles-parse-nomid-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut e = env();
        e.spec = Specifiers::new(&Root::new(Some(&tmp)).unwrap(), true);
        assert!(matches!(
            parse_with(&e, "d /var/log/journal/%m"),
            Err(LineError::Unresolvable(_))
        ));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn invalid_lines() {
        assert!(err("d").contains("Syntax error"));
        assert!(err("d relative/path").contains("not absolute"));
        assert!(err("y /x").contains("Unknown command type"));
        assert!(err("a+ /var/log/journal - - - - d:group::r-x").contains("not supported"));
        assert!(err("h /x - - - - +C").contains("not supported"));
        assert!(err("w /x").contains("requires argument"));
        assert!(err("c /dev/x 0600 - - - 10").contains("major/minor"));
        assert!(err("C /x - - - - relative").contains("not absolute"));
        assert!(err(r#"d "/x"#).contains("Failed to parse line"));
        assert!(err(r"f /x - - - - bad\q").contains("unescape"));
        assert!(err("t /x - - - - noequals").contains("name=value"));
    }

    #[test]
    fn warnings_paths_and_prefixes() {
        let p = parse_with(&env(), "d /run//x/./y/ - - - - extra").unwrap();
        assert_eq!(p.item.as_ref().unwrap().path, "/run/x/y");
        assert_eq!(p.item.unwrap().argument, None);
        assert!(p.warnings[0].contains("don't take argument"));
        let p = parse_with(&env(), "d /var/run/foo").unwrap();
        assert_eq!(p.item.unwrap().path, "/run/foo");
        assert!(p.warnings[0].contains("legacy directory"));
        assert_eq!(item("L /var/run - - - - ../run").path, "/var/run");
        let mut e = env();
        e.exclude = vec!["/run".into(), "/dev".into()];
        assert!(parse_with(&e, "d /run/x").unwrap().item.is_none());
        assert!(parse_with(&e, "d /runx").unwrap().item.is_some());
        e.include = vec!["/var/lib".into()];
        assert!(parse_with(&e, "d /var/lib/x").unwrap().item.is_some());
        assert!(parse_with(&e, "d /var/log/x").unwrap().item.is_none());
    }
}
