/// dynamod-svmgr: Service manager for the dynamod init system.
mod cgroup;
mod config;
mod dependency;
mod ipc;
mod namespace;
mod process;
mod shutdown;
mod supervisor;
mod system_action;

use std::collections::HashSet;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use config::service::{self, ServiceDef, ServiceType, parse_duration_secs};
use config::supervisor as sup_config;
use config::validate;
use dependency::graph::DependencyGraph;
use ipc::init_channel::InitChannel;
use process::readiness::{ReadinessResult, ReadinessTracker};
use process::spawn;
use supervisor::intensity::RestartIntensity;
use supervisor::lifecycle::{ExitInfo, ServiceState};
use supervisor::strategy;
use supervisor::tree::{SupervisorTree, TreeNode};
use system_action::SystemAction;

use dynamod_common::config_dirs;
use dynamod_common::paths;
use dynamod_common::protocol::{MessageBody, ShutdownKind};

type CgroupHierarchy = Option<cgroup::hierarchy::CgroupHierarchy>;
type CgroupMonitor = cgroup::monitor::CgroupMonitor;

/// State shared by the main loops and the control actions, besides the tree.
struct Runtime {
    /// Alternative root for configuration and markers ($DYNAMOD_ROOT).
    root: Option<PathBuf>,
    /// Dependency graph of every loaded service (used for shutdown order).
    dep_graph: DependencyGraph,
    /// Shutdown scheduled by `shutdown +M` / `shutdown HH:MM`: kind and
    /// wall-clock time (seconds since the epoch).
    scheduled_shutdown: Option<(ShutdownKind, u64)>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("dynamod-svmgr starting");

    // Connect to init via IPC
    let mut init_channel = InitChannel::from_env();
    if init_channel.is_some() {
        tracing::info!("connected to dynamod-init via IPC");
    } else {
        tracing::warn!("no init connection, running standalone");
    }

    if let Some(ref mut ch) = init_channel {
        let _ = ch.send_heartbeat();
    }

    let root = paths::root_from_env();
    if let Some(ref r) = root {
        tracing::warn!(
            "using alternative root {} (${})",
            r.display(),
            paths::ROOT_ENV
        );
    }

    // Load configuration: /usr/lib/dynamod/... then /etc/dynamod/...
    let services = service::load_services(&paths::service_dirs(root.as_deref()));
    tracing::info!("loaded {} service definition(s)", services.len());

    let supervisor_defs = sup_config::load_supervisors(&paths::supervisor_dirs(root.as_deref()));
    tracing::info!("loaded {} supervisor definition(s)", supervisor_defs.len());

    let errors = validate::validate_all(&services, &supervisor_defs);
    for err in &errors {
        tracing::error!("config validation: {err}");
    }

    // Initialize cgroup hierarchy
    let cgroup_hierarchy = if cgroup::hierarchy::CgroupHierarchy::is_available() {
        match cgroup::hierarchy::CgroupHierarchy::init() {
            Ok(h) => {
                tracing::info!("cgroup v2 hierarchy initialized at {}", h.root().display());
                Some(h)
            }
            Err(e) => {
                tracing::warn!("failed to initialize cgroup hierarchy: {e}");
                None
            }
        }
    } else {
        tracing::info!("cgroup v2 not available, skipping resource isolation");
        None
    };

    let mut cgroup_monitor = cgroup::monitor::CgroupMonitor::new();

