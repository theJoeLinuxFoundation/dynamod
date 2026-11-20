/// Dynamic frontier algorithm for dependency-aware parallel service startup.
///
/// Instead of computing a single topological order, we maintain a frontier
/// of services whose dependencies are all satisfied. This maximizes
/// parallelism: services start as soon as their dependencies are ready.
///
/// Algorithm:
/// 1. Compute unmet[s] = count of unready dependencies for each service
/// 2. Frontier = { s : unmet[s] == 0 }
/// 3. When service becomes READY: decrement unmet for all dependents
/// 4. When service FAILS: block all transitive dependents via `requires`
///
/// `after-all` services are held back even when their own dependencies are
/// met, until the frontier has settled: no other service is waiting to start
/// or started-but-not-ready. Services that wait on an after-all service do
/// not count (they cannot progress before it).
use std::collections::{HashMap, HashSet, VecDeque};

use super::graph::DependencyGraph;

/// Tracks the startup frontier state.
#[derive(Debug)]
pub struct StartupFrontier {
    /// Number of unmet dependencies for each service.
    unmet: HashMap<String, usize>,
    /// Services that are ready to start (all deps satisfied).
    ready_to_start: VecDeque<String>,
    /// Services that have been started and are now ready.
    ready: HashSet<String>,
    /// Services that have been started but are not yet ready.
    pending: HashSet<String>,
    /// Services that are blocked (a required dependency failed).
    blocked: HashSet<String>,
    /// Services that have been completed (started and ready, or failed).
    completed: HashSet<String>,
    /// Services with `after-all = true`.
    after_all: HashSet<String>,
    /// After-all services whose dependencies are met, waiting for the rest
    /// of the boot to settle.
    held: Vec<String>,
}

impl StartupFrontier {
    /// Initialize the frontier from a dependency graph.
    pub fn new(graph: &DependencyGraph) -> Self {
        Self::with_after_all(graph, HashSet::new())
    }

    /// Initialize the frontier; the services in `after_all` start only once
    /// everything else has settled.
    pub fn with_after_all(graph: &DependencyGraph, after_all: HashSet<String>) -> Self {
        let mut frontier = Self {
            unmet: HashMap::new(),
            ready_to_start: VecDeque::new(),
            ready: HashSet::new(),
            pending: HashSet::new(),
            blocked: HashSet::new(),
            completed: HashSet::new(),
            after_all,
            held: Vec::new(),
        };

        let mut services: Vec<&String> = graph.all_services().iter().collect();
        services.sort();
        for service in services {
            let count = graph.dependencies_of(service).len();
            frontier.unmet.insert(service.clone(), count);
            if count == 0 {
                frontier.enqueue(service.clone());
            }
        }
        frontier.release_held_if_settled();
        frontier
    }

    /// A service whose dependencies are all met: start it, or hold it if it
    /// is an after-all service.
    fn enqueue(&mut self, service: String) {
        if self.after_all.contains(&service) {
            if !self.held.contains(&service) {
                self.held.push(service);
            }
        } else {
            self.ready_to_start.push_back(service);
        }
    }

    /// Whether nothing but after-all services is waiting to start or to
    /// become ready.
    pub fn is_settled(&self) -> bool {
        !self
            .ready_to_start
            .iter()
            .any(|s| !self.after_all.contains(s))
            && !self.pending.iter().any(|s| !self.after_all.contains(s))
    }

    fn release_held_if_settled(&mut self) {
        if !self.held.is_empty() && self.is_settled() {
            let held = std::mem::take(&mut self.held);
            self.ready_to_start.extend(held);
        }
    }

    /// Decrement the unmet count of `dependent`, enqueueing it at zero.
    fn dependency_done(&mut self, dependent: &str) {
        if self.blocked.contains(dependent) {
            return;
        }
        if let Some(count) = self.unmet.get_mut(dependent) {
            *count = count.saturating_sub(1);
            if *count == 0
                && !self.completed.contains(dependent)
                && !self.pending.contains(dependent)
            {
                self.enqueue(dependent.to_string());
            }
        }
    }

