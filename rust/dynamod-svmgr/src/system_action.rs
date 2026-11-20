/// Machine-level actions requested by failing services (`failure-action`).
///
/// Failures are detected deep in the exit-handling code; the request is
/// queued here and performed by the main loops (`take`) within one
/// iteration (~100ms).
use std::sync::Mutex;

use dynamod_common::protocol::ShutdownKind;

use crate::config::service::FailureAction;

/// What to do with the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAction {
    /// Clean shutdown: stop services in order, then ask dynamod-init.
    Shutdown(ShutdownKind),
    /// sync and reboot(2) immediately, without stopping anything.
    ForceReboot,
}

/// The machine action for a service's failure-action, if any.
pub fn for_failure(action: FailureAction) -> Option<SystemAction> {
    match action {
        FailureAction::None => None,
        FailureAction::Reboot => Some(SystemAction::Shutdown(ShutdownKind::Reboot)),
        FailureAction::Poweroff => Some(SystemAction::Shutdown(ShutdownKind::Poweroff)),
        FailureAction::RebootForce => Some(SystemAction::ForceReboot),
    }
}

static PENDING: Mutex<Option<(SystemAction, String)>> = Mutex::new(None);

/// A service has failed for good: queue its failure-action, if it has one.
/// Returns the queued action.
pub fn service_failed(service: &str, reason: &str, action: FailureAction) -> Option<SystemAction> {
    let system_action = for_failure(action)?;
    tracing::error!(
        "service '{service}' failed ({reason}); failure-action = \"{}\"",
        action.as_str()
    );
    request(
        system_action,
        format!("service '{service}' failed ({reason})"),
    );
    Some(system_action)
}

/// Queue a machine action. The first request wins, except that an
/// immediate reboot overrides a pending clean shutdown.
pub fn request(action: SystemAction, reason: String) {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let replace = match &*pending {
        None => true,
        Some((SystemAction::ForceReboot, _)) => false,
        Some(_) => action == SystemAction::ForceReboot,
    };
    if replace {
        *pending = Some((action, reason));
    }
}

/// Take the queued machine action, if any.
pub fn take() -> Option<(SystemAction, String)> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_action_mapping() {
        assert_eq!(for_failure(FailureAction::None), None);
        assert_eq!(
            for_failure(FailureAction::Reboot),
            Some(SystemAction::Shutdown(ShutdownKind::Reboot))
        );
        assert_eq!(
            for_failure(FailureAction::Poweroff),
            Some(SystemAction::Shutdown(ShutdownKind::Poweroff))
        );
        assert_eq!(
            for_failure(FailureAction::RebootForce),
            Some(SystemAction::ForceReboot)
        );
    }

    // The only test that touches the global queue.
    #[test]
    fn queue_precedence() {
        let _ = take();
        assert_eq!(service_failed("quiet", "exit 1", FailureAction::None), None);
        assert!(take().is_none(), "failure-action = none queues nothing");

        service_failed("boot-check", "exit 1", FailureAction::Reboot);
        service_failed("other", "exit 1", FailureAction::Poweroff);
        let (action, reason) = take().unwrap();
        assert_eq!(
            action,
            SystemAction::Shutdown(ShutdownKind::Reboot),
            "first request wins"
        );
        assert!(reason.contains("boot-check"));
        assert!(take().is_none());

        service_failed("boot-check", "exit 1", FailureAction::Reboot);
        service_failed("watchdog", "exit 1", FailureAction::RebootForce);
        service_failed("other", "exit 1", FailureAction::Poweroff);
        assert_eq!(
            take().unwrap().0,
            SystemAction::ForceReboot,
            "reboot-force overrides"
        );
    }
}
