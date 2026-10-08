#!/usr/bin/env bash
# fork/regen-in-container.sh — run assemble's snapshot regen on Linux.
#
#   regen-in-container.sh WORKTREE REGEN_LOG [FILTER]
#
# assemble regenerates snapshots by running the tui, core and cli suites under
# INSTA_UPDATE=always, and records whatever they render. On macOS that is not
# what CI sees: every thread start reconfigures an FSEvents watch, which can
# block for seconds, so tests miss their own deadlines and never reach the
# assertion that would rewrite their snapshot. CI runs Linux, where the same
# watch is an inotify call. This runs the regen there: in a podman container
# on ubuntu-24.04, the image ore-ci uses, as an unprivileged user with
# bubblewrap, as ore-ci does.
#
# The container persists (`ore-regen`) so its target directory and registry
# survive between assemblies. The worktree's codex-rs tree is copied in, the
# suites run one crate at a time, and every .snap file is copied back.
#
# Exit status follows nextest's: 0 green, 100 tests failed (snapshots were
# still rewritten), anything else means the suites did not build.
set -euo pipefail

WORKTREE=${1:?worktree}
REGEN_LOG=${2:?regen log}
FILTER=${3:-}
NAME=ore-regen
IMAGE=docker.io/library/ubuntu:24.04
TOOLCHAIN=1.95.0
PACKAGES=(codex-tui codex-core codex-cli)

if [[ -z "${CONTAINER_HOST:-}" ]]; then
  sock=$(podman machine inspect --format '{{.ConnectionInfo.PodmanSocket.Path}}' 2>/dev/null || true)
  [[ -n "$sock" ]] && export CONTAINER_HOST="unix://$sock"
fi
if ! podman info >/dev/null 2>&1; then
  # `machine start` can report an ssh known_hosts complaint after the VM is up;
  # whether the API answers is what matters.
  podman machine start >/dev/null 2>&1 || true
  podman info >/dev/null 2>&1 || { echo "regen-in-container: podman is not reachable" >&2; exit 2; }
fi

if ! podman container exists "$NAME"; then
  podman run -d --name "$NAME" --privileged "$IMAGE" sleep infinity >/dev/null
fi
podman start "$NAME" >/dev/null
# Setup is keyed on a marker, not on the container existing, so an
# interrupted first run is completed rather than mistaken for a ready one.
if ! podman exec "$NAME" test -f /opt/.ore-regen-ready; then
  podman exec "$NAME" bash -c "
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends curl ca-certificates build-essential \
      pkg-config libcap-dev bubblewrap git libssl-dev cmake clang zsh python3 >/dev/null
    curl -sSf https://sh.rustup.rs | sh -s -- -y -q --default-toolchain $TOOLCHAIN \
      --profile minimal --no-modify-path >/dev/null
    arch=\$(uname -m); [[ \$arch == aarch64 ]] && nt=linux-arm || nt=linux
    curl -LsSf https://get.nexte.st/latest/\$nt | tar zxf - -C /opt/cargo/bin
    chmod -R a+rwX /opt/rustup /opt/cargo
    id t >/dev/null 2>&1 || useradd -m t
    mkdir -p /w && chown t /w
    touch /opt/.ore-regen-ready
  "
fi

# The V8 prebuilt for the container's own target, fetched once per version.
version=$(python3 "$WORKTREE/.github/scripts/rusty_v8_bazel.py" resolved-v8-crate-version)
podman exec -u t "$NAME" bash -c "
  set -euo pipefail
  [[ -f /w/v8-$version/lib.a.gz ]] && exit 0
  arch=\$(uname -m)
  target=\$arch-unknown-linux-gnu
  base=https://github.com/openai/codex/releases/download/rusty-v8-v$version
  mkdir -p /w/v8-$version
  curl -fsSL \"\$base/librusty_v8_ptrcomp_sandbox_release_\$target.a.gz\" -o /w/v8-$version/lib.a.gz
  curl -fsSL \"\$base/src_binding_ptrcomp_sandbox_release_\$target.rs\" -o /w/v8-$version/binding.rs
"

# A fresh copy of the tree each time; the target directory stays.
podman exec "$NAME" bash -c "rm -rf /w/src && mkdir -p /w/src && chown t /w/src"
COPYFILE_DISABLE=1 tar --no-xattrs -C "$WORKTREE" --exclude 'codex-rs/target' -cf - codex-rs .github/scripts fork \
  | podman exec -i -u t "$NAME" tar -C /w/src -xf -

ENV="export PATH=/opt/cargo/bin:\$PATH RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
  export CARGO_TARGET_DIR=/w/target RUST_MIN_STACK=8388608
  export RUSTY_V8_ARCHIVE=/w/v8-$version/lib.a.gz RUSTY_V8_SRC_BINDING_PATH=/w/v8-$version/binding.rs"

# Tests find sibling binaries (the CLI, the code-mode host, the MCP test
# servers) in the target directory, which a per-crate run does not build.
# ore-ci tests the whole workspace and so always has them; build them here.
podman exec -u t "$NAME" bash -c "$ENV
  cd /w/src/codex-rs && cargo build --workspace --bins --exclude codex-voice-host
" >>"$REGEN_LOG" 2>&1 || exit $?

rc=0
for pkg in "${PACKAGES[@]}"; do
  pkg_rc=0
  podman exec -u t -e FILTER="$FILTER" "$NAME" bash -c "$ENV
    export INSTA_UPDATE=always
    cd /w/src/codex-rs
    args=(--no-fail-fast -p $pkg)
    [[ -n \"\$FILTER\" ]] && args+=(-E \"not (\$FILTER)\")
    cargo nextest run \"\${args[@]}\"
  " >>"$REGEN_LOG" 2>&1 || pkg_rc=$?
  if [[ "$pkg_rc" -ne 0 && "$pkg_rc" -ne 100 ]]; then
    exit "$pkg_rc"
  fi
  if [[ "$pkg_rc" -eq 100 ]]; then rc=100; fi
done

podman exec -u t "$NAME" bash -c "cd /w/src && find codex-rs -name '*.snap' -o -name '*.pending-snap' | tar -cf - -T -" \
  | tar -C "$WORKTREE" -xf -
exit "$rc"