    /// Take the next batch of services that are ready to start.
    /// Returns an empty vec when no more services can start right now.
    pub fn take_ready(&mut self) -> Vec<String> {
        let batch: Vec<String> = self.ready_to_start.drain(..).collect();
        for s in &batch {
            self.pending.insert(s.clone());
        }
        batch
    }

    /// Mark a service as READY (started and confirmed operational).
    /// Updates the frontier: any dependents whose unmet count drops to 0
    /// become ready to start.
    pub fn mark_ready(&mut self, service: &str, graph: &DependencyGraph) {
        self.pending.remove(service);
        self.ready.insert(service.to_string());
        self.completed.insert(service.to_string());

        // Decrement unmet count for all dependents
        for dependent in graph.dependents_of(service) {
            self.dependency_done(dependent);
        }
        self.release_held_if_settled();
    }

    /// Mark a service as FAILED.
    /// For `requires` dependencies: block all transitive dependents.
    /// For `after`-only dependencies: they can still start (ordering satisfied).
    pub fn mark_failed(&mut self, service: &str, graph: &DependencyGraph) {
        self.pending.remove(service);
        self.completed.insert(service.to_string());

        // Block all services that require this one (transitively)
        let mut to_block = VecDeque::new();
        let mut soft_done = Vec::new();
        for dependent in graph.dependents_of(service) {
            // Only block if this is a hard (requires) dependency
            if graph.requires_of(dependent).contains(service) {
                to_block.push_back(dependent.clone());
            } else {
                soft_done.push(dependent.clone());
            }
        }

        // BFS to transitively block all services that require the failed one
        while let Some(svc) = to_block.pop_front() {
            if self.blocked.contains(&svc) {
                continue;
            }
            self.blocked.insert(svc.clone());
            self.completed.insert(svc.clone());
            self.ready_to_start.retain(|s| s != &svc);
            self.held.retain(|s| s != &svc);

            // Block anything that requires this newly-blocked service; for
            // `wants`/`after` dependents, the blocked service is done.
            for dep in graph.dependents_of(&svc) {
                if graph.requires_of(dep).contains(&svc) {
                    if !self.blocked.contains(dep) {
                        to_block.push_back(dep.clone());
                    }
                } else {
                    soft_done.push(dep.clone());
                }
            }
        }

        // Soft dependents: the failed or blocked dependency counts as done.
        for dependent in soft_done {
            self.dependency_done(&dependent);
        }
        self.release_held_if_settled();
    }

    /// Check if all services have been processed.
    pub fn is_complete(&self) -> bool {
        self.completed.len() + self.blocked.len() >= self.unmet.len()
    }

    /// Check if there are services waiting to start.
    pub fn has_ready(&self) -> bool {
        !self.ready_to_start.is_empty()
    }

    /// Check if there are services currently pending (started but not ready).
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Get all blocked services.
    pub fn blocked_services(&self) -> &HashSet<String> {
        &self.blocked
    }

    /// Get all ready services.
    pub fn ready_services(&self) -> &HashSet<String> {
        &self.ready
    }

    /// Get the number of services still pending startup.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::service::ServiceDef;

    fn make_service(name: &str, requires: &[&str], after: &[&str]) -> ServiceDef {
        let requires_str: Vec<String> = requires.iter().map(|s| s.to_string()).collect();
        let after_str: Vec<String> = after.iter().map(|s| s.to_string()).collect();
        let toml_str = format!(
            r#"
[service]
name = "{name}"
exec = ["/bin/true"]

[dependencies]
requires = {requires}
after = {after}
"#,
            requires = serde_json::to_string(&requires_str).unwrap(),
            after = serde_json::to_string(&after_str).unwrap(),
        );
        toml::from_str(&toml_str).unwrap()
    }

    #[test]
    fn test_independent_services_all_ready() {
        let defs = vec![
            make_service("a", &[], &[]),
            make_service("b", &[], &[]),
            make_service("c", &[], &[]),
        ];
        let graph = DependencyGraph::build(&defs);
        let mut frontier = StartupFrontier::new(&graph);

        let batch = frontier.take_ready();
        assert_eq!(batch.len(), 3);
    }

