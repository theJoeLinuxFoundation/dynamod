/// Service readiness detection.
///
/// Supports multiple readiness mechanisms:
/// - **none**: Ready immediately after exec
/// - **notify**: sd_notify compatible (READY=1 on a Unix datagram socket)
/// - **tcp-port**: Poll a TCP port until it accepts connections
/// - **exec**: Run a health-check command; ready when it exits 0
/// - **fd**: Service writes a byte to a passed file descriptor
use nix::errno::Errno;
use nix::sys::socket::{
    ControlMessageOwned, MsgFlags, UnixAddr, bind, recvmsg, setsockopt, sockopt,
};
use std::io::{self, IoSliceMut};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::config::service::{ReadinessSection, ReadinessType};

/// Result of a readiness check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessResult {
    /// Service is ready.
    Ready,
    /// Still waiting for readiness.
    NotReady,
    /// Readiness check timed out.
    TimedOut,
    /// Readiness check failed (unrecoverable).
    Failed(String),
}

/// Tracks readiness state for a single service.
#[derive(Debug)]
pub struct ReadinessTracker {
    readiness_type: ReadinessType,
    port: Option<u16>,
    check_exec: Option<Vec<String>>,
    timeout: Duration,
    started_at: Instant,
    notify_socket: Option<PathBuf>,
    /// Bound notify socket (created once, polled on each check)
    bound_notify: Option<UnixDatagram>,
    /// Read end of the fd-based readiness pipe (parent polls this).
    ready_fd: Option<OwnedFd>,
}

impl ReadinessTracker {
    /// Create a readiness tracker from the service's readiness config.
    pub fn new(config: &ReadinessSection, service_name: &str) -> io::Result<Self> {
        let timeout = crate::config::service::parse_duration_secs(&config.timeout)
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(30));

        let notify_socket = if config.readiness_type == ReadinessType::Notify {
            let root = dynamod_common::paths::root_from_env();
            let dir =
                dynamod_common::paths::rooted(root.as_deref(), dynamod_common::paths::RUN_DIR);
            Some(dir.join(format!("notify-{service_name}.sock")))
        } else {
            None
        };
        let bound_notify = notify_socket.as_deref().map(bind_notify).transpose()?;

        Ok(Self {
            readiness_type: config.readiness_type.clone(),
            port: config.port,
            check_exec: config.check_exec.clone(),
            timeout,
            started_at: Instant::now(),
            notify_socket,
            bound_notify,
            ready_fd: None,
        })
    }

    /// Get the NOTIFY_SOCKET path (for sd_notify compatible services).
    pub fn notify_socket_path(&self) -> Option<&Path> {
        self.notify_socket.as_deref()
    }

    /// Set the read end of the fd-based readiness pipe.
    pub fn set_ready_fd(&mut self, fd: OwnedFd) {
        self.ready_fd = Some(fd);
    }

    /// Check if this service is immediately ready (type = none).
    pub fn is_immediate(&self) -> bool {
        self.readiness_type == ReadinessType::None
    }

    /// Perform a readiness check. Non-blocking.
    pub fn check(&self, main_pid: i32) -> ReadinessResult {
        if self.started_at.elapsed() > self.timeout {
            return ReadinessResult::TimedOut;
        }

        match self.readiness_type {
            ReadinessType::None => ReadinessResult::Ready,
            ReadinessType::TcpPort => self.check_tcp_port(),
            ReadinessType::Exec => self.check_exec(),
            ReadinessType::Notify => self.check_notify(main_pid),
            ReadinessType::Fd => self.check_fd(),
        }
    }

    /// Check if a TCP port is accepting connections.
    fn check_tcp_port(&self) -> ReadinessResult {
        let port = match self.port {
            Some(p) => p,
            None => return ReadinessResult::Failed("no port configured".into()),
        };

        let addr = format!("127.0.0.1:{port}");
        match TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(200)) {
            Ok(_) => ReadinessResult::Ready,
            Err(_) => ReadinessResult::NotReady,
        }
    }

    /// Run a health-check command and check if it exits 0.
    fn check_exec(&self) -> ReadinessResult {
        let cmd = match &self.check_exec {
            Some(c) if !c.is_empty() => c,
            _ => return ReadinessResult::Failed("no check-exec configured".into()),
        };

        match std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => ReadinessResult::Ready,
            Ok(_) => ReadinessResult::NotReady,
            Err(e) => ReadinessResult::Failed(format!("check-exec error: {e}")),
        }
    }

    /// Check for sd_notify readiness (READY=1 on the notify socket).
    fn check_notify(&self, main_pid: i32) -> ReadinessResult {
        let Some(sock) = &self.bound_notify else {
            return ReadinessResult::Failed("no notify socket".into());
        };
        match receive_notify(sock, main_pid) {
            Ok(true) => ReadinessResult::Ready,
            Ok(false) => ReadinessResult::NotReady,
            Err(e) => ReadinessResult::Failed(format!("notify socket: {e}")),
        }
    }

    /// Check for fd-based readiness (service writes a byte to the pipe).
    fn check_fd(&self) -> ReadinessResult {
        let fd = match &self.ready_fd {
            Some(fd) => fd,
            None => return ReadinessResult::Failed("no ready fd configured".into()),
        };

        // Non-blocking read: check if any byte has arrived
        let mut buf = [0u8; 1];
        let result =
            unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, 1) };

        if result > 0 {
            ReadinessResult::Ready
        } else if result == 0 {
            // Pipe closed without writing — service exited without signaling
            ReadinessResult::Failed("ready fd closed without signal".into())
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                ReadinessResult::NotReady
            } else {
                ReadinessResult::Failed(format!("ready fd read error: {err}"))
            }
        }
    }
}