    // Start control socket server
    let control_server = match ipc::control::ControlServer::bind(
        &paths::rooted(root.as_deref(), paths::CONTROL_SOCK),
        root.clone(),
    ) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("failed to start control socket: {e}");
            None
        }
    };

    // Build supervisor tree
    let root_intensity = RestartIntensity::new(20, Duration::from_secs(600));
    let mut tree = SupervisorTree::new(
        "root",
        sup_config::RestartStrategy::OneForOne,
        root_intensity,
    );

    // Add configured supervisors
    for sup_def in &supervisor_defs {
        if sup_def.supervisor.name == "root" {
            // Update root's strategy from config
            if let Some(TreeNode::Supervisor(root)) = tree.get_mut("root") {
                root.strategy = sup_def.supervisor.strategy.clone();
                let window =
                    parse_duration_secs(&sup_def.restart.max_restart_window).unwrap_or(600);
                root.intensity = RestartIntensity::new(
                    sup_def.restart.max_restarts,
                    Duration::from_secs(window),
                );
            }
            continue;
        }
        add_supervisor(&mut tree, sup_def);
    }

    // Add workers (services) to the tree; keep the ones that made it
    let mut loaded: Vec<ServiceDef> = Vec::with_capacity(services.len());
    for def in services {
        if add_worker(&mut tree, &def) {
            loaded.push(def);
        }
    }
    let services = loaded;

    // Build dependency graph and validate no cycles
    let dep_graph = DependencyGraph::build(&services);
    if let Err(e) = dependency::cycle::validate_no_cycles(&dep_graph) {
        tracing::error!("FATAL: {e}");
        tracing::error!("refusing to boot with cyclic dependencies");
        std::process::exit(1);
    }

    // The services to start at boot: enabled ones and what they require/want
    let boot = dependency::boot::boot_set(&services, |d| {
        config_dirs::enablement(root.as_deref(), &d.service.name, d.service.enabled).enabled
    });
    let boot_defs: Vec<ServiceDef> = services
        .iter()
        .filter(|d| boot.contains(&d.service.name))
        .cloned()
        .collect();
    let boot_graph = DependencyGraph::build(&boot_defs);
    let after_all: HashSet<String> = boot_defs
        .iter()
        .filter(|d| d.dependencies.after_all)
        .map(|d| d.service.name.clone())
        .collect();
    tracing::info!(
        "{} of {} service(s) to start at boot ({} disabled)",
        boot.len(),
        services.len(),
        services.len() - boot.len()
    );

    let mut rt = Runtime {
        root,
        dep_graph,
        scheduled_shutdown: None,
    };

    // Start services using the dynamic frontier algorithm
    let mut frontier =
        dependency::frontier::StartupFrontier::with_after_all(&boot_graph, after_all);
    let mut pending_readiness = HashSet::new();
    // Oneshot services waiting for process exit before being marked ready
    let mut pending_oneshots: HashSet<String> = HashSet::new();

    tracing::info!("starting services via dependency frontier");

    // Process the frontier until all services are started
    loop {
        perform_pending_system_actions(
            &mut tree,
            &mut rt,
            &cgroup_hierarchy,
            &mut cgroup_monitor,
            &mut init_channel,
        );

        // Reap exited children during startup (handles oneshot completion)
        reap_oneshots_during_startup(
            &mut tree,
            &mut pending_oneshots,
            &mut frontier,
            &boot_graph,
            &cgroup_hierarchy,
            &mut cgroup_monitor,
        );
        settle_lost_oneshots(&tree, &mut pending_oneshots, &mut frontier, &boot_graph);

        // Start any services whose dependencies are satisfied
        let batch = frontier.take_ready();
        for name in &batch {
            // An explicit start may already be waiting for readiness.
            if !tree.get_worker(name).is_some_and(|w| w.pid.is_some() || w.state == ServiceState::Exited) {
                start_worker(&mut tree, name, &cgroup_hierarchy, &mut cgroup_monitor);
            }

            let Some(w) = tree.get_worker(name) else {
                frontier.mark_failed(name, &boot_graph);
                continue;
            };
            if w.state == ServiceState::Exited {
                frontier.mark_ready(name, &boot_graph);
                continue;
            }
            if w.pid.is_none() {
                // Spawning failed
                frontier.mark_failed(name, &boot_graph);
                service_failed(&tree, name, "could not be started");
                continue;
            }

            // Set up readiness tracking
            let is_oneshot = w.def.service.service_type == ServiceType::Oneshot;
            if w.readiness.is_some() {
                pending_readiness.insert(name.clone());
            } else if is_oneshot {
                // Oneshot with no readiness check: wait for process exit
                pending_oneshots.insert(name.clone());
                tracing::info!("service '{name}' is oneshot, waiting for exit");
            } else {
                // Long-running service with no readiness check: ready immediately
                frontier.mark_ready(name, &boot_graph);
                tracing::info!("service '{name}' ready (immediate)");
            }
        }

        poll_readiness(&mut tree, |name, ready| {
            if pending_readiness.remove(name) {
                if ready {
                    frontier.mark_ready(name, &boot_graph);
                } else {
                    frontier.mark_failed(name, &boot_graph);
                }
            }
        });
        // A restart can fail before it has a new tracker.
        pending_readiness.retain(|name| {
            if tree.get_worker(name).is_some_and(|w| w.readiness.is_some()) {
                return true;
            }
            frontier.mark_failed(name, &boot_graph);
            false
        });

        // If no more work to do, break out of startup
        if !frontier.has_ready() && pending_readiness.is_empty() && pending_oneshots.is_empty() {
            break;
        }

        // Accept dynamodctl connections during startup (listener is non-blocking).
        if let Some(ref server) = control_server {
            dispatch_control_actions(
                server.poll(&tree),
                &mut tree,
                &mut rt,
                &cgroup_hierarchy,
                &mut cgroup_monitor,
                &mut init_channel,
            );
        }

        // Brief sleep to avoid busy-polling readiness checks
        std::thread::sleep(Duration::from_millis(100));
    }

    let blocked = frontier.blocked_services();
    if !blocked.is_empty() {
        tracing::warn!(
            "{} service(s) blocked due to failed dependencies: {:?}",
            blocked.len(),
            blocked
        );
    }

    tracing::info!(
        "startup complete: {} ready, {} blocked, {} not started (disabled)",
        frontier.ready_services().len(),
        blocked.len(),
        services.len() - boot.len(),
    );

    // Main event loop
    let heartbeat_interval = Duration::from_secs(5);
    let mut last_heartbeat = Instant::now();
    let mut heartbeat_failures: u32 = 0;
    const MAX_HEARTBEAT_FAILURES: u32 = 3;

    loop {
        perform_pending_system_actions(
            &mut tree,
            &mut rt,
            &cgroup_hierarchy,
            &mut cgroup_monitor,
            &mut init_channel,
        );

        // Reap exited children and apply supervisor strategies
        reap_and_handle_exits(&mut tree, &cgroup_hierarchy, &mut cgroup_monitor);

        poll_readiness(&mut tree, |_, _| {});

        // Handle control socket requests
        if let Some(ref server) = control_server {
            dispatch_control_actions(
                server.poll(&tree),
                &mut tree,
                &mut rt,
                &cgroup_hierarchy,
                &mut cgroup_monitor,
                &mut init_channel,
            );
        }

        // Check for messages from init
        if let Some(ref mut ch) = init_channel {
            while let Some(msg) = ch.try_recv() {
                match msg.body {
                    MessageBody::ShutdownSignal { ref signal } => {
                        tracing::info!("shutdown signal from init: {signal}");
                        shutdown::execute_shutdown(
                            &mut tree,
                            &rt.dep_graph,
                            &cgroup_hierarchy,
                            &mut cgroup_monitor,
                        );
                        std::process::exit(0);
                    }
                    MessageBody::HeartbeatAck => {}
                    _ => {
                        tracing::debug!("message from init: {:?}", msg.body);
                    }
                }
            }
        }

        // Poll cgroup events (OOM, memory pressure)
        for event in cgroup_monitor.poll() {
            match event {
                cgroup::monitor::CgroupEvent::OomKill {
                    ref service_name,
                    count,
                } => {
                    tracing::error!("OOM kill in service '{service_name}' ({count} kill(s))");
                }
                cgroup::monitor::CgroupEvent::MemoryHigh {
                    ref service_name,
                    current_bytes,
                } => {
                    tracing::warn!(
                        "memory pressure in '{service_name}': {} bytes",
                        current_bytes
                    );
                }
            }
        }

        // Periodic heartbeat
        if last_heartbeat.elapsed() >= heartbeat_interval {
            if let Some(ref mut ch) = init_channel {
                if let Err(e) = ch.send_heartbeat() {
                    heartbeat_failures += 1;
                    tracing::error!(
                        "heartbeat failed ({heartbeat_failures}/{MAX_HEARTBEAT_FAILURES}): {e}"
                    );
                    if heartbeat_failures >= MAX_HEARTBEAT_FAILURES {
                        tracing::error!("init connection lost, exiting");
                        break;
                    }
                } else {
                    heartbeat_failures = 0;
                }
            }
            last_heartbeat = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Add a supervisor from its definition (parent falls back to root).
fn add_supervisor(tree: &mut SupervisorTree, sup_def: &sup_config::SupervisorDef) -> bool {
    let parent = sup_def.supervisor.parent.as_deref().unwrap_or("root");
    let window = parse_duration_secs(&sup_def.restart.max_restart_window).unwrap_or(300);
    let intensity =
        RestartIntensity::new(sup_def.restart.max_restarts, Duration::from_secs(window));

    match tree.add_supervisor(
        &sup_def.supervisor.name,
        parent,
        sup_def.supervisor.strategy.clone(),
        intensity,
    ) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(
                "failed to add supervisor '{}': {e}",
                sup_def.supervisor.name
            );
            false
        }
    }
}

