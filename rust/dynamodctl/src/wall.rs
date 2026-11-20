/// A minimal wall(1): write a message to every terminal that accepts
/// messages (group-writable, i.e. `mesg y`), without utmp.
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

fn terminals() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev/pts") {
        for e in entries.flatten() {
            if e.file_name() != "ptmx" {
                paths.push(e.path());
            }
        }
    }
    for n in 1..=63 {
        paths.push(PathBuf::from(format!("/dev/tty{n}")));
    }
    paths
        .into_iter()
        .filter(|p| {
            std::fs::metadata(p).is_ok_and(|m| {
                m.file_type().is_char_device() && m.permissions().mode() & 0o020 != 0
            })
        })
        .collect()
}

/// Broadcast `message` to logged-in terminals. Errors are ignored.
pub fn broadcast(message: &str) {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "localhost".into());
    let text = format!(
        "\r\nBroadcast message from root@{host} (dynamod):\r\n\r\n{}\r\n\r\n",
        message.replace('\n', "\r\n")
    );
    for path in terminals() {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(&path)
        {
            let _ = f.write_all(text.as_bytes());
        }
    }
}
