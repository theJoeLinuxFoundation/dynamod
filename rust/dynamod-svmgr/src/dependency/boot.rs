/// Which services start at boot.
///
/// Every enabled service, plus whatever an enabled service `requires` or
/// `wants` (transitively), even if that one is disabled. Disabled services
/// that nothing pulls in stay loaded but stopped until started explicitly.
use std::collections::{HashMap, HashSet, VecDeque};

use crate::config::service::ServiceDef;

/// The names of the services to start at boot, given which are enabled.
pub fn boot_set(defs: &[ServiceDef], is_enabled: impl Fn(&ServiceDef) -> bool) -> HashSet<String> {
    let by_name: HashMap<&str, &ServiceDef> =
        defs.iter().map(|d| (d.service.name.as_str(), d)).collect();

    let mut set = HashSet::new();
    let mut queue: VecDeque<&str> = defs
        .iter()
        .filter(|d| is_enabled(d))
        .map(|d| d.service.name.as_str())
        .collect();

    while let Some(name) = queue.pop_front() {
        if !set.insert(name.to_string()) {
            continue;
        }
        let Some(def) = by_name.get(name) else {
            continue;
        };
        for dep in def
            .dependencies
            .requires
            .iter()
            .chain(&def.dependencies.wants)
        {
            if by_name.contains_key(dep.as_str()) && !set.contains(dep) {
                queue.push_back(dep);
            }
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(
        name: &str,
        enabled: bool,
        requires: &[&str],
        wants: &[&str],
        after: &[&str],
    ) -> ServiceDef {
        let list = |v: &[&str]| format!("{:?}", v);
        let t = format!(
            "[service]\nname = \"{name}\"\nexec = [\"/bin/true\"]\nenabled = {enabled}\n\n[dependencies]\nrequires = {}\nwants = {}\nafter = {}\n",
            list(requires),
            list(wants),
            list(after)
        );
        toml::from_str(&t).unwrap()
    }

    fn names(set: &HashSet<String>) -> Vec<String> {
        let mut v: Vec<String> = set.iter().cloned().collect();
        v.sort();
        v
    }

    #[test]
    fn enabled_services_and_their_requires_and_wants() {
        let defs = vec![
            svc("core", true, &["lib"], &["log"], &["ordering-only"]),
            svc("lib", false, &["deep"], &[], &[]),
            svc("deep", false, &[], &[], &[]),
            svc("log", false, &[], &[], &[]),
            svc("ordering-only", false, &[], &[], &[]),
            svc("optional", false, &[], &[], &[]),
        ];
        let set = boot_set(&defs, |d| d.service.enabled);
        assert_eq!(names(&set), vec!["core", "deep", "lib", "log"]);
    }

    #[test]
    fn markers_decide_through_the_predicate() {
        let defs = vec![
            svc("a", true, &[], &[], &[]),
            svc("b", false, &[], &[], &[]),
        ];
        // As if /etc/dynamod/disabled/a and /etc/dynamod/enabled/b existed.
        let set = boot_set(&defs, |d| d.service.name == "b");
        assert_eq!(names(&set), vec!["b"]);
    }

    #[test]
    fn unknown_dependencies_are_ignored() {
        let defs = vec![svc("a", true, &["missing"], &["also-missing"], &[])];
        assert_eq!(names(&boot_set(&defs, |d| d.service.enabled)), vec!["a"]);
    }
}
