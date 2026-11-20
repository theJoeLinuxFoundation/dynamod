//! Field splitting and C-style escapes, as systemd's `extract_first_word()`
//! and `cunescape()` do them.
//!
//! Fields are separated by runs of whitespace. With `unquote`, single or
//! double quotes group text (they may start mid-word: `a"b c"` is `ab c`) and
//! are removed. A backslash escapes the next character; with `cunescape` it
//! starts a C escape (`\n`, `\x41`, `\101`, `é`, ...).

const SEPARATORS: &[char] = &[' ', '\t', '\n', '\r'];

fn is_sep(c: char) -> bool {
    SEPARATORS.contains(&c)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    /// Quotes group and are removed.
    pub unquote: bool,
    /// Backslash starts a C escape (otherwise it just protects the next
    /// character).
    pub cunescape: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordError {
    UnbalancedQuote,
    TrailingBackslash,
    BadEscape(String),
}

impl std::fmt::Display for WordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WordError::UnbalancedQuote => write!(f, "unbalanced quotes"),
            WordError::TrailingBackslash => write!(f, "trailing backslash"),
            WordError::BadEscape(s) => write!(f, "invalid escape sequence '\\{s}'"),
        }
    }
}

/// Take the next field from `*rest` and advance `*rest` past it and the
/// whitespace that follows, so that `*rest` then starts at the next field.
/// Returns `None` when only whitespace is left.
pub fn next_word(rest: &mut &str, flags: Flags) -> Result<Option<String>, WordError> {
    let s = rest.trim_start_matches(is_sep);
    if s.is_empty() {
        *rest = s;
        return Ok(None);
    }
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    let bytes_len = s.len();
    let mut end = bytes_len;
    while i < bytes_len {
        let c = s[i..].chars().next().unwrap();
        let clen = c.len_utf8();
        if let Some(q) = quote {
            if c == q {
                quote = None;
                i += clen;
                continue;
            }
            if c == '\\' {
                i += clen;
                i += escape(&s[i..], flags, &mut out)?;
                continue;
            }
            out.push(c);
            i += clen;
            continue;
        }
        if is_sep(c) {
            end = i;
            break;
        }
        if flags.unquote && (c == '"' || c == '\'') {
            quote = Some(c);
            i += clen;
            continue;
        }
        if c == '\\' {
            i += clen;
            i += escape(&s[i..], flags, &mut out)?;
            continue;
        }
        out.push(c);
        i += clen;
    }
    if quote.is_some() {
        return Err(WordError::UnbalancedQuote);
    }
    *rest = s[end.min(bytes_len)..].trim_start_matches(is_sep);
    Ok(Some(out))
}

/// Handle what follows a backslash; returns the number of bytes consumed.
fn escape(s: &str, flags: Flags, out: &mut String) -> Result<usize, WordError> {
    let Some(c) = s.chars().next() else {
        return Err(WordError::TrailingBackslash);
    };
    if !flags.cunescape {
        out.push(c);
        return Ok(c.len_utf8());
    }
    let (bytes, used) = cunescape_one(s)?;
    match String::from_utf8(bytes) {
        Ok(t) => out.push_str(&t),
        // A raw byte (\xff, \377) in a field that must be text.
        Err(_) => return Err(WordError::BadEscape(s[..used].to_string())),
    }
    Ok(used)
}

fn hex(c: u8) -> Option<u32> {
    (c as char).to_digit(16)
}

/// Decode one C escape; `s` starts just after the backslash. Returns the
/// bytes it stands for and how many bytes of `s` it used. NUL is refused.
pub fn cunescape_one(s: &str) -> Result<(Vec<u8>, usize), WordError> {
    let b = s.as_bytes();
    let bad = |n: usize| WordError::BadEscape(s[..n.min(s.len())].to_string());
    let Some(&c) = b.first() else {
        return Err(WordError::TrailingBackslash);
    };
    let simple = |v: u8| Ok((vec![v], 1));
    match c {
        b'a' => simple(7),
        b'b' => simple(8),
        b'f' => simple(12),
        b'n' => simple(b'\n'),
        b'r' => simple(b'\r'),
        b't' => simple(b'\t'),
        b'v' => simple(11),
        b'\\' => simple(b'\\'),
        b'"' => simple(b'"'),
        b'\'' => simple(b'\''),
        b's' => simple(b' '),
        b'x' => {
            let (Some(h), Some(l)) = (
                b.get(1).copied().and_then(hex),
                b.get(2).copied().and_then(hex),
            ) else {
                return Err(bad(3));
            };
            let v = (h << 4 | l) as u8;
            if v == 0 {
                return Err(bad(3));
            }
            Ok((vec![v], 3))
        }
        b'u' | b'U' => {
            let n = if c == b'u' { 4 } else { 8 };
            let mut v: u32 = 0;
            for k in 1..=n {
                let Some(d) = b.get(k).copied().and_then(hex) else {
                    return Err(bad(n + 1));
                };
                v = v << 4 | d;
            }
            match char::from_u32(v) {
                Some(ch) if v != 0 => {
                    let mut buf = [0u8; 4];
                    Ok((ch.encode_utf8(&mut buf).as_bytes().to_vec(), n + 1))
                }
                _ => Err(bad(n + 1)),
            }
        }
        b'0'..=b'7' => {
            let mut v: u32 = 0;
            for k in 0..3 {
                match b.get(k) {
                    Some(d @ b'0'..=b'7') => v = v * 8 + u32::from(d - b'0'),
                    _ => return Err(bad(3)),
                }
            }
            if v == 0 || v > 255 {
                return Err(bad(3));
            }
            Ok((vec![v as u8], 3))
        }
        _ => {
            let n = s.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            Err(bad(n))
        }
    }
}

