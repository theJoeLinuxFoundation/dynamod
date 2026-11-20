//! All lines of all files, arranged the way systemd-tmpfiles applies them.
//!
//! Lines are grouped by path: lines without globs in one ordered map, lines
//! with globs in another (applied second). For one path, the first file (in
//! name order) wins; a later line that conflicts with it is ignored, unless
//! it has `+`. Each group knows its closest ancestor group, so that a parent
//! is created before its children and removed after them.
use std::collections::HashMap;

use dynamod_sdconf::confdirs::ConfFile;
use dynamod_sdconf::{log_debug, log_error, log_notice, log_warning};

use crate::item::Item;
use crate::parse::{LineError, ParseCtx, parse_line};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Map {
    Items,
    Globs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArrayRef {
    pub map: Map,
    pub index: usize,
}

#[derive(Debug, Default)]
pub struct Array {
    pub path: String,
    pub items: Vec<Item>,
    pub parent: Option<ArrayRef>,
    pub children: Vec<ArrayRef>,
}

#[derive(Debug, Default)]
pub struct Config {
    pub items: Vec<Array>,
    pub globs: Vec<Array>,
    item_index: HashMap<String, usize>,
    glob_index: HashMap<String, usize>,
    /// Some line was invalid (exit status 65).
    pub invalid: bool,
    /// Some file could not be read.
    pub unreadable: bool,
}

fn order_key(i: &Item) -> (bool, char) {
    (!i.kind.takes_ownership(), i.letter)
}

impl Config {
    pub fn array(&self, r: ArrayRef) -> &Array {
        match r.map {
            Map::Items => &self.items[r.index],
            Map::Globs => &self.globs[r.index],
        }
    }

    pub fn has_item_path(&self, path: &str) -> bool {
        self.item_index.contains_key(path)
    }

    /// Add a line. Returns a notice when it is ignored as a duplicate.
    pub fn add(&mut self, item: Item) -> Option<String> {
        let glob = item.kind.takes_glob();
        let (arrays, index) = if glob {
            (&mut self.globs, &mut self.glob_index)
        } else {
            (&mut self.items, &mut self.item_index)
        };
        match index.get(&item.path) {
            Some(&n) => {
                let a = &mut arrays[n];
                if !item.plus && a.items.iter().any(|e| !e.compatible(&item)) {
                    return Some(format!(
                        "{}: Duplicate line for path \"{}\", ignoring.",
                        item.origin, item.path
                    ));
                }
                a.items.push(item);
                a.items.sort_by_key(order_key);
            }
            None => {
                index.insert(item.path.clone(), arrays.len());
                arrays.push(Array {
                    path: item.path.clone(),
                    items: vec![item],
                    parent: None,
                    children: Vec::new(),
                });
            }
        }
        None
    }

    /// Link every non-glob path to its closest ancestor that has lines
    /// (systemd's `link_parent()`).
    pub fn link_parents(&mut self) {
        for n in 0..self.items.len() {
            let path = self.items[n].path.clone();
            let mut p = path.as_str();
            while let Some(pos) = p.rfind('/') {
                p = if pos == 0 { "/" } else { &p[..pos] };
                if p == path {
                    break;
                }
                let parent = if let Some(&i) = self.item_index.get(p) {
                    Some(ArrayRef {
                        map: Map::Items,
                        index: i,
                    })
                } else {
                    self.glob_index.get(p).map(|&i| ArrayRef {
                        map: Map::Globs,
                        index: i,
                    })
                };
                if let Some(pr) = parent {
                    let me = ArrayRef {
                        map: Map::Items,
                        index: n,
                    };
                    self.items[n].parent = Some(pr);
                    match pr.map {
                        Map::Items => self.items[pr.index].children.push(me),
                        Map::Globs => self.globs[pr.index].children.push(me),
                    }
                    break;
                }
                if p == "/" {
                    break;
                }
            }
        }
    }

    /// Parse the text of one file into this configuration, reporting
    /// problems as they are found.
    pub fn read_text(
        &mut self,
        name: &str,
        text: &str,
        ctx: &ParseCtx,
        unresolvable_noted: &mut bool,
    ) {
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let origin = crate::item::Origin {
                file: name.to_string(),
                line: n + 1,
            };
            match parse_line(line, ctx) {
                Ok(p) => {
                    for w in &p.warnings {
                        log_warning!("{origin}: {w}");
                    }
                    if let Some(why) = &p.skipped {
                        log_debug!("{origin}: skipping: {why}");
                    }
                    if let Some(mut item) = p.item {
                        item.origin = origin;
                        if let Some(notice) = self.add(item) {
                            log_notice!("{notice}");
                        }
                    }
                }
                Err(LineError::Invalid(m)) => {
                    log_error!("{origin}: {m}");
                    self.invalid = true;
                }
                Err(LineError::Unresolvable(m)) => {
                    if *unresolvable_noted {
                        log_debug!("{origin}: {m}");
                    } else {
                        log_notice!("{origin}: {m}");
                        log_notice!(
                            "All rules containing unresolvable specifiers will be skipped."
                        );
                        *unresolvable_noted = true;
                    }
                }
            }
        }
    }

    pub fn read_file(&mut self, f: &ConfFile, ctx: &ParseCtx, unresolvable_noted: &mut bool) {
        if f.masked {
            log_debug!("{} is masked, skipping", f.path);
            return;
        }
        match f.read() {
            Ok(text) => self.read_text(&f.path, &text, ctx, unresolvable_noted),
            Err(e) => {
                log_error!("Failed to read {}: {e}", f.path);
                self.unreadable = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Kind;

    fn it(kind: Kind, letter: char, path: &str) -> Item {
        Item::new(kind, letter, path)
    }

    #[test]
    fn first_line_wins_and_order_is_stable() {
        let mut c = Config::default();
        let mut a = it(Kind::CreateDirectory, 'd', "/var/lib/x");
        a.mode = 0o700;
        a.mode_set = true;
        assert!(c.add(a).is_none());
        let mut b = it(Kind::CreateDirectory, 'd', "/var/lib/x");
        b.mode = 0o755;
        b.mode_set = true;
        assert!(c.add(b).unwrap().contains("Duplicate line"));
        // An identical line is not a conflict.
        let mut a2 = it(Kind::CreateDirectory, 'd', "/var/lib/x");
        a2.mode = 0o700;
        a2.mode_set = true;
        assert!(c.add(a2).is_none());
        assert_eq!(c.items[0].items.len(), 2);
        // Adjustments sort after the line that owns the path.
        let mut c = Config::default();
        c.add(it(Kind::AdjustMode, 'z', "/x"));
        c.add(it(Kind::RemovePath, 'r', "/x"));
        let letters: Vec<char> = c.globs[0].items.iter().map(|i| i.letter).collect();
        assert_eq!(letters, ['r', 'z']);
        // "+" lines may differ (several w+ lines append in turn).
        let mut c = Config::default();
        let mut w1 = it(Kind::WriteFile, 'w', "/etc/doas.conf");
        w1.plus = true;
        w1.argument = Some(b"a\n".to_vec());
        let mut w2 = w1.clone();
        w2.argument = Some(b"b\n".to_vec());
        c.add(w1);
        assert!(c.add(w2).is_none());
        assert_eq!(c.globs[0].items.len(), 2);
    }

    #[test]
    fn parents_are_linked() {
        let mut c = Config::default();
        c.add(it(Kind::CreateDirectory, 'd', "/var/lib/a/b/c"));
        c.add(it(Kind::CreateDirectory, 'd', "/var/lib/a"));
        c.add(it(Kind::CreateDirectory, 'd', "/"));
        c.add(it(Kind::EmptyDirectory, 'e', "/var/lib/a/b"));
        c.link_parents();
        assert_eq!(
            c.items[0].parent,
            Some(ArrayRef {
                map: Map::Globs,
                index: 0
            })
        );
        assert_eq!(
            c.items[1].parent,
            Some(ArrayRef {
                map: Map::Items,
                index: 2
            })
        );
        assert_eq!(c.items[2].parent, None);
        assert_eq!(
            c.globs[0].children,
            vec![ArrayRef {
                map: Map::Items,
                index: 0
            }]
        );
    }
}
