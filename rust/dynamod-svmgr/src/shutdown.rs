/// Graceful shutdown sequence for dynamod-svmgr.
///
/// Stops services in reverse dependency order, respects per-service
/// stop-signal and stop-timeout, escalates to SIGKILL after timeout.
/// Services with `[shutdown] last = true` stop after all others.
use std::collections::HashSet;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::cgroup;
use crate::config::service::parse_duration_secs;
use crate::dependency::graph::DependencyGraph;
use crate::supervisor::lifecycle::ServiceState;
use crate::supervisor::tree::{SupervisorTree, TreeNode};

/// Execute a full graceful shutdown of all services.
///
/// 1. Compute reverse dependency order (dependents first, dependencies last)
/// 2. For each service: send stop signal, wait up to stop-timeout, then SIGKILL
/// 3. Clean up cgroups
/// 4. Return when all services have been stopped
pub fn execute_shutdown(
    tree: &mut SupervisorTree,
    dep_graph: &DependencyGraph,
    cgroup_hierarchy: &Option<cgroup::hierarchy::CgroupHierarchy>,
    cgroup_monitor: &mut cgroup::monitor::CgroupMonitor,
) {
    tracing::info!("beginning graceful shutdown sequence");

    // Reverse topological order (Kahn's algorithm), `last` services at the end
    let stop_order = shutdown_order(dep_graph, |name| {
        tree.get_worker(name).is_some_and(|w| w.def.shutdown.last)
    });

    tracing::info!(
        "stopping {} service(s) in reverse dependency order",
        stop_order.len()
    );

    for name in &stop_order {
        stop_service_graceful(tree, name, cgroup_hierarchy, cgroup_monitor);
    }

    // Final reap of any remaining zombies
    reap_remaining();

    tracing::info!("all services stopped");
}

/// The order in which to stop services: dependents before their
/// dependencies, except that every service for which `is_last` holds comes
/// after all the others (in reverse dependency order among themselves).
pub fn shutdown_order(graph: &DependencyGraph, is_last: impl Fn(&str) -> bool) -> Vec<String> {
    let (normal, last): (Vec<String>, Vec<String>) = compute_reverse_topo_order(graph)
        .into_iter()
        .partition(|s| !is_last(s));
    normal.into_iter().chain(last).collect()
}

/// Stop a remain-after-exit oneshot that is active ("exited"): run its
/// stop-exec, if any, bounded by stop-timeout, and mark it stopped.
/// Returns false if the service is not in that state.
pub fn stop_exited_service(tree: &mut SupervisorTree, name: &str) -> bool {
    let (stop_exec, timeout) = match tree.get_worker(name) {
        Some(w) if w.state == ServiceState::Exited => (
            w.def.shutdown.stop_exec.clone(),
            parse_duration_secs(&w.def.shutdown.stop_timeout)
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(10)),
        ),
        _ => return false,
    };
    if let Some(TreeNode::Worker(w)) = tree.get_mut(name) {
        w.state = ServiceState::Stopping {
            deadline: Some(Instant::now() + timeout),
        };
    }
    match stop_exec {
        Some(cmd) if !cmd.is_empty() => {
            tracing::info!("stopping '{name}' (exited): running stop-exec {:?}", cmd);
            match run_stop_exec(name, &cmd, timeout) {
                Ok(status) if status.success() => {
                    tracing::info!("stop-exec for '{name}' succeeded")
                }
                Ok(status) => tracing::warn!("stop-exec for '{name}' exited with {status}"),
                Err(e) => tracing::warn!("stop-exec for '{name}' failed: {e}"),
            }
        }
        _ => tracing::info!("stopping '{name}' (exited, no stop-exec)"),
    }
    if let Some(TreeNode::Worker(w)) = tree.get_mut(name) {
        w.state = ServiceState::Stopped;
    }
    true
}

/// Run a stop-exec command, waiting at most `timeout` (then SIGKILL).
/// Its output goes to dynamod-logd like the service's own, or to svmgr's
/// stderr when logd is not running (e.g. late in shutdown).
pub fn run_stop_exec(
    name: &str,
    cmd: &[String],
    timeout: Duration,
) -> std::io::Result<std::process::ExitStatus> {
    use std::process::{Command, Stdio};

    // The relay thread ends when the last writer closes the pipe. It is not
    // joined: a stop-exec that leaves a background process holding its
    // output must not hang the shutdown.
    let out = log_pipe(name);
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]).stdin(Stdio::null());
    match out {
        Some(fd) => {
            let fd2 = fd.try_clone()?;
            command.stdout(Stdio::from(fd)).stderr(Stdio::from(fd2));
        }
        None => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    let mut child = command.spawn()?;
    drop(command); // closes our copies of the pipe's write end
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "stop-exec for '{name}' did not finish within {}s, killing it",
                timeout.as_secs()
            );
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(status)
}