    #[test]
    fn test_linear_chain() {
        let defs = vec![
            make_service("a", &[], &[]),
            make_service("b", &["a"], &[]),
            make_service("c", &["b"], &[]),
        ];
        let graph = DependencyGraph::build(&defs);
        let mut frontier = StartupFrontier::new(&graph);

        // Only "a" should be ready initially
        let batch1 = frontier.take_ready();
        assert_eq!(batch1, vec!["a"]);

        // Mark "a" ready -> "b" becomes available
        frontier.mark_ready("a", &graph);
        let batch2 = frontier.take_ready();
        assert_eq!(batch2, vec!["b"]);

        // Mark "b" ready -> "c" becomes available
        frontier.mark_ready("b", &graph);
        let batch3 = frontier.take_ready();
        assert_eq!(batch3, vec!["c"]);

        frontier.mark_ready("c", &graph);
        assert!(frontier.is_complete());
    }

    #[test]
    fn test_diamond_parallelism() {
        // A -> B, A -> C, B+C -> D
        let defs = vec![
            make_service("a", &[], &[]),
            make_service("b", &["a"], &[]),
            make_service("c", &["a"], &[]),
            make_service("d", &["b", "c"], &[]),
        ];
        let graph = DependencyGraph::build(&defs);
        let mut frontier = StartupFrontier::new(&graph);

        // Only "a" ready
        let batch1 = frontier.take_ready();
        assert_eq!(batch1, vec!["a"]);

        // "a" ready -> both "b" and "c" become available (parallel!)
        frontier.mark_ready("a", &graph);
        let mut batch2 = frontier.take_ready();
        batch2.sort();
        assert_eq!(batch2, vec!["b", "c"]);

        // "b" ready, but "d" still waits for "c"
        frontier.mark_ready("b", &graph);
        assert!(frontier.take_ready().is_empty());

        // "c" ready -> "d" becomes available
        frontier.mark_ready("c", &graph);
        let batch3 = frontier.take_ready();
        assert_eq!(batch3, vec!["d"]);

        frontier.mark_ready("d", &graph);
        assert!(frontier.is_complete());
    }

    #[test]
    fn test_failure_blocks_requires_deps() {
        let defs = vec![
            make_service("a", &[], &[]),
            make_service("b", &["a"], &[]),
            make_service("c", &["b"], &[]),
        ];
        let graph = DependencyGraph::build(&defs);
        let mut frontier = StartupFrontier::new(&graph);

        frontier.take_ready(); // "a"
        frontier.mark_failed("a", &graph);

        // Both "b" and "c" should be blocked (transitive)
        assert!(frontier.blocked_services().contains("b"));
        assert!(frontier.blocked_services().contains("c"));
        assert!(frontier.take_ready().is_empty());
    }

    #[test]
    fn test_failure_of_after_dep_does_not_block() {
        let defs = vec![
            make_service("a", &[], &[]),
            make_service("b", &[], &["a"]), // after, not requires
        ];
        let graph = DependencyGraph::build(&defs);
        let mut frontier = StartupFrontier::new(&graph);

        let batch1 = frontier.take_ready();
        assert_eq!(batch1, vec!["a"]);

        // "a" fails, but "b" only has an `after` dep, not `requires`
        frontier.mark_failed("a", &graph);
        let batch2 = frontier.take_ready();
        assert_eq!(batch2, vec!["b"]);
    }

    #[test]
    fn test_mixed_deps_partial_block() {
        // "d" requires "b" and is after "c"
        // If "b" fails, "d" is blocked
        // If "c" fails, "d" can still start (once "b" is ready)
        let defs = vec![
            make_service("a", &[], &[]),
            make_service("b", &["a"], &[]),
            make_service("c", &["a"], &[]),
            make_service("d", &["b"], &["c"]),
        ];
        let graph = DependencyGraph::build(&defs);
        let mut frontier = StartupFrontier::new(&graph);

        frontier.take_ready(); // "a"
        frontier.mark_ready("a", &graph);
        frontier.take_ready(); // "b", "c"

        // "c" fails (after-only dep for "d") — "d" unmet decrements but still waits for "b"
        frontier.mark_failed("c", &graph);
        assert!(!frontier.blocked_services().contains("d"));

        // "b" succeeds — "d" should now be ready
        frontier.mark_ready("b", &graph);
        let batch = frontier.take_ready();
        assert_eq!(batch, vec!["d"]);
    }