/// Add a service to the tree under its supervisor (or root if unknown).
fn add_worker(tree: &mut SupervisorTree, def: &ServiceDef) -> bool {
    let parent = &def.service.supervisor;
    let parent_id = if tree.get_supervisor(parent).is_some() {
        parent.as_str()
    } else {
        tracing::warn!(
            "service '{}' references unknown supervisor '{}', using root",
            def.service.name,
            parent
        );
        "root"
    };

    match tree.add_worker(def.clone(), parent_id) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!("failed to add service '{}' to tree: {e}", def.service.name);
            false
        }
    }
}

/// All service definitions currently in the tree.
fn tree_defs(tree: &SupervisorTree) -> Vec<ServiceDef> {
    tree.all_workers()
        .iter()
        .filter_map(|name| tree.get_worker(name))
        .map(|w| w.def.clone())
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A service has failed for good: queue its failure-action, if any.
fn service_failed(tree: &SupervisorTree, name: &str, reason: &str) {
    if let Some(w) = tree.get_worker(name) {
        system_action::service_failed(name, reason, w.def.service.failure_action);
    }
}

/// Perform a failure-action queued by a failing service, or a scheduled
/// shutdown that is due. Does not return if either happens.
fn perform_pending_system_actions(
    tree: &mut SupervisorTree,
    rt: &mut Runtime,
    cgroup_hierarchy: &CgroupHierarchy,
    cgroup_monitor: &mut CgroupMonitor,
    init_channel: &mut Option<InitChannel>,
) {
    if let Some((action, reason)) = system_action::take() {
        match action {
            SystemAction::ForceReboot => {
                tracing::error!("{reason}: rebooting immediately (reboot-force)");
                let err = dynamod_common::power::reboot_now(ShutdownKind::Reboot, true);
                tracing::error!("reboot(2) failed: {err}; falling back to a clean reboot");
                perform_shutdown(
                    ShutdownKind::Reboot,
                    tree,
                    rt,
                    cgroup_hierarchy,
                    cgroup_monitor,
                    init_channel,
                );
            }
            SystemAction::Shutdown(kind) => {
                tracing::error!("{reason}: starting {kind:?}");
                perform_shutdown(
                    kind,
                    tree,
                    rt,
                    cgroup_hierarchy,
                    cgroup_monitor,
                    init_channel,
                );
            }
        }
    }

    if let Some((kind, at)) = rt.scheduled_shutdown {
        if now_unix() >= at {
            tracing::info!("scheduled {kind:?} is due");
            rt.scheduled_shutdown = None;
            perform_shutdown(
                kind,
                tree,
                rt,
                cgroup_hierarchy,
                cgroup_monitor,
                init_channel,
            );
        }
    }
}

/// Clean shutdown: stop every service in order, then ask dynamod-init to
/// finish (kill what is left, unmount, reboot(2)).
fn perform_shutdown(
    kind: ShutdownKind,
    tree: &mut SupervisorTree,
    rt: &Runtime,
    cgroup_hierarchy: &CgroupHierarchy,
    cgroup_monitor: &mut CgroupMonitor,
    init_channel: &mut Option<InitChannel>,
) -> ! {
    shutdown::execute_shutdown(tree, &rt.dep_graph, cgroup_hierarchy, cgroup_monitor);
    if let Some(ch) = init_channel {
        match ch.request_shutdown(kind) {
            Ok(()) => {
                // dynamod-init now terminates every process, svmgr included.
                // Wait for that rather than exiting, so that init cannot see
                // svmgr exit (and restart it) before reading the request.
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(250));
                }
                tracing::error!("dynamod-init did not act on the shutdown request");
            }
            Err(e) => tracing::error!("failed to send shutdown request to init: {e}"),
        }
    }
    std::process::exit(0);
}

