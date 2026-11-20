//! dynamod-sdconf: what dynamod-tmpfiles and dynamod-sysusers share.
//!
//! Both read systemd-style `*.d` configuration (tmpfiles.d(5), sysusers.d(5))
//! and may operate on another root (`--root=DIR`), so they share:
//!
//! - [`confdirs`]: the search directories, their precedence and masking;
//! - [`words`]: splitting a line into fields with quotes and C escapes, the
//!   way systemd's `extract_first_word()` does;
//! - [`specifier`]: `%x` specifier expansion;
//! - [`rootfs`]: resolving paths inside the root, symlinks included;
//! - [`accounts`]: reading the root's own `/etc/passwd` and `/etc/group`
//!   (never the C library's name service, which knows the running system's
//!   accounts and, on bionic, not those files at all);
//! - [`log`]: messages on standard error.
pub mod accounts;
pub mod confdirs;
pub mod log;
pub mod rootfs;
pub mod specifier;
pub mod words;

pub use rootfs::Root;