    fn make_after_all(name: &str, requires: &[&str]) -> ServiceDef {
        let mut def = make_service(name, requires, &[]);
        def.dependencies.after_all = true;
        def
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    fn frontier_for(defs: &[ServiceDef]) -> (DependencyGraph, StartupFrontier) {
        let graph = DependencyGraph::build(defs);
        let after_all = defs
            .iter()
            .filter(|d| d.dependencies.after_all)
            .map(|d| d.service.name.clone())
            .collect();
        let frontier = StartupFrontier::with_after_all(&graph, after_all);
        (graph, frontier)
    }

    #[test]
    fn test_after_all_waits_for_every_other_service() {
        let defs = vec![
            make_service("a", &[], &[]),
            make_service("b", &["a"], &[]),
            make_service("c", &[], &[]),
            make_after_all("check", &[]),
        ];
        let (graph, mut f) = frontier_for(&defs);

        // "check" has no dependencies but is held back.
        assert_eq!(sorted(f.take_ready()), vec!["a", "c"]);
        f.mark_ready("c", &graph);
        assert!(f.take_ready().is_empty());
        f.mark_ready("a", &graph);
        assert_eq!(f.take_ready(), vec!["b"], "b still runs before check");
        f.mark_ready("b", &graph);
        assert_eq!(f.take_ready(), vec!["check"]);
    }

    #[test]
    fn test_after_all_counts_failed_and_blocked_as_settled() {
        let defs = vec![
            make_service("a", &[], &[]),
            make_service("b", &["a"], &[]),
            make_service("c", &[], &[]),
            make_after_all("check", &[]),
        ];
        let (graph, mut f) = frontier_for(&defs);
        assert_eq!(sorted(f.take_ready()), vec!["a", "c"]);
        f.mark_failed("a", &graph); // blocks b
        assert!(f.blocked_services().contains("b"));
        assert!(f.take_ready().is_empty(), "c is still pending");
        f.mark_ready("c", &graph);
        assert_eq!(f.take_ready(), vec!["check"]);
    }

    #[test]
    fn test_after_all_ignores_services_that_wait_for_it() {
        // "report" runs after the check; it must not hold the check back.
        let defs = vec![
            make_service("a", &[], &[]),
            make_after_all("check", &[]),
            make_service("report", &[], &["check"]),
        ];
        let (graph, mut f) = frontier_for(&defs);
        assert_eq!(f.take_ready(), vec!["a"]);
        f.mark_ready("a", &graph);
        assert_eq!(f.take_ready(), vec!["check"]);
        f.mark_ready("check", &graph);
        assert_eq!(f.take_ready(), vec!["report"]);
    }

    #[test]
    fn test_after_all_alone_starts_immediately() {
        let defs = vec![make_after_all("check", &[])];
        let (_graph, mut f) = frontier_for(&defs);
        assert_eq!(f.take_ready(), vec!["check"]);
    }

    #[test]
    fn test_after_all_still_honours_its_own_requires() {
        let defs = vec![make_service("a", &[], &[]), make_after_all("check", &["a"])];
        let (graph, mut f) = frontier_for(&defs);
        assert_eq!(f.take_ready(), vec!["a"]);
        f.mark_failed("a", &graph);
        assert!(f.blocked_services().contains("check"));
        assert!(f.take_ready().is_empty());
        assert!(!f.has_ready());
    }

    #[test]
    fn test_soft_dependent_of_blocked_service_still_starts() {
        // b requires a; c is only `after` b. a fails -> b blocked -> c starts.
        let defs = vec![
            make_service("a", &[], &[]),
            make_service("b", &["a"], &[]),
            make_service("c", &[], &["b"]),
        ];
        let graph = DependencyGraph::build(&defs);
        let mut f = StartupFrontier::new(&graph);
        assert_eq!(f.take_ready(), vec!["a"]);
        f.mark_failed("a", &graph);
        assert!(f.blocked_services().contains("b"));
        assert_eq!(f.take_ready(), vec!["c"]);
    }
}
