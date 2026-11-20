#!/bin/sh
# Install dynamod into $DESTDIR$PREFIX. Invoked by `neomake run install`.
set -eu

: "${DESTDIR:=}"
: "${PREFIX:=/usr}"

ZIG_OUT="zig/zig-out/bin"
if [ -d rust/target/x86_64-unknown-linux-musl/release ]; then
    CARGO_OUT="rust/target/x86_64-unknown-linux-musl/release"
else
    CARGO_OUT="rust/target/release"
fi

install -Dm755 "$ZIG_OUT/dynamod-init"        "$DESTDIR$PREFIX/sbin/dynamod-init"
install -Dm755 "$CARGO_OUT/dynamod-svmgr"     "$DESTDIR$PREFIX/lib/dynamod/dynamod-svmgr"
install -Dm755 "$CARGO_OUT/dynamodctl"        "$DESTDIR$PREFIX/bin/dynamodctl"
install -Dm755 "$CARGO_OUT/dynamod-logd"      "$DESTDIR$PREFIX/lib/dynamod/dynamod-logd"
install -Dm755 "$CARGO_OUT/dynamod-logind"    "$DESTDIR$PREFIX/lib/dynamod/dynamod-logind"
install -Dm755 "$CARGO_OUT/dynamod-sd1bridge" "$DESTDIR$PREFIX/lib/dynamod/dynamod-sd1bridge"
install -Dm755 "$CARGO_OUT/dynamod-hostnamed" "$DESTDIR$PREFIX/lib/dynamod/dynamod-hostnamed"

# Definitions: packages install into $PREFIX/lib/dynamod/{services,supervisors};
# /etc/dynamod/... is the administrator's (same file name replaces, a symlink
# to /dev/null masks). enabled/ and disabled/ hold `dynamodctl enable` markers.
install -dm755 "$DESTDIR$PREFIX/lib/dynamod/services"
install -dm755 "$DESTDIR$PREFIX/lib/dynamod/supervisors"
install -dm755 "$DESTDIR/etc/dynamod/services"
install -dm755 "$DESTDIR/etc/dynamod/supervisors"
install -dm755 "$DESTDIR/etc/dynamod/enabled"
install -dm755 "$DESTDIR/etc/dynamod/disabled"
install -Dm644 config/supervisors/root.toml "$DESTDIR/etc/dynamod/supervisors/root.toml"

# dynamodctl is also reboot, poweroff, halt, shutdown and systemctl
# (multi-call, by program name). COMPAT_LINKS=0 skips these links, e.g. on
# a system that keeps other implementations of them.
: "${COMPAT_LINKS:=1}"
if [ "$COMPAT_LINKS" = 1 ]; then
    install -dm755 "$DESTDIR$PREFIX/sbin"
    for cmd in reboot poweroff halt shutdown; do
        ln -sfn ../bin/dynamodctl "$DESTDIR$PREFIX/sbin/$cmd"
    done
    ln -sfn dynamodctl "$DESTDIR$PREFIX/bin/systemctl"
fi

install -dm755 "$DESTDIR/usr/share/dbus-1/system.d"
for f in config/dbus-1/*.conf; do
    install -Dm644 "$f" "$DESTDIR/usr/share/dbus-1/system.d/$(basename "$f")"
done

# tmpfiles.d and sysusers.d (docs/compat.md): dynamod-tmpfiles and
# dynamod-sysusers, also as systemd-tmpfiles and systemd-sysusers, the names
# packages and package managers run (skipped with COMPAT_LINKS=0). Their boot
# services are config/services/{sysusers,tmpfiles-setup}.toml.
install -Dm755 "$CARGO_OUT/dynamod-tmpfiles"  "$DESTDIR$PREFIX/bin/dynamod-tmpfiles"
install -Dm755 "$CARGO_OUT/dynamod-sysusers"  "$DESTDIR$PREFIX/bin/dynamod-sysusers"
if [ "${COMPAT_LINKS:-1}" = 1 ]; then
    ln -sfn dynamod-tmpfiles "$DESTDIR$PREFIX/bin/systemd-tmpfiles"
    ln -sfn dynamod-sysusers "$DESTDIR$PREFIX/bin/systemd-sysusers"
fi