/// A pipe whose read end is relayed line by line to dynamod-logd as
/// "<name>\t<line>", falling back to stderr.
fn log_pipe(name: &str) -> Option<std::os::fd::OwnedFd> {
    use std::io::BufRead;
    use std::os::fd::FromRawFd;

    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let write_end = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
    let name = name.to_string();
    std::thread::spawn(move || {
        let sock = std::os::unix::net::UnixDatagram::unbound().ok();
        for line in std::io::BufReader::new(read_end).lines() {
            let Ok(line) = line else { break };
            let sent = sock.as_ref().is_some_and(|s| {
                s.send_to(
                    format!("{name}\t{line}").as_bytes(),
                    "/run/dynamod/log.sock",
                )
                .is_ok()
            });
            if !sent {
                eprintln!("{name}: {line}");
            }
        }
    });
    Some(write_end)
}

/// Stop a single service gracefully:
/// 1. Send configured stop-signal (or SIGTERM)
/// 2. Wait up to stop-timeout
/// 3. If still alive, send SIGKILL and wait briefly
fn stop_service_graceful(
    tree: &mut SupervisorTree,
    name: &str,
    cgroup_hierarchy: &Option<cgroup::hierarchy::CgroupHierarchy>,
    cgroup_monitor: &mut cgroup::monitor::CgroupMonitor,
) {
    if stop_exited_service(tree, name) {
        return;
    }
    let (pid, stop_signal, stop_timeout, stop_exec) = match tree.get_worker(name) {
        Some(w) if w.pid.is_some() => {
            let sig = parse_signal(&w.def.shutdown.stop_signal);
            let timeout = parse_duration_secs(&w.def.shutdown.stop_timeout)
                .map(Duration::from_secs)
                .unwrap_or(Duration::from_secs(10));
            (
                w.pid.unwrap(),
                sig,
                timeout,
                w.def.shutdown.stop_exec.clone(),
            )
        }
        _ => return, // Not running or not found
    };

    tracing::info!(
        "stopping '{name}' (pid {pid}, signal={stop_signal}, timeout={}s)",
        stop_timeout.as_secs()
    );

    // Run stop-exec command if configured (e.g. "nginx -s quit", "pg_ctl stop"),
    // for at most stop-timeout
    if let Some(ref cmd) = stop_exec {
        if !cmd.is_empty() {
            tracing::info!("running stop-exec for '{name}': {:?}", cmd);
            match run_stop_exec(name, cmd, stop_timeout) {
                Ok(status) if status.success() => {
                    tracing::info!("stop-exec for '{name}' succeeded");
                }
                Ok(status) => {
                    tracing::warn!(
                        "stop-exec for '{name}' exited with {}, falling back to signal",
                        status
                    );
                }
                Err(e) => {
                    tracing::warn!("stop-exec for '{name}' failed: {e}, falling back to signal");
                }
            }
        }
    }

    // Send the stop signal
    let nix_pid = Pid::from_raw(pid);
    if signal::kill(nix_pid, stop_signal).is_err() {
        tracing::debug!("kill({pid}, {stop_signal}) failed — process may already be dead");
    }

    if let Some(TreeNode::Worker(w)) = tree.get_mut(name) {
        w.state = ServiceState::Stopping {
            deadline: Some(Instant::now() + stop_timeout),
        };
    }

    // Wait for the process to exit (polling with short sleeps)
    let deadline = Instant::now() + stop_timeout;
    loop {
        match waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => {
                tracing::info!("'{name}' exited (code {code})");
                finalize_stop(tree, name, pid, cgroup_hierarchy, cgroup_monitor);
                return;
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                tracing::info!("'{name}' killed by signal {sig}");
                finalize_stop(tree, name, pid, cgroup_hierarchy, cgroup_monitor);
                return;
            }
            Ok(WaitStatus::StillAlive) => {}
            Err(nix::errno::Errno::ECHILD) => {
                // Already reaped
                finalize_stop(tree, name, pid, cgroup_hierarchy, cgroup_monitor);
                return;
            }
            _ => {}
        }

        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Timeout: escalate to SIGKILL
    tracing::warn!("'{name}' did not stop within timeout, sending SIGKILL");
    let _ = signal::kill(nix_pid, Signal::SIGKILL);

    // Wait briefly for SIGKILL to take effect
    let kill_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {}
            _ => break,
        }
        if Instant::now() >= kill_deadline {
            tracing::error!("'{name}' (pid {pid}) did not die after SIGKILL");
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    finalize_stop(tree, name, pid, cgroup_hierarchy, cgroup_monitor);
}

