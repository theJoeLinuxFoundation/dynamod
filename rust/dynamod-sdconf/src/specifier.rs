//! `%x` specifiers, as tmpfiles.d(5) and sysusers.d(5) list them (for the
//! system instance: `%u` is `root`, `%h` is `/root`, ...).
//!
//! Values that describe the installed system (`%m` from `/etc/machine-id`,
//! `%o` and friends from `os-release`) are read inside the root; values that
//! describe the running machine (`%H`, `%v`, `%b`) come from the running
//! kernel, as with systemd.
use std::cell::RefCell;
use std::collections::HashMap;

use crate::rootfs::Root;

/// Specifiers both tools understand.
pub const COMMON: &str = "aAbBHlmMoqTvVwW";
/// Specifiers only tmpfiles.d understands.
pub const TMPFILES_ONLY: &str = "CDgGhLStuU";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// Not a specifier this tool knows: the line is invalid.
    Unknown(char),
    /// A known specifier with no value yet, such as `%m` before
    /// `/etc/machine-id` exists: systemd-tmpfiles skips such lines.
    Unresolvable(char, String),
    /// A known specifier whose value could not be determined.
    Failed(char, String),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::Unknown(c) => write!(f, "unknown or unsupported specifier '%{c}'"),
            SpecError::Unresolvable(c, why) => {
                write!(f, "specifier '%{c}' cannot be resolved yet: {why}")
            }
            SpecError::Failed(c, why) => write!(f, "failed to resolve specifier '%{c}': {why}"),
        }
    }
}

pub struct Specifiers {
    root: Root,
    known: String,
    cache: RefCell<HashMap<char, Result<String, SpecError>>>,
    overrides: HashMap<char, String>,
}

impl Specifiers {
    /// The specifiers of tmpfiles.d (`tmpfiles`) or of sysusers.d.
    pub fn new(root: &Root, tmpfiles: bool) -> Specifiers {
        let mut known = COMMON.to_string();
        if tmpfiles {
            known.push_str(TMPFILES_ONLY);
        }
        Specifiers {
            root: root.clone(),
            known,
            cache: RefCell::new(HashMap::new()),
            overrides: HashMap::new(),
        }
    }

    /// Fix the value of a specifier (tests).
    pub fn with_value(mut self, c: char, v: &str) -> Specifiers {
        self.overrides.insert(c, v.to_string());
        self
    }

    /// Expand the specifiers in `s`. `%%` is a percent sign; a lone `%` at
    /// the very end is kept.
    pub fn expand(&self, s: &str) -> Result<String, SpecError> {
        let bytes = self.expand_bytes(s.as_bytes())?;
        Ok(String::from_utf8(bytes).expect("expansion of UTF-8 stays UTF-8"))
    }

    /// Same, on bytes (arguments after C unescaping need not be UTF-8).
    pub fn expand_bytes(&self, s: &[u8]) -> Result<Vec<u8>, SpecError> {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if s[i] != b'%' {
                out.push(s[i]);
                i += 1;
                continue;
            }
            let Some(&n) = s.get(i + 1) else {
                out.push(b'%');
                break;
            };
            if n == b'%' {
                out.push(b'%');
            } else {
                // Multi-byte characters are never specifiers.
                let c = if n.is_ascii() { n as char } else { '\u{fffd}' };
                out.extend_from_slice(self.value(c)?.as_bytes());
            }
            i += 2;
        }
        Ok(out)
    }

    fn value(&self, c: char) -> Result<String, SpecError> {
        if !self.known.contains(c) {
            return Err(SpecError::Unknown(c));
        }
        if let Some(v) = self.overrides.get(&c) {
            return Ok(v.clone());
        }
        if let Some(v) = self.cache.borrow().get(&c) {
            return v.clone();
        }
        let v = self.compute(c);
        self.cache.borrow_mut().insert(c, v.clone());
        v
    }

    fn compute(&self, c: char) -> Result<String, SpecError> {
        Ok(match c {
            'a' => architecture().to_string(),
            'A' => self.os_release("IMAGE_VERSION"),
            'B' => self.os_release("BUILD_ID"),
            'M' => self.os_release("IMAGE_ID"),
            'o' => self.os_release("ID"),
            'w' => self.os_release("VERSION_ID"),
            'W' => self.os_release("VARIANT_ID"),
            'b' => boot_id().map_err(|e| SpecError::Failed(c, e))?,
            'H' => hostname(),
            'l' => short_hostname(&hostname()),
            'q' => self.pretty_hostname(),
            'm' => self.machine_id(c)?,
            'v' => uname_release().map_err(|e| SpecError::Failed(c, e))?,
            'T' => tmp_dir("/tmp"),
            'V' => tmp_dir("/var/tmp"),
            'C' => "/var/cache".into(),
            'D' => "/usr/share".into(),
            'L' => "/var/log".into(),
            'S' => "/var/lib".into(),
            't' => "/run".into(),
            'h' => "/root".into(),
            'g' | 'u' => "root".into(),
            'G' | 'U' => "0".into(),
            _ => return Err(SpecError::Unknown(c)),
        })
    }

    fn read_root_file(&self, path: &str) -> Option<String> {
        let host = self.root.resolve(path, true).ok()?;
        std::fs::read_to_string(host).ok()
    }

    fn os_release(&self, key: &str) -> String {
        let text = self
            .read_root_file("/etc/os-release")
            .or_else(|| self.read_root_file("/usr/lib/os-release"));
        text.and_then(|t| parse_env(&t).remove(key))
            .unwrap_or_default()
    }

    fn pretty_hostname(&self) -> String {
        self.read_root_file("/etc/machine-info")
            .and_then(|t| parse_env(&t).remove("PRETTY_HOSTNAME"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| short_hostname(&hostname()))
    }

    fn machine_id(&self, c: char) -> Result<String, SpecError> {
        let Some(text) = self.read_root_file("/etc/machine-id") else {
            return Err(SpecError::Unresolvable(
                c,
                "/etc/machine-id does not exist".into(),
            ));
        };
        let id = text.trim();
        if id.is_empty() || id == "uninitialized" {
            return Err(SpecError::Unresolvable(
                c,
                "/etc/machine-id is not initialized".into(),
            ));
        }
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SpecError::Failed(
                c,
                "/etc/machine-id is not a valid machine ID".into(),
            ));
        }
        Ok(id.to_ascii_lowercase())
    }
}

