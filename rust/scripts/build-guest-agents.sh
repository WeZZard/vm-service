#!/bin/sh
# Build the guest console agent for every guest OS this host can serve, and
# install each build under the name the daemon looks for.
#
# The Python original shipped `guest-console-agent.py`: one artifact that runs
# on any guest with an interpreter. The native port streams machine code, so a
# single artifact is no longer correct — the bytes must match the *guest's*
# operating system, not the host's. A macOS host leasing a Linux guest would
# otherwise upload a Mach-O image that the guest cannot execute, which shows up
# as a probe that never answers.
#
# `console::guest::agent_path_for` resolves, in order:
#   1. $VM_GUEST_CONSOLE_AGENT                      (explicit override)
#   2. guest-console-agent-<kind> beside the daemon (this script produces these)
#   3. guest-console-agent beside the daemon        (host-native fallback)
#
# Usage: scripts/build-guest-agents.sh [output-dir]
# The output dir must not be cargo's own target dir, or the copies are no-ops.
set -eu

cd "$(dirname "$0")/.."
out="${1:-target/guest-agents}"
target_dir="${CARGO_TARGET_DIR:-target}"
mkdir -p "$out"

echo "building host-native agent -> $out/guest-console-agent"
cargo build -p guest-console-agent
cp "$target_dir/debug/guest-console-agent" "$out/guest-console-agent"

# Linux guests cannot execute the host build, so cross-compile the same crate.
target=aarch64-unknown-linux-musl
if rustup target list --installed | grep -qx "$target"; then
    echo "building linux agent ($target) -> $out/guest-console-agent-linux"
    CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
        cargo build -p guest-console-agent --target "$target"
    cp "$target_dir/$target/debug/guest-console-agent" "$out/guest-console-agent-linux"
else
    echo "error: rust target $target is not installed;" >&2
    echo "       run: rustup target add $target" >&2
    echo "       Linux guests cannot be served without a Linux agent binary." >&2
    exit 1
fi

echo "done:"
ls -l "$out/guest-console-agent" "$out/guest-console-agent-linux"