fn dispatch_control_actions(
    actions: Vec<ipc::control::ControlAction>,
    tree: &mut SupervisorTree,
    rt: &mut Runtime,
    cgroup_hierarchy: &CgroupHierarchy,
    cgroup_monitor: &mut CgroupMonitor,
    init_channel: &mut Option<InitChannel>,
) {
    use ipc::control::ControlAction;

    for action in actions {
        match action {
            ControlAction::StartService(name) => {
                let active = tree
                    .get_worker(&name)
                    .is_some_and(|w| w.pid.is_some() || w.state == ServiceState::Exited);
                if active {
                    tracing::info!("service '{name}' is already active");
                } else {
                    start_worker(tree, &name, cgroup_hierarchy, cgroup_monitor);
                    if tree.get_worker(&name).is_some_and(|w| w.pid.is_none()) {
                        service_failed(tree, &name, "could not be started");
                    }
                }
            }
            ControlAction::StopService(name) => {
                stop_worker(tree, &name);
            }
            ControlAction::RestartService(name) => {
                let old_pid = tree.get_worker(&name).and_then(|w| w.pid);
                stop_worker(tree, &name);

                if let Some(pid) = old_pid {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        reap_and_handle_exits(tree, cgroup_hierarchy, cgroup_monitor);
                        let still_running =
                            tree.get_worker(&name).is_some_and(|w| w.pid == Some(pid));
                        if !still_running || Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }

                start_worker(tree, &name, cgroup_hierarchy, cgroup_monitor);
            }
            ControlAction::Reload => {
                tracing::info!("reload requested via control socket");
                reload_services(tree, rt, cgroup_hierarchy, cgroup_monitor);
            }
            ControlAction::Shutdown(kind) => {
                tracing::info!("shutdown requested via control socket");
                rt.scheduled_shutdown = None;
                perform_shutdown(
                    kind,
                    tree,
                    rt,
                    cgroup_hierarchy,
                    cgroup_monitor,
                    init_channel,
                );
            }
            ControlAction::ScheduleShutdown(kind, at) => {
                tracing::info!("{kind:?} scheduled in {}s", at.saturating_sub(now_unix()));
                rt.scheduled_shutdown = Some((kind, at));
            }
            ControlAction::CancelShutdown => {
                if rt.scheduled_shutdown.take().is_some() {
                    tracing::info!("scheduled shutdown cancelled");
                }
            }
        }
    }
}