impl Drop for ReadinessTracker {
    fn drop(&mut self) {
        if let Some(path) = &self.notify_socket {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn bind_notify(path: &Path) -> io::Result<UnixDatagram> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let sock = UnixDatagram::unbound()?;
    setsockopt(&sock, sockopt::PassCred, &true)?;
    sock.set_nonblocking(true)?;
    bind(sock.as_raw_fd(), &UnixAddr::new(path)?)?;
    // A daemon may drop its uid before sending READY=1.
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)) {
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(sock)
}

fn receive_notify(sock: &UnixDatagram, main_pid: i32) -> io::Result<bool> {
    // Bound each poll so a busy socket cannot monopolize the service manager.
    let mut buf = [0; 8192];
    let mut control = nix::cmsg_space!(libc::ucred);
    for _ in 0..16 {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = match recvmsg::<()>(
            sock.as_raw_fd(),
            &mut iov,
            Some(&mut control),
            MsgFlags::MSG_CMSG_CLOEXEC,
        ) {
            Ok(msg) => msg,
            Err(Errno::EAGAIN) => return Ok(false),
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        };
        if msg
            .flags
            .intersects(MsgFlags::MSG_CTRUNC | MsgFlags::MSG_TRUNC)
        {
            continue;
        }
        let authenticated = msg.cmsgs()?.any(|cmsg| {
            matches!(
                cmsg, ControlMessageOwned::ScmCredentials(cred) if cred.pid() == main_pid
            )
        });
        let n = msg.bytes;
        if authenticated
            && buf[..n]
                .split(|&b| b == b'\n')
                .any(|field| field == b"READY=1")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_requires_main_pid_and_complete_ready_field() {
        let pid = std::process::id() as i32;
        let path = std::env::temp_dir().join(format!("dynamod-notify-test-{pid}.sock"));
        let socket = bind_notify(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o666
        );
        let sender = UnixDatagram::unbound().unwrap();
        sender.send_to(b"READY=1", &path).unwrap();
        // The real sender has the same uid as the authorized service.
        assert!(!receive_notify(&socket, pid + 1).unwrap());
        for field in [b"STATUS=READY=1".as_slice(), b"READY=10", b"NOTREADY=1"] {
            sender.send_to(field, &path).unwrap();
            assert!(!receive_notify(&socket, pid).unwrap());
        }
        sender
            .send_to(b"STATUS=clock synchronized\nREADY=1\n", &path)
            .unwrap();
        assert!(receive_notify(&socket, pid).unwrap());
        assert!(!receive_notify(&socket, pid).unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_none_readiness_immediate() {
        let config = ReadinessSection {
            readiness_type: ReadinessType::None,
            port: None,
            check_exec: None,
            timeout: "30s".into(),
        };
        let tracker = ReadinessTracker::new(&config, "test").unwrap();
        assert!(tracker.is_immediate());
        assert_eq!(
            tracker.check(std::process::id() as i32),
            ReadinessResult::Ready
        );
    }

    #[test]
    fn test_tcp_port_not_ready() {
        let config = ReadinessSection {
            readiness_type: ReadinessType::TcpPort,
            port: Some(19999), // unlikely to be listening
            check_exec: None,
            timeout: "5s".into(),
        };
        let tracker = ReadinessTracker::new(&config, "test").unwrap();
        assert!(!tracker.is_immediate());
        assert_eq!(
            tracker.check(std::process::id() as i32),
            ReadinessResult::NotReady
        );
    }

    #[test]
    fn test_exec_readiness_true() {
        let config = ReadinessSection {
            readiness_type: ReadinessType::Exec,
            port: None,
            check_exec: Some(vec!["true".into()]),
            timeout: "5s".into(),
        };
        let tracker = ReadinessTracker::new(&config, "test").unwrap();
        assert_eq!(
            tracker.check(std::process::id() as i32),
            ReadinessResult::Ready
        );
    }

    #[test]
    fn test_exec_readiness_false() {
        let config = ReadinessSection {
            readiness_type: ReadinessType::Exec,
            port: None,
            check_exec: Some(vec!["false".into()]),
            timeout: "5s".into(),
        };
        let tracker = ReadinessTracker::new(&config, "test").unwrap();
        assert_eq!(
            tracker.check(std::process::id() as i32),
            ReadinessResult::NotReady
        );
    }
}