/// Clean up after a stopped service.
fn finalize_stop(
    tree: &mut SupervisorTree,
    name: &str,
    pid: i32,
    cgroup_hierarchy: &Option<cgroup::hierarchy::CgroupHierarchy>,
    cgroup_monitor: &mut cgroup::monitor::CgroupMonitor,
) {
    tree.unregister_pid(pid);
    if let Some(TreeNode::Worker(w)) = tree.get_mut(name) {
        w.pid = None;
        w.state = ServiceState::Stopped;
    }

    // Clean up cgroup
    cgroup_monitor.unwatch(name);
    if let Some(hierarchy) = cgroup_hierarchy {
        let _ = hierarchy.remove_service_cgroup(name);
    }
}

/// Compute reverse topological order (dependents before dependencies).
/// Services with no dependents are stopped first; base services last.
/// Uses forward Kahn's algorithm then reverses the result.
fn compute_reverse_topo_order(graph: &DependencyGraph) -> Vec<String> {
    let all_services = graph.all_services();

    let mut forward_in_degree: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for svc in all_services {
        forward_in_degree.insert(svc.as_str(), graph.dependencies_of(svc).len());
    }

    let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    for (svc, &deg) in &forward_in_degree {
        if deg == 0 {
            queue.push_back(svc);
        }
    }

    let mut forward_order: Vec<String> = Vec::new();
    let mut visited: HashSet<&str> = HashSet::new();

    while let Some(svc) = queue.pop_front() {
        if !visited.insert(svc) {
            continue;
        }
        forward_order.push(svc.to_string());

        for dependent in graph.dependents_of(svc) {
            if let Some(deg) = forward_in_degree.get_mut(dependent.as_str()) {
                *deg = deg.saturating_sub(1);
                if *deg == 0 && !visited.contains(dependent.as_str()) {
                    queue.push_back(dependent.as_str());
                }
            }
        }
    }

    // Add any services not reached (cycles or isolated)
    for svc in all_services {
        if !visited.contains(svc.as_str()) {
            forward_order.push(svc.clone());
        }
    }

    // Reverse for shutdown order
    forward_order.reverse();
    forward_order
}

/// Parse a signal name (e.g. "SIGTERM", "SIGINT") into a nix Signal.
fn parse_signal(name: &str) -> Signal {
    match name {
        "SIGTERM" | "sigterm" => Signal::SIGTERM,
        "SIGINT" | "sigint" => Signal::SIGINT,
        "SIGQUIT" | "sigquit" => Signal::SIGQUIT,
        "SIGHUP" | "sighup" => Signal::SIGHUP,
        "SIGUSR1" | "sigusr1" => Signal::SIGUSR1,
        "SIGUSR2" | "sigusr2" => Signal::SIGUSR2,
        _ => Signal::SIGTERM,
    }
}