/// Poll trackers from boot, explicit starts and automatic restarts alike.
fn poll_readiness(tree: &mut SupervisorTree, mut report: impl FnMut(&str, bool)) {
    let mut failures = Vec::new();
    for worker in tree.workers_mut() {
        let Some(tracker) = &worker.readiness else { continue };
        let result = match worker.pid {
            Some(pid) => tracker.check(pid),
            None => ReadinessResult::Failed("service exited before readiness".into()),
        };
        match result {
            ReadinessResult::NotReady => continue,
            ReadinessResult::Ready => {
                tracing::info!("service '{}' ready", worker.name);
                report(&worker.name, true);
            }
            error => {
                tracing::error!("service '{}' readiness: {error:?}", worker.name);
                report(&worker.name, false);
                if worker.pid.is_some() {
                    failures.push(worker.name.clone());
                }
            }
        }
        worker.readiness = None;
    }
    for name in failures {
        service_failed(tree, &name, "readiness check failed");
    }
}

/// Start a worker process and register it in the tree.
fn start_worker(
    tree: &mut SupervisorTree,
    worker_id: &str,
    cgroup_hierarchy: &CgroupHierarchy,
    cgroup_monitor: &mut CgroupMonitor,
) {
    let Some(worker) = tree.get_worker_mut(worker_id) else { return };
    let def = worker.def.clone();
    // Release the previous instance's socket before binding the new one.
    worker.readiness = None;
    let mut tracker = match ReadinessTracker::new(&def.readiness, worker_id) {
        Ok(tracker) => tracker,
        Err(e) => {
            tracing::error!("failed to set up readiness for '{worker_id}': {e}");
            worker.state = ServiceState::Failed { exit_code: None, signal: None };
            return;
        }
    };

    // Log namespace info if configured
    if let Some(ref ns) = def.namespace {
        tracing::info!(
            "starting service '{}' (namespaces: {})",
            worker_id,
            namespace::setup::describe_namespaces(ns)
        );
    } else {
        tracing::info!("starting service '{}'", worker_id);
    }

    // Set up cgroup before spawning
    let cgroup_path = if let (Some(hierarchy), Some(cg_config)) = (cgroup_hierarchy, &def.cgroup) {
        match hierarchy.create_service_cgroup(worker_id) {
            Ok(path) => {
                if let Err(e) = cgroup::limits::apply_limits(&path, cg_config) {
                    tracing::warn!("failed to apply cgroup limits for '{worker_id}': {e}");
                }
                Some(path)
            }
            Err(e) => {
                tracing::warn!("failed to create cgroup for '{worker_id}': {e}");
                None
            }
        }
    } else {
        None
    };

    match spawn::spawn_service(&def, tracker.notify_socket_path()) {
        Ok(spawned) => {
            let pid = spawned.pid.as_raw();
            if let Some(fd) = spawned.ready_fd {
                tracker.set_ready_fd(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            tree.register_pid(worker_id, pid);
            if let Some(TreeNode::Worker(w)) = tree.get_mut(worker_id) {
                if !tracker.is_immediate() {
                    w.readiness = Some(tracker);
                }
            }

            // Move the process into its cgroup
            if let (Some(hierarchy), Some(_)) = (cgroup_hierarchy, &cgroup_path) {
                if let Err(e) = hierarchy.add_process(worker_id, pid as u32) {
                    tracing::warn!("failed to add pid {pid} to cgroup for '{worker_id}': {e}");
                }
                // Start monitoring the cgroup
                cgroup_monitor.watch(worker_id, &hierarchy.service_path(worker_id));
            }
        }
        Err(e) => {
            tracing::error!("failed to start '{}': {e}", worker_id);
            if let Some(TreeNode::Worker(w)) = tree.get_mut(worker_id) {
                w.state = ServiceState::Failed {
                    exit_code: None,
                    signal: None,
                };
            }
            // Clean up cgroup on failure
            if let Some(hierarchy) = cgroup_hierarchy {
                let _ = hierarchy.remove_service_cgroup(worker_id);
            }
        }
    }
}

/// Stop a worker: SIGTERM a running process (its exit is then handled as a
/// stop, not a failure), or run the stop-exec of a remain-after-exit
/// oneshot that is active.
fn stop_worker(tree: &mut SupervisorTree, worker_id: &str) {
    if shutdown::stop_exited_service(tree, worker_id) {
        return;
    }

    let pid = match tree.get_worker(worker_id) {
        Some(w) => w.pid,
        None => return,
    };

    if let Some(pid) = pid {
        tracing::info!("stopping service '{worker_id}' (pid {pid})");
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
        if let Some(TreeNode::Worker(w)) = tree.get_mut(worker_id) {
            w.state = ServiceState::Stopping { deadline: None };
        }
    }
}

/// Oneshots in `pending` whose process was reaped elsewhere (e.g. by a
/// restart during boot): settle them in the frontier from their state.
fn settle_lost_oneshots(
    tree: &SupervisorTree,
    pending: &mut HashSet<String>,
    frontier: &mut dependency::frontier::StartupFrontier,
    boot_graph: &DependencyGraph,
) {
    let lost: Vec<String> = pending
        .iter()
        .filter(|n| tree.get_worker(n).is_none_or(|w| w.pid.is_none()))
        .cloned()
        .collect();
    for name in lost {
        pending.remove(&name);
        let state = tree.get_worker(&name).map(|w| w.state.clone());
        match state {
            Some(ServiceState::Exited) | Some(ServiceState::Stopped) => {
                frontier.mark_ready(&name, boot_graph)
            }
            _ => frontier.mark_failed(&name, boot_graph),
        }
    }
}

/// Reap exited children during the startup phase, handling oneshot completion.
/// Oneshot services that exit with code 0 are marked ready in the frontier;
/// those that exit with nonzero are marked failed.
fn reap_oneshots_during_startup(
    tree: &mut SupervisorTree,
    pending_oneshots: &mut HashSet<String>,
    frontier: &mut dependency::frontier::StartupFrontier,
    dep_graph: &DependencyGraph,
    cgroup_hierarchy: &CgroupHierarchy,
    cgroup_monitor: &mut CgroupMonitor,
) {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    loop {
        let (pid, exit) = match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => (
                pid,
                ExitInfo {
                    exit_code: Some(code),
                    signal: None,
                },
            ),
            Ok(WaitStatus::Signaled(pid, sig, _)) => (
                pid,
                ExitInfo {
                    exit_code: None,
                    signal: Some(sig as i32),
                },
            ),
            Ok(WaitStatus::StillAlive) => break,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(nix::errno::Errno::EINTR) => continue,
            _ => break,
        };

        let worker_id = tree.unregister_pid(pid.as_raw());
        if let Some(ref name) = worker_id {
            if pending_oneshots.remove(name.as_str()) {
                // This is a pending oneshot — mark ready or failed
                let remain = tree
                    .get_worker(name)
                    .is_some_and(|w| w.def.service.remain_after_exit);
                if let Some(TreeNode::Worker(w)) = tree.get_mut(name) {
                    w.pid = None;
                    w.state = if exit.is_normal() {
                        if remain {
                            ServiceState::Exited
                        } else {
                            ServiceState::Stopped
                        }
                    } else {
                        ServiceState::Failed {
                            exit_code: exit.exit_code,
                            signal: exit.signal,
                        }
                    };
                }
                cgroup_monitor.unwatch(name);
                if let Some(hierarchy) = cgroup_hierarchy {
                    let _ = hierarchy.remove_service_cgroup(name);
                }
                if exit.is_normal() {
                    tracing::info!("oneshot '{name}' completed (code 0)");
                    frontier.mark_ready(name, dep_graph);
                } else {
                    let reason = match (exit.exit_code, exit.signal) {
                        (Some(code), _) => format!("exit code {code}"),
                        (None, Some(sig)) => format!("killed by signal {sig}"),
                        _ => "abnormal exit".to_string(),
                    };
                    tracing::error!("oneshot '{name}' failed ({reason})");
                    frontier.mark_failed(name, dep_graph);
                    service_failed(tree, name, &reason);
                }
                continue;
            }
        }
        // Not a pending oneshot — handle normally
        if let Some(name) = worker_id {
            // Re-register PID temporarily so handle_child_exit can unregister it
            tree.register_pid(&name, pid.as_raw());
            handle_child_exit(tree, pid.as_raw(), exit, cgroup_hierarchy, cgroup_monitor);
        }
    }
}

/// Reap all zombie children and apply supervisor restart strategies.
fn reap_and_handle_exits(
    tree: &mut SupervisorTree,
    cgroup_hierarchy: &CgroupHierarchy,
    cgroup_monitor: &mut CgroupMonitor,
) {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => {
                handle_child_exit(
                    tree,
                    pid.as_raw(),
                    ExitInfo {
                        exit_code: Some(code),
                        signal: None,
                    },
                    cgroup_hierarchy,
                    cgroup_monitor,
                );
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                handle_child_exit(
                    tree,
                    pid.as_raw(),
                    ExitInfo {
                        exit_code: None,
                        signal: Some(sig as i32),
                    },
                    cgroup_hierarchy,
                    cgroup_monitor,
                );
            }
            Ok(WaitStatus::StillAlive) => break,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(nix::errno::Errno::EINTR) => continue,
            _ => break,
        }
    }
}