/// Parse an os-release style `KEY=value` file (values may be quoted).
pub fn parse_env(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let mut rest = v;
        let flags = crate::words::Flags {
            unquote: true,
            cunescape: false,
        };
        let value = match crate::words::next_word(&mut rest, flags) {
            Ok(Some(w)) if rest.is_empty() => w,
            _ => v.to_string(),
        };
        m.insert(k.trim().to_string(), value);
    }
    m
}

/// systemd's name for the architecture (`ConditionArchitecture=`).
pub fn architecture() -> &'static str {
    let little = cfg!(target_endian = "little");
    match std::env::consts::ARCH {
        "x86_64" => "x86-64",
        "x86" => "x86",
        "aarch64" if little => "arm64",
        "aarch64" => "arm64-be",
        "arm" if little => "arm",
        "arm" => "arm-be",
        "riscv64" => "riscv64",
        "riscv32" => "riscv32",
        "powerpc64" if little => "ppc64-le",
        "powerpc64" => "ppc64",
        "powerpc" => "ppc",
        "s390x" => "s390x",
        "loongarch64" => "loongarch64",
        "mips64" if little => "mips64-le",
        "mips64" => "mips64",
        "mips" if little => "mips-le",
        "mips" => "mips",
        "sparc64" => "sparc64",
        other => other,
    }
}

fn utsname() -> Option<libc::utsname> {
    // SAFETY: uname() fills the structure it is given.
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) == 0 {
            Some(u)
        } else {
            None
        }
    }
}

fn field(f: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = f
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn uname_release() -> Result<String, String> {
    utsname()
        .map(|u| field(&u.release))
        .ok_or_else(|| "uname() failed".to_string())
}

/// The running system's host name (`localhost` if it has none).
pub fn hostname() -> String {
    let h = utsname().map(|u| field(&u.nodename)).unwrap_or_default();
    if h.is_empty() || h == "(none)" {
        "localhost".into()
    } else {
        h
    }
}

fn short_hostname(h: &str) -> String {
    h.split('.').next().unwrap_or(h).to_string()
}

fn boot_id() -> Result<String, String> {
    let t = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|e| format!("/proc/sys/kernel/random/boot_id: {e}"))?;
    let id: String = t.trim().chars().filter(|&c| c != '-').collect();
    if id.len() != 32 {
        return Err("invalid boot ID".into());
    }
    Ok(id.to_ascii_lowercase())
}

fn tmp_dir(default: &str) -> String {
    for var in ["TMPDIR", "TEMP", "TMP"] {
        if let Ok(v) = std::env::var(var)
            && v.starts_with('/')
        {
            return v;
        }
    }
    default.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_with(files: &[(&str, &str)]) -> (std::path::PathBuf, Root) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("sdconf-spec-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (p, c) in files {
            let full = d.join(p.trim_start_matches('/'));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, c).unwrap();
        }
        std::fs::create_dir_all(&d).unwrap();
        let r = Root::new(Some(&d)).unwrap();
        (d, r)
    }

    #[test]
    fn expands_common_specifiers() {
        let (d, root) = root_with(&[
            ("/etc/machine-id", "0123456789ABCDEF0123456789abcdef\n"),
            ("/usr/lib/os-release", "ID=joe\nVERSION_ID=\"1.0\"\n"),
        ]);
        let s = Specifiers::new(&root, true);
        assert_eq!(
            s.expand("/var/log/journal/%m").unwrap(),
            "/var/log/journal/0123456789abcdef0123456789abcdef"
        );
        assert_eq!(
            s.expand("100%% %o-%w %h %u:%G %t").unwrap(),
            "100% joe-1.0 /root root:0 /run"
        );
        assert_eq!(s.expand("x%").unwrap(), "x%");
        assert_eq!(s.expand("%H").unwrap(), hostname());
        assert!(!s.expand("%v").unwrap().is_empty());
        assert_eq!(s.expand("%b").unwrap().len(), 32);
        assert_eq!(s.expand("%Y"), Err(SpecError::Unknown('Y')));
        // sysusers.d does not know %h.
        let su = Specifiers::new(&root, false);
        assert_eq!(su.expand("%h"), Err(SpecError::Unknown('h')));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn missing_machine_id_is_unresolvable() {
        let (d, root) = root_with(&[]);
        let s = Specifiers::new(&root, true);
        assert!(matches!(
            s.expand("/x/%m"),
            Err(SpecError::Unresolvable('m', _))
        ));
        let (d2, root2) = root_with(&[("/etc/machine-id", "uninitialized\n"), ("/etc/x", "")]);
        assert!(matches!(
            Specifiers::new(&root2, true).expand("%m"),
            Err(SpecError::Unresolvable('m', _))
        ));
        std::fs::remove_dir_all(&d).unwrap();
        std::fs::remove_dir_all(&d2).unwrap();
    }
}
