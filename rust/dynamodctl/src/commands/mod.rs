/// dynamodctl command implementations.
///
/// Each command connects to the control socket, sends a request,
/// and prints the response.
use dynamod_common::protocol::{MessageBody, ShutdownKind};

use crate::client::send_request;

type Result = std::result::Result<(), Box<dyn std::error::Error>>;

pub fn start(socket_path: &str, name: &str) -> Result {
    match send_request(
        socket_path,
        MessageBody::StartService {
            name: name.to_string(),
        },
    )? {
        MessageBody::Ack => {
            println!("Starting service '{name}'...");
            Ok(())
        }
        MessageBody::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}

pub fn stop(socket_path: &str, name: &str) -> Result {
    match send_request(
        socket_path,
        MessageBody::StopService {
            name: name.to_string(),
        },
    )? {
        MessageBody::Ack => {
            println!("Stopping service '{name}'...");
            Ok(())
        }
        MessageBody::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}

pub fn restart(socket_path: &str, name: &str) -> Result {
    match send_request(
        socket_path,
        MessageBody::RestartService {
            name: name.to_string(),
        },
    )? {
        MessageBody::Ack => {
            println!("Restarting service '{name}'...");
            Ok(())
        }
        MessageBody::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}

pub fn status(socket_path: &str, name: &str) -> Result {
    match send_request(
        socket_path,
        MessageBody::DescribeService {
            name: name.to_string(),
        },
    )? {
        MessageBody::ServiceDetails(d) => {
            println!("Service: {}", d.name);
            println!("  Status:     {}", d.status);
            if let Some(p) = d.pid {
                println!("  PID:        {p}");
            }
            println!("  Supervisor: {}", d.supervisor);
            println!(
                "  Boot:       {} ({})",
                if d.enabled { "enabled" } else { "disabled" },
                d.enabled_source
            );
            println!("  Type:       {}", d.service_type);
            if let Some(ref src) = d.source {
                println!("  Definition: {src}");
            }
            let mut options = Vec::new();
            if d.remain_after_exit {
                options.push("remain-after-exit".to_string());
            }
            if d.after_all {
                options.push("after-all".to_string());
            }
            if d.last {
                options.push("last".to_string());
            }
            if d.failure_action != "none" {
                options.push(format!("failure-action={}", d.failure_action));
            }
            if !options.is_empty() {
                println!("  Options:    {}", options.join(", "));
            }
            Ok(())
        }
        // An svmgr without DescribeService
        MessageBody::Error { message } if message == "unknown command" => {
            status_basic(socket_path, name)
        }
        MessageBody::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}

fn status_basic(socket_path: &str, name: &str) -> Result {
    match send_request(
        socket_path,
        MessageBody::ServiceStatus {
            name: name.to_string(),
        },
    )? {
        MessageBody::ServiceInfo {
            name,
            status,
            pid,
            supervisor,
        } => {
            println!("Service: {name}");
            println!("  Status:     {status}");
            if let Some(p) = pid {
                println!("  PID:        {p}");
            }
            println!("  Supervisor: {supervisor}");
            Ok(())
        }
        MessageBody::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}

pub fn list(socket_path: &str) -> Result {
    match send_request(socket_path, MessageBody::ListServices)? {
        MessageBody::ServiceList { services } => {
            if services.is_empty() {
                println!("No services configured.");
                return Ok(());
            }

            // Print table header
            println!(
                "{:<30} {:<15} {:<10} {:<8}",
                "SERVICE", "STATUS", "PID", "BOOT"
            );
            println!("{}", "-".repeat(64));

            let mut services = services;
            services.sort_by(|a, b| a.name.cmp(&b.name));

            for svc in &services {
                let pid_str = svc.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
                let boot = match svc.enabled {
                    Some(true) => "enabled",
                    Some(false) => "disabled",
                    None => "-",
                };
                println!(
                    "{:<30} {:<15} {:<10} {:<8}",
                    svc.name, svc.status, pid_str, boot
                );
            }

            println!("\n{} service(s) total", services.len());
            Ok(())
        }
        MessageBody::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}

pub fn tree(socket_path: &str) -> Result {
    match send_request(socket_path, MessageBody::TreeStatus)? {
        MessageBody::Tree { text } => {
            print!("{text}");
            Ok(())
        }
        MessageBody::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}

pub fn reload(socket_path: &str) -> Result {
    match send_request(socket_path, MessageBody::Reload)? {
        MessageBody::Ack => {
            println!("Reloading service definitions...");
            Ok(())
        }
        MessageBody::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}

/// `dynamodctl shutdown [poweroff|reboot|halt]`: immediate clean shutdown.
pub fn shutdown(socket_path: &str, kind: &str) -> Result {
    let shutdown_kind = match kind {
        "poweroff" | "off" => ShutdownKind::Poweroff,
        "reboot" => ShutdownKind::Reboot,
        "halt" => ShutdownKind::Halt,
        _ => {
            return Err(
                format!("unknown shutdown kind: {kind} (use poweroff, reboot, or halt)").into(),
            );
        }
    };

    match send_request(
        socket_path,
        MessageBody::Shutdown {
            kind: shutdown_kind,
        },
    )? {
        MessageBody::Ack => {
            println!("System {kind} initiated.");
            Ok(())
        }
        MessageBody::Error { message } => Err(message.into()),
        _ => Err("unexpected response".into()),
    }
}