/// Handle a single child process exit.
fn handle_child_exit(
    tree: &mut SupervisorTree,
    pid: i32,
    exit_info: ExitInfo,
    cgroup_hierarchy: &CgroupHierarchy,
    cgroup_monitor: &mut CgroupMonitor,
) {
    // Find which worker this PID belongs to
    let worker_id = match tree.unregister_pid(pid) {
        Some(id) => id,
        None => {
            tracing::debug!("reaped unknown pid {pid}");
            return;
        }
    };

    if let Some(code) = exit_info.exit_code {
        tracing::info!("service '{worker_id}' exited (code {code})");
    } else if let Some(sig) = exit_info.signal {
        tracing::warn!("service '{worker_id}' killed (signal {sig})");
    }

    // Clean up cgroup for the exited service
    cgroup_monitor.unwatch(&worker_id);
    if let Some(hierarchy) = cgroup_hierarchy {
        let _ = hierarchy.remove_service_cgroup(&worker_id);
    }

    // Stopped on request, completed oneshot, or the supervisor's strategy
    let decision = supervisor::exit::decide_exit(tree, &worker_id, &exit_info);

    if let Some(ref supervisor_id) = decision.supervisor_failed {
        tracing::error!("supervisor '{supervisor_id}' failed (intensity exceeded)");
        match decision.escalated_to {
            Some(ref parent_id) => tracing::warn!("escalating failure to supervisor '{parent_id}'"),
            None => tracing::error!("root supervisor failed — system degraded"),
        }
    }

    if decision.failed {
        let reason = match (exit_info.exit_code, exit_info.signal) {
            (Some(code), _) => format!("exit code {code}, not restarted"),
            (None, Some(sig)) => format!("killed by signal {sig}, not restarted"),
            _ => "not restarted".to_string(),
        };
        service_failed(tree, &worker_id, &reason);
    }

    for action in &decision.actions {
        execute_strategy_action(tree, action, cgroup_hierarchy, cgroup_monitor);
    }
}