/// Decode all C escapes in `s` (systemd's `cunescape()` without flags).
pub fn cunescape(s: &str) -> Result<Vec<u8>, WordError> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap();
        if c == '\\' {
            let (bytes, used) = cunescape_one(&s[i + 1..])?;
            out.extend_from_slice(&bytes);
            i += 1 + used;
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            i += c.len_utf8();
        }
    }
    Ok(out)
}

/// Decode standard Base64 (padding optional, whitespace ignored).
pub fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut padding = false;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding = true;
                continue;
            }
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        if padding {
            return None;
        }
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QE: Flags = Flags {
        unquote: true,
        cunescape: true,
    };
    const Q: Flags = Flags {
        unquote: true,
        cunescape: false,
    };

    fn words(s: &str, f: Flags) -> Vec<String> {
        let mut rest = s;
        let mut v = Vec::new();
        while let Some(w) = next_word(&mut rest, f).unwrap() {
            v.push(w);
        }
        v
    }

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(
            words("  d   /run/x\t0755 root ", QE),
            ["d", "/run/x", "0755", "root"]
        );
    }

    #[test]
    fn quotes_group_and_are_removed() {
        assert_eq!(
            words(r#"u sshd 50 "sshd PrivSep" /var/lib/sshd"#, Q),
            ["u", "sshd", "50", "sshd PrivSep", "/var/lib/sshd"]
        );
        assert_eq!(words(r#"a"b c"d 'e f'"#, Q), ["ab cd", "e f"]);
        assert_eq!(words(r#""""#, Q), [""]);
    }

    #[test]
    fn escapes() {
        assert_eq!(words(r"/a\x20b /c\\d", QE), ["/a b", r"/c\d"]);
        assert_eq!(words(r"/a\ b", Q), ["/a b"]);
        assert_eq!(words(r#""x\"y""#, QE), [r#"x"y"#]);
        let mut rest = r"/a\q";
        assert!(matches!(
            next_word(&mut rest, QE),
            Err(WordError::BadEscape(_))
        ));
        let mut rest = r"/a\";
        assert_eq!(next_word(&mut rest, QE), Err(WordError::TrailingBackslash));
    }

    #[test]
    fn unbalanced_quote_is_an_error() {
        let mut rest = r#"u "abc"#;
        next_word(&mut rest, Q).unwrap();
        assert_eq!(next_word(&mut rest, Q), Err(WordError::UnbalancedQuote));
    }

    #[test]
    fn rest_starts_at_next_field() {
        let mut rest = "f /x - - - -   hello  world";
        for _ in 0..6 {
            next_word(&mut rest, QE).unwrap();
        }
        assert_eq!(rest, "hello  world");
    }

    #[test]
    fn cunescape_all() {
        assert_eq!(
            cunescape(r"permit nopass liveuser\n").unwrap(),
            b"permit nopass liveuser\n"
        );
        assert_eq!(cunescape(r"\x41\102é\t").unwrap(), "AB\u{e9}\t".as_bytes());
        assert_eq!(cunescape(r"\xff").unwrap(), vec![0xff]);
        assert!(cunescape(r"\x00").is_err());
        assert!(cunescape(r"abc\").is_err());
        assert!(cunescape(r"\z").is_err());
    }

    #[test]
    fn base64() {
        assert_eq!(unbase64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(unbase64("aGVsbG8").unwrap(), b"hello");
        assert_eq!(unbase64("aGVs\nbG8gd29ybGQK").unwrap(), b"hello world\n");
        assert!(unbase64("a$b").is_none());
    }
}