/// Reap any remaining zombie processes.
fn reap_remaining() {
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(nix::errno::Errno::ECHILD) => break,
            _ => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::service::ServiceDef;

    fn make_service(name: &str, requires: &[&str]) -> ServiceDef {
        let requires_str: Vec<String> = requires.iter().map(|s| s.to_string()).collect();
        let toml_str = format!(
            r#"
[service]
name = "{name}"
exec = ["/bin/true"]

[dependencies]
requires = {requires}

[shutdown]
stop-signal = "SIGTERM"
stop-timeout = "5s"
"#,
            requires = serde_json::to_string(&requires_str).unwrap(),
        );
        toml::from_str(&toml_str).unwrap()
    }

    #[test]
    fn test_reverse_topo_order_linear() {
        let defs = vec![
            make_service("a", &[]),
            make_service("b", &["a"]),
            make_service("c", &["b"]),
        ];
        let graph = DependencyGraph::build(&defs);
        let order = compute_reverse_topo_order(&graph);

        // c depends on b depends on a -> stop c first, then b, then a
        let pos_a = order.iter().position(|s| s == "a").unwrap();
        let pos_b = order.iter().position(|s| s == "b").unwrap();
        let pos_c = order.iter().position(|s| s == "c").unwrap();
        assert!(pos_c < pos_b, "c should stop before b");
        assert!(pos_b < pos_a, "b should stop before a");
    }

    #[test]
    fn test_reverse_topo_order_diamond() {
        let defs = vec![
            make_service("a", &[]),
            make_service("b", &["a"]),
            make_service("c", &["a"]),
            make_service("d", &["b", "c"]),
        ];
        let graph = DependencyGraph::build(&defs);
        let order = compute_reverse_topo_order(&graph);

        let pos_a = order.iter().position(|s| s == "a").unwrap();
        let pos_b = order.iter().position(|s| s == "b").unwrap();
        let pos_c = order.iter().position(|s| s == "c").unwrap();
        let pos_d = order.iter().position(|s| s == "d").unwrap();

        // d should stop first, a should stop last
        assert!(pos_d < pos_b, "d before b");
        assert!(pos_d < pos_c, "d before c");
        assert!(pos_b < pos_a, "b before a");
        assert!(pos_c < pos_a, "c before a");
    }

    #[test]
    fn test_reverse_topo_independent() {
        let defs = vec![
            make_service("x", &[]),
            make_service("y", &[]),
            make_service("z", &[]),
        ];
        let graph = DependencyGraph::build(&defs);
        let order = compute_reverse_topo_order(&graph);
        // All independent, any order is valid, just check all present
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn test_last_services_stop_after_all_others() {
        // finalize requires base, app requires base. finalize is `last`:
        // it stops after app *and* after base, despite requiring base.
        let defs = vec![
            make_service("base", &[]),
            make_service("app", &["base"]),
            make_service("finalize", &["base"]),
            make_service("other", &[]),
        ];
        let graph = DependencyGraph::build(&defs);
        let order = shutdown_order(&graph, |n| n == "finalize");
        assert_eq!(order.len(), 4);
        assert_eq!(order.last().map(String::as_str), Some("finalize"));
        let pos = |n: &str| order.iter().position(|s| s == n).unwrap();
        assert!(
            pos("app") < pos("base"),
            "dependency order kept among the others"
        );

        // Several `last` services keep reverse dependency order among themselves.
        let defs = vec![
            make_service("a", &[]),
            make_service("l1", &[]),
            make_service("l2", &["l1"]),
        ];
        let graph = DependencyGraph::build(&defs);
        let order = shutdown_order(&graph, |n| n.starts_with('l'));
        assert_eq!(order, vec!["a", "l2", "l1"]);
    }

    fn temp_file(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("dynamod-shutdown-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn exited_tree(stop_exec: Option<&str>) -> SupervisorTree {
        use crate::config::supervisor::RestartStrategy;
        use crate::supervisor::intensity::RestartIntensity;
        let mut tree = SupervisorTree::new(
            "root",
            RestartStrategy::OneForOne,
            RestartIntensity::new(5, Duration::from_secs(60)),
        );
        let stop = stop_exec
            .map(|f| format!("stop-exec = [\"/bin/sh\", \"-c\", \"echo stopped > {f}\"]\n"))
            .unwrap_or_default();
        let t = format!(
            "[service]\nname = \"fin\"\nexec = [\"/bin/true\"]\ntype = \"oneshot\"\nremain-after-exit = true\n\n[shutdown]\n{stop}last = true\n"
        );
        tree.add_worker(toml::from_str(&t).unwrap(), "root")
            .unwrap();
        tree.get_worker_mut("fin").unwrap().state = ServiceState::Exited;
        tree
    }

    #[test]
    fn test_stopping_exited_service_runs_stop_exec() {
        let marker = temp_file("stopexec");
        let mut tree = exited_tree(Some(marker.to_str().unwrap()));
        let graph = DependencyGraph::build(&[tree.get_worker("fin").unwrap().def.clone()]);
        let mut monitor = cgroup::monitor::CgroupMonitor::new();
        execute_shutdown(&mut tree, &graph, &None, &mut monitor);
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "stopped");
        assert_eq!(tree.get_worker("fin").unwrap().state, ServiceState::Stopped);
        std::fs::remove_file(&marker).unwrap();
    }

    #[test]
    fn test_stopping_exited_service_without_stop_exec_marks_stopped() {
        let mut tree = exited_tree(None);
        assert!(stop_exited_service(&mut tree, "fin"));
        assert_eq!(tree.get_worker("fin").unwrap().state, ServiceState::Stopped);
        // Not exited any more: nothing to do.
        assert!(!stop_exited_service(&mut tree, "fin"));
    }

    #[test]
    fn test_stop_exec_is_bounded_by_timeout() {
        let start = Instant::now();
        let status = run_stop_exec(
            "slow",
            &["/bin/sh".into(), "-c".into(), "sleep 10".into()],
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(!status.success());
        assert!(start.elapsed() < Duration::from_secs(5));

        // A background process keeping the output pipe open does not hang it.
        let start = Instant::now();
        let status = run_stop_exec(
            "bg",
            &[
                "/bin/sh".into(),
                "-c".into(),
                "sleep 3 & echo started".into(),
            ],
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(status.success());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn test_parse_signal_names() {
        assert_eq!(parse_signal("SIGTERM"), Signal::SIGTERM);
        assert_eq!(parse_signal("SIGINT"), Signal::SIGINT);
        assert_eq!(parse_signal("SIGHUP"), Signal::SIGHUP);
        assert_eq!(parse_signal("unknown"), Signal::SIGTERM);
    }
}