/// Execute the stop/start actions from a strategy decision.
fn execute_strategy_action(
    tree: &mut SupervisorTree,
    action: &strategy::StrategyAction,
    cgroup_hierarchy: &CgroupHierarchy,
    cgroup_monitor: &mut CgroupMonitor,
) {
    // Stop children that need stopping
    for child_id in &action.to_stop {
        if let Some(TreeNode::Worker(w)) = tree.get(child_id) {
            if w.state.is_active() {
                stop_worker(tree, child_id);
            }
        }
    }

    // Brief delay for stops to take effect
    if !action.to_stop.is_empty() {
        std::thread::sleep(Duration::from_millis(100));
        // Reap any stopped processes
        reap_and_handle_exits(tree, cgroup_hierarchy, cgroup_monitor);
    }

    // Start children that need starting
    for child_id in &action.to_start {
        // Get restart delay from the worker's config
        if let Some(w) = tree.get_worker(child_id) {
            let delay = parse_duration_secs(&w.def.restart.delay).unwrap_or(1);
            if delay > 0 {
                tracing::info!("waiting {delay}s before restarting '{child_id}'");
                std::thread::sleep(Duration::from_secs(delay));
            }
        }
        start_worker(tree, child_id, cgroup_hierarchy, cgroup_monitor);
    }
}

