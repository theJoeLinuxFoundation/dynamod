//! Shell globs for the line types that take them (`w`, `e`, `x`, `X`, `r`,
//! `R`, `z`, `Z`, `t`, `T`): `*`, `?`, `[...]` and `{a,b}`, as glob(3) with
//! `GLOB_BRACE` does them, evaluated inside the root.
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use dynamod_sdconf::Root;

pub fn has_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Expand `{a,b}` alternatives (nested ones too) into separate patterns.
pub fn expand_braces(p: &str) -> Vec<String> {
    let b = p.as_bytes();
    let mut depth = 0;
    let mut open = None;
    let mut commas = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'{' => {
                if depth == 0 {
                    open = Some(i);
                    commas.clear();
                }
                depth += 1;
            }
            b',' if depth == 1 => commas.push(i),
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let o = open.unwrap();
                    if commas.is_empty() {
                        // "{x}" is not an alternative; keep it and look further.
                        open = None;
                    } else {
                        let (pre, post) = (&p[..o], &p[i + 1..]);
                        let mut parts = Vec::new();
                        let mut start = o + 1;
                        for &c in &commas {
                            parts.push(&p[start..c]);
                            start = c + 1;
                        }
                        parts.push(&p[start..i]);
                        let mut out = Vec::new();
                        for part in parts {
                            out.extend(expand_braces(&format!("{pre}{part}{post}")));
                        }
                        return out;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    vec![p.to_string()]
}

/// Match one path component against one pattern component (no `/` in
/// either). A leading `.` must be matched explicitly.
pub fn fnmatch(pat: &str, name: &[u8]) -> bool {
    if name.first() == Some(&b'.') && !pat.starts_with('.') && !pat.starts_with("\\.") {
        return false;
    }
    match_here(pat.as_bytes(), name)
}

fn match_here(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star_p, mut star_s) = (None, 0);
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star_p = Some(pi);
                    star_s = si;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                b'[' => {
                    if let Some((ok, len)) = bracket(&p[pi..], s[si]) {
                        if ok {
                            pi += len;
                            si += 1;
                            continue;
                        }
                    } else if s[si] == b'[' {
                        // An unterminated '[' is literal.
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                b'\\' if pi + 1 < p.len() => {
                    if p[pi + 1] == s[si] {
                        pi += 2;
                        si += 1;
                        continue;
                    }
                }
                c => {
                    if c == s[si] {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
            }
        }
        match star_p {
            Some(sp) => {
                pi = sp + 1;
                star_s += 1;
                si = star_s;
            }
            None => return false,
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// `[...]` at the start of `p`: whether `c` matches, and the length of the
/// bracket expression; `None` if it is not terminated.
fn bracket(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(i), Some(b'!') | Some(b'^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    loop {
        let &x = p.get(i)?;
        if x == b']' && !first {
            i += 1;
            break;
        }
        first = false;
        if x == b'[' && p.get(i + 1) == Some(&b':') {
            let end = p[i + 2..].windows(2).position(|w| w == b":]")? + i + 2;
            let class = &p[i + 2..end];
            let ok = match class {
                b"alnum" => c.is_ascii_alphanumeric(),
                b"alpha" => c.is_ascii_alphabetic(),
                b"digit" => c.is_ascii_digit(),
                b"lower" => c.is_ascii_lowercase(),
                b"upper" => c.is_ascii_uppercase(),
                b"space" => c.is_ascii_whitespace(),
                b"xdigit" => c.is_ascii_hexdigit(),
                b"punct" => c.is_ascii_punctuation(),
                b"blank" => c == b' ' || c == b'\t',
                b"cntrl" => c.is_ascii_control(),
                b"print" => (0x20..0x7f).contains(&c),
                b"graph" => c.is_ascii_graphic(),
                _ => false,
            };
            matched |= ok;
            i = end + 2;
            continue;
        }
        let lo = if x == b'\\' {
            i += 1;
            *p.get(i)?
        } else {
            x
        };
        if p.get(i + 1) == Some(&b'-') && p.get(i + 2).is_some_and(|&e| e != b']') {
            let hi = p[i + 2];
            matched |= lo <= c && c <= hi;
            i += 3;
        } else {
            matched |= lo == c;
            i += 1;
        }
    }
    Some((matched != negate, i))
}

/// Match a whole absolute path against a pattern, component by component
/// (`fnmatch()` with `FNM_PATHNAME|FNM_PERIOD`). Braces are expanded.
pub fn path_match(pattern: &str, path: &str) -> bool {
    let pc: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    expand_braces(pattern).iter().any(|pat| {
        let qc: Vec<&str> = pat.split('/').filter(|c| !c.is_empty()).collect();
        qc.len() == pc.len() && qc.iter().zip(&pc).all(|(q, p)| fnmatch(q, p.as_bytes()))
    })
}

fn unescape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// What a glob matched: the path inside the root and where it is on the
/// host (its parent resolved, the last component itself not followed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub path: String,
    pub host: PathBuf,
}

/// Everything that exists and matches `pattern` (a literal path matches
/// itself if it exists, dangling symlinks included), sorted.
pub fn glob(root: &Root, pattern: &str) -> Vec<Match> {
    let mut out: Vec<Match> = Vec::new();
    for pat in expand_braces(pattern) {
        glob_one(root, &pat, &mut out);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    out
}

fn glob_one(root: &Root, pattern: &str, out: &mut Vec<Match>) {
    let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        out.push(Match {
            path: "/".into(),
            host: root.dir().to_path_buf(),
        });
        return;
    }
    // (path inside the root, resolved host directory)
    let mut cur: Vec<(String, PathBuf)> = vec![(String::new(), root.dir().to_path_buf())];
    for (n, comp) in comps.iter().enumerate() {
        let last = n + 1 == comps.len();
        let mut next = Vec::new();
        for (cp, hp) in &cur {
            let names: Vec<Vec<u8>> = if has_glob(comp) {
                let Ok(rd) = std::fs::read_dir(hp) else {
                    continue;
                };
                let mut v: Vec<Vec<u8>> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().as_bytes().to_vec())
                    .filter(|name| fnmatch(comp, name))
                    .collect();
                v.sort();
                v
            } else {
                vec![unescape_literal(comp).into_bytes()]
            };
            for name in names {
                let name_s = String::from_utf8_lossy(&name).into_owned();
                let np = format!("{cp}/{name_s}");
                let cand = hp.join(OsStr::from_bytes(&name));
                let Ok(md) = std::fs::symlink_metadata(&cand) else {
                    continue;
                };
                if last {
                    next.push((np, cand));
                } else {
                    let dir = if md.file_type().is_symlink() {
                        match root.resolve(&np, true) {
                            Ok(r) => r,
                            Err(_) => continue,
                        }
                    } else {
                        cand
                    };
                    if dir.is_dir() {
                        next.push((np, dir));
                    }
                }
            }
        }
        cur = next;
    }
    out.extend(cur.into_iter().map(|(path, host)| Match { path, host }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn braces() {
        assert_eq!(expand_braces("/a/{b,c}/d"), ["/a/b/d", "/a/c/d"]);
        assert_eq!(expand_braces("/{x,y{1,2}}"), ["/x", "/y1", "/y2"]);
        assert_eq!(expand_braces("/plain"), ["/plain"]);
        assert_eq!(expand_braces("/a{b}"), ["/a{b}"]);
    }

    #[test]
    fn matching() {
        assert!(fnmatch("*.pid", b"dnf.pid"));
        assert!(!fnmatch("*.pid", b".hidden.pid"));
        assert!(fnmatch(".*", b".hidden"));
        assert!(fnmatch(".X[0-9]*-lock", b".X0-lock"));
        assert!(fnmatch(".X[0-9]*-lock", b".X12-lock"));
        assert!(!fnmatch(".X[0-9]*-lock", b".Xa-lock"));
        assert!(fnmatch("a?c", b"abc"));
        assert!(fnmatch("[!a]x", b"bx"));
        assert!(!fnmatch("[!a]x", b"ax"));
        assert!(fnmatch("[[:digit:]]", b"7"));
        assert!(fnmatch("a\\*", b"a*"));
        assert!(!fnmatch("a\\*", b"ab"));
        assert!(fnmatch("*", b"anything"));
        assert!(fnmatch("a*b*c", b"axxbyyc"));
        assert!(!fnmatch("a*b*c", b"axxbyy"));
        assert!(path_match(
            "/tmp/systemd-private-*",
            "/tmp/systemd-private-abc"
        ));
        assert!(!path_match("/tmp/*", "/tmp/a/b"));
        assert!(path_match("/var/{cache,lib}/x", "/var/lib/x"));
    }

    #[test]
    fn globbing_inside_a_root() {
        let d = std::env::temp_dir().join(format!("tmpfiles-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("real/a")).unwrap();
        std::fs::write(d.join("real/a/x.lock"), "").unwrap();
        std::fs::write(d.join("real/a/y.lock"), "").unwrap();
        std::fs::write(d.join("real/a/.z.lock"), "").unwrap();
        std::os::unix::fs::symlink("/real", d.join("link")).unwrap();
        let root = Root::new(Some(&d)).unwrap();
        let m = glob(&root, "/link/*/*.lock");
        let paths: Vec<_> = m.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["/link/a/x.lock", "/link/a/y.lock"]);
        assert_eq!(m[0].host, root.dir().join("real/a/x.lock"));
        assert_eq!(glob(&root, "/real/a/x.lock").len(), 1);
        assert!(glob(&root, "/real/a/none").is_empty());
        assert_eq!(glob(&root, "/real/a/{x,y}.lock").len(), 2);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
