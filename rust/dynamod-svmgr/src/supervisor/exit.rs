use super::lifecycle::{ExitInfo, ServiceState};
use super::strategy::{self, StrategyAction};
use super::tree::SupervisorTree;
/// What a service's process exit means.
///
/// - The service was being stopped (dynamodctl stop, a strategy stop): it
///   is now stopped. No restart, not a failure.
/// - A oneshot exited 0: it completed. No restart; a `remain-after-exit`
///   oneshot stays active ("exited").
/// - Otherwise the supervisor's restart strategy decides. If the service is
///   not restarted and ended in failure, or the supervisor gave up on it,
///   the service has failed for good and its `failure-action` applies.
use crate::config::service::ServiceType;

/// The outcome of a service's exit.
#[derive(Debug, Default)]
pub struct ExitDecision {
    /// Stop/start actions to execute, in order.
    pub actions: Vec<StrategyAction>,
    /// The service failed and will not be restarted.
    pub failed: bool,
    /// The service's supervisor exceeded its restart intensity.
    pub supervisor_failed: Option<String>,
    /// The supervisor the failure was escalated to (None at the root).
    pub escalated_to: Option<String>,
}

/// Record the exit of `worker_id` in the tree and decide what follows.
pub fn decide_exit(tree: &mut SupervisorTree, worker_id: &str, exit: &ExitInfo) -> ExitDecision {
    let Some(w) = tree.get_worker_mut(worker_id) else {
        return ExitDecision::default();
    };
    let was_stopping = matches!(w.state, ServiceState::Stopping { .. });
    let oneshot = w.def.service.service_type == ServiceType::Oneshot;
    let completed = oneshot && exit.is_normal();

    w.pid = None;
    w.state = if was_stopping {
        ServiceState::Stopped
    } else if completed && w.def.service.remain_after_exit {
        ServiceState::Exited
    } else if exit.is_normal() {
        ServiceState::Stopped
    } else {
        ServiceState::Failed {
            exit_code: exit.exit_code,
            signal: exit.signal,
        }
    };

    if was_stopping || completed {
        return ExitDecision::default();
    }

    let Some(supervisor_id) = tree.parent_of(worker_id).map(str::to_string) else {
        return ExitDecision {
            failed: !exit.is_normal(),
            ..Default::default()
        };
    };

    let action = strategy::apply_strategy(tree, &supervisor_id, worker_id, exit);
    if action.supervisor_failed {
        // The supervisor gave up: its children are abandoned. As before,
        // only the parent's reaction is executed.
        let mut decision = ExitDecision {
            failed: true,
            supervisor_failed: Some(supervisor_id.clone()),
            ..Default::default()
        };
        if let Some((parent_id, parent_action)) = strategy::escalate_failure(tree, &supervisor_id) {
            decision.escalated_to = Some(parent_id);
            decision.actions.push(parent_action);
        }
        return decision;
    }

    let restarting = action.to_start.iter().any(|c| c == worker_id);
    let failed = !restarting
        && tree.get_worker(worker_id).is_some_and(|w| {
            matches!(
                w.state,
                ServiceState::Failed { .. } | ServiceState::Abandoned
            )
        });
    ExitDecision {
        actions: vec![action],
        failed,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::service::ServiceDef;
    use crate::config::supervisor::RestartStrategy;
    use crate::supervisor::intensity::RestartIntensity;
    use std::time::Duration;

    fn tree_with(def_toml: &str, max_restarts: u32) -> SupervisorTree {
        let intensity = RestartIntensity::new(max_restarts, Duration::from_secs(60));
        let mut tree = SupervisorTree::new("root", RestartStrategy::OneForOne, intensity);
        let def: ServiceDef = toml::from_str(def_toml).unwrap();
        tree.add_worker(def, "root").unwrap();
        tree.register_pid("svc", 100);
        tree.unregister_pid(100);
        tree
    }

    const CODE_0: ExitInfo = ExitInfo {
        exit_code: Some(0),
        signal: None,
    };
    const CODE_1: ExitInfo = ExitInfo {
        exit_code: Some(1),
        signal: None,
    };
    const SIGTERM: ExitInfo = ExitInfo {
        exit_code: None,
        signal: Some(15),
    };

    fn state(tree: &SupervisorTree) -> ServiceState {
        tree.get_worker("svc").unwrap().state.clone()
    }

    #[test]
    fn explicit_stop_is_not_restarted_or_failed() {
        let mut tree = tree_with("[service]\nname = \"svc\"\nexec = [\"/bin/x\"]\n", 5);
        tree.get_worker_mut("svc").unwrap().state = ServiceState::Stopping { deadline: None };
        let d = decide_exit(&mut tree, "svc", &SIGTERM);
        assert!(
            d.actions.is_empty(),
            "a permanent service stopped on request stays stopped"
        );
        assert!(!d.failed);
        assert_eq!(state(&tree), ServiceState::Stopped);
    }

    #[test]
    fn remain_after_exit_oneshot_stays_exited() {
        let mut tree = tree_with(
            "[service]\nname = \"svc\"\nexec = [\"/bin/true\"]\ntype = \"oneshot\"\nremain-after-exit = true\n",
            5,
        );
        let d = decide_exit(&mut tree, "svc", &CODE_0);
        assert!(d.actions.is_empty());
        assert!(!d.failed);
        assert_eq!(state(&tree), ServiceState::Exited);
        assert!(state(&tree).is_active());
        assert_eq!(state(&tree).display_name(), "exited");
    }

    #[test]
    fn completed_oneshot_is_not_rerun() {
        // Default restart policy is permanent; a successful oneshot still ends.
        let mut tree = tree_with(
            "[service]\nname = \"svc\"\nexec = [\"/bin/true\"]\ntype = \"oneshot\"\n",
            5,
        );
        let d = decide_exit(&mut tree, "svc", &CODE_0);
        assert!(d.actions.is_empty());
        assert!(!d.failed);
        assert_eq!(state(&tree), ServiceState::Stopped);
    }

    #[test]
    fn failed_oneshot_without_restart_has_failed() {
        let mut tree = tree_with(
            "[service]\nname = \"svc\"\nexec = [\"/bin/false\"]\ntype = \"oneshot\"\nremain-after-exit = true\n\n[restart]\npolicy = \"temporary\"\n",
            5,
        );
        let d = decide_exit(&mut tree, "svc", &CODE_1);
        assert!(d.failed);
        assert!(matches!(
            state(&tree),
            ServiceState::Failed {
                exit_code: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn restarted_service_has_not_failed_yet() {
        let mut tree = tree_with("[service]\nname = \"svc\"\nexec = [\"/bin/x\"]\n", 5);
        let d = decide_exit(&mut tree, "svc", &CODE_1);
        assert!(!d.failed);
        assert_eq!(d.actions.len(), 1);
        assert_eq!(d.actions[0].to_start, vec!["svc".to_string()]);
    }

    #[test]
    fn supervisor_giving_up_is_a_failure() {
        let mut tree = tree_with("[service]\nname = \"svc\"\nexec = [\"/bin/x\"]\n", 0);
        let d = decide_exit(&mut tree, "svc", &CODE_1);
        assert!(d.failed);
        assert_eq!(d.supervisor_failed.as_deref(), Some("root"));
        assert_eq!(state(&tree), ServiceState::Abandoned);
    }

    #[test]
    fn normal_exit_of_transient_service_is_not_a_failure() {
        let mut tree = tree_with(
            "[service]\nname = \"svc\"\nexec = [\"/bin/x\"]\n\n[restart]\npolicy = \"transient\"\n",
            5,
        );
        let d = decide_exit(&mut tree, "svc", &CODE_0);
        assert!(!d.failed);
        assert_eq!(state(&tree), ServiceState::Stopped);
    }
}