/// Reload definitions from the vendor and administrator directories:
/// add new supervisors and services (starting new services that are
/// enabled), update the definitions of existing services in place (used
/// from their next start), and stop services whose definition is gone or
/// masked.
fn reload_services(
    tree: &mut SupervisorTree,
    rt: &mut Runtime,
    cgroup_hierarchy: &CgroupHierarchy,
    cgroup_monitor: &mut CgroupMonitor,
) {
    let root = rt.root.clone();
    let root = root.as_deref();

    for sup_def in sup_config::load_supervisors(&paths::supervisor_dirs(root)) {
        if sup_def.supervisor.name != "root" && tree.get(&sup_def.supervisor.name).is_none() {
            add_supervisor(tree, &sup_def);
        }
    }

    let new_defs = service::load_services(&paths::service_dirs(root));

    let existing: HashSet<String> = tree.all_workers().iter().map(|s| s.to_string()).collect();
    let mut loaded: HashSet<String> = HashSet::new();

    let (mut added, mut updated) = (0, 0);
    for def in new_defs {
        let name = def.service.name.clone();
        if !loaded.insert(name.clone()) {
            tracing::error!(
                "reload: duplicate service name '{name}', ignoring {:?}",
                def.source
            );
            continue;
        }
        if existing.contains(&name) {
            if let Some(w) = tree.get_worker_mut(&name) {
                if w.def.service.supervisor != def.service.supervisor {
                    tracing::warn!(
                        "reload: '{name}' stays under supervisor '{}' until svmgr restarts",
                        w.parent
                    );
                }
                w.restart_policy = def.restart.policy.clone();
                w.def = def;
                updated += 1;
            }
            continue;
        }
        if !add_worker(tree, &def) {
            continue;
        }
        added += 1;
        if config_dirs::enablement(root, &name, def.service.enabled).enabled {
            start_worker(tree, &name, cgroup_hierarchy, cgroup_monitor);
        }
    }

    // Stop services that no longer have a definition
    let mut removed = 0;
    for name in &existing {
        if !loaded.contains(name) {
            stop_worker(tree, name);
            removed += 1;
        }
    }

    rt.dep_graph = DependencyGraph::build(&tree_defs(tree));
    if let Err(e) = dependency::cycle::validate_no_cycles(&rt.dep_graph) {
        tracing::error!("reload: {e}");
    }

    tracing::info!("reload complete: {added} added, {updated} updated, {removed} removed");
}
