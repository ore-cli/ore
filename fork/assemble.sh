#!/usr/bin/env bash
#
# fork/assemble.sh — upstream tag -> generated `main` candidate, in one pass.
#
#   assemble.sh --tag rust-vX.Y.Z [--base rust-vA.B.C] [--delta <ref>]
#               [--worktree <dir>] [--agent "<cmd>"] [--skip-heavy] [--reassemble]
#
# The series is rebased FIRST, onto the pristine tag, and the generated passes
# run second. Substituting before rebasing would rewrite the context lines every
# series hunk anchors on — mass conflicts, and every manifest change would shift
# the conflict text so no rerere record ever converges.
#
#   0. fetch upstream tags into refs/upstream/tags/*   (never local tags)
#   1. restore the rerere cache from the orphan branch rerere-cache
#   2. worktree: branch sync-delta/<tag> = delta; rebase --onto TAG BASE
#   3. generated passes: relocate workflows, substitutions, cargo fmt,
#      version/lock, schema regen [heavy], snapshot regen [heavy], fork/UPSTREAM
#   4. one assembly commit; then M = commit-tree TREE -p prev-main -p TAG^{commit}
#      written to refs/heads/sync/<tag>   (--reassemble: single parent)
#   5. snapshot the rerere cache back out
#
# Exit codes:
#   0  clean — refs/heads/sync/<tag> points at the merge candidate
#   2  unresolved rebase conflicts (state left in the worktree for a human;
#      an agent violation aborts the rebase instead — its edits are untrusted)
#   3  generated-pass failure (substitution check/audit, fmt, lock, schema,
#      snapshots) — worktree kept for inspection
#   4  precondition failure — nothing was touched
#
# This script NEVER pushes and never moves refs/heads/{main,delta}; the only
# refs it writes are sync-delta/<tag>, sync/<tag> and rerere-cache. Pushing is
# the sync workflow's job, through safe_push() only.
#
# On success the assembly worktree is removed (set WORKTREE_KEEP=1 to inspect
# it in place instead); on failure it is always kept.
#
# Assembly is reproducible: two runs over the same tag and series produce the
# same tree, with `assembled_at` the single deliberate exception. Set
# ORE_ASSEMBLED_AT to the first run's stamp and the second run's tree hash is
# identical -- which is how I-TREE proves the generated passes carry no other
# hidden nondeterminism (timestamps, ordering, temp paths).

set -euo pipefail

FORK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$FORK_DIR/.." && pwd)"
# shellcheck source=lib.sh
. "$FORK_DIR/lib.sh"

BOT_NAME="ore-sync[bot]"
BOT_EMAIL="ore-sync[bot]@users.noreply.github.com"
# rerere is enabled per-invocation, never via global config, so read-only
# clones are unaffected and CI needs no setup step. gc.auto=0 prevents a
# mid-run gc from pruning fresh rr-cache entries before the snapshot.
RERERE_CFG=(-c rerere.enabled=true -c rerere.autoupdate=true -c gc.auto=0)
AGENT_MAX_STOPS=25
AGENT_MAX_SECONDS=3600

usage() {
  sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

fail_pre() { printf '\033[31mprecondition:\033[0m %s\n' "$*" >&2; exit 4; }
fail_pass() { printf '\033[31mpass failed:\033[0m %s\n' "$*" >&2; keep_state_hint; exit 3; }
fail_conflict() { printf '\033[31mconflict:\033[0m %s\n' "$*" >&2; keep_state_hint; exit 2; }
keep_state_hint() {
  [[ -n "${WORKTREE:-}" && -d "${WORKTREE:-/nonexistent}" ]] \
    && printf 'state kept in %s (remove with: git worktree remove --force %q; git branch -D %q)\n' \
         "$WORKTREE" "$WORKTREE" "sync-delta/$TAG" >&2 || true
}

# ---------------------------------------------------------------- arguments

TAG="" BASE="" DELTA="refs/heads/delta" WORKTREE="" AGENT_CMD="" SKIP_HEAVY=0 REASSEMBLE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)        TAG="${2:?--tag needs a value}"; shift 2 ;;
    --base)       BASE="${2:?--base needs a value}"; shift 2 ;;
    --delta)      DELTA="${2:?--delta needs a value}"; shift 2 ;;
    --worktree)   WORKTREE="${2:?--worktree needs a value}"; shift 2 ;;
    --agent)      AGENT_CMD="${2:?--agent needs a value}"; shift 2 ;;
    --skip-heavy) SKIP_HEAVY=1; shift ;;
    --reassemble) REASSEMBLE=1; shift ;;
    -h|--help)    usage; exit 0 ;;
    *)            usage >&2; fail_pre "unknown argument '$1'" ;;
  esac
done

# --------------------------------------------------------------- preconditions

cd "$REPO_ROOT"
[[ -n "$TAG" ]] || { usage >&2; fail_pre "--tag is required"; }
[[ "$TAG" =~ $ORE_STABLE_TAG_RE ]] \
  || fail_pre "'$TAG' is not a stable tag (grammar: rust-vX.Y.Z; alphas/betas are never a base)"
# Subshell: the helper die()s with exit 1; preconditions must exit 4.
( require_clean_tree "$REPO_ROOT" ) || fail_pre "dirty working tree"

# Upstream tags are fetched into a NON-TAG namespace so no upstream-named tag
# ref ever exists locally: even a reflexive `git push --tags` then has nothing
# upstream-shaped to push (layer 1 of the tag defence, see fork/lib.sh).
# Offline is tolerated when the refs already resolve from a previous fetch.
if git remote get-url upstream >/dev/null 2>&1; then
  git fetch --no-tags upstream '+refs/tags/rust-v*:refs/upstream/tags/rust-v*' 2>/dev/null \
    || warn "fetch from upstream failed; falling back to already-local refs"
else
  warn "no 'upstream' remote configured; using already-local refs"
fi

TAG_REF=$(resolve_upstream_tag "$TAG") || fail_pre "tag '$TAG' not found (refs/upstream/tags/ or refs/tags/)"
TAG_COMMIT=$(peel_tag "$TAG_REF") || fail_pre "cannot peel $TAG_REF"

git rev-parse --verify --quiet "$DELTA" >/dev/null || fail_pre "delta ref '$DELTA' does not exist"

if [[ -z "$BASE" ]]; then
  # The authoritative base is what main was last assembled from; fall back to
  # the delta copy (a tracked placeholder, stale-ok) before origin/main exists.
  for src in refs/remotes/origin/main "$DELTA"; do
    BASE=$(git show "$src:fork/UPSTREAM" 2>/dev/null | sed -n 's/^tag = "\(.*\)"$/\1/p') && [[ -n "$BASE" ]] && break
  done
  [[ -n "$BASE" ]] || fail_pre "cannot derive --base from fork/UPSTREAM on origin/main or '$DELTA'"
fi
[[ "$BASE" =~ $ORE_STABLE_TAG_RE ]] || fail_pre "base '$BASE' is not a stable tag"
BASE_REF=$(resolve_upstream_tag "$BASE") || fail_pre "base tag '$BASE' not found"
BASE_COMMIT=$(peel_tag "$BASE_REF") || fail_pre "cannot peel $BASE_REF"

git merge-base --is-ancestor "$BASE_COMMIT" "$DELTA" \
  || fail_pre "base $BASE ($BASE_COMMIT) is not an ancestor of '$DELTA' — fork/UPSTREAM and the series disagree"

PREV=$(git rev-parse --verify --quiet refs/remotes/origin/main \
       || git rev-parse --verify --quiet refs/heads/main \
       || true)

if [[ "$TAG" == "$BASE" && "$REASSEMBLE" -eq 0 && -n "$PREV" ]]; then
  fail_pre "--tag equals the current base; a fork-side regeneration needs --reassemble"
fi
if [[ "$REASSEMBLE" -eq 1 && "$TAG" != "$BASE" ]]; then
  fail_pre "--reassemble regenerates the SAME base (--tag $TAG != base $BASE); drop the flag to sync forward"
fi

# The previous generated main: origin/main once the fork is pushed, the local
# branch during bootstrap. Resolved up front so --reassemble can fail fast.
# Bootstrap: with no generated main yet, assembling the current base IS the first
# main (human checklist item 7 pushes delta and this candidate together). Nothing
# to re-generate from, so --reassemble is neither needed nor accepted.
[[ "$REASSEMBLE" -eq 1 && -z "$PREV" ]] \
  && fail_pre "--reassemble needs an existing main to re-generate from; drop it to bootstrap the first main"

# A stale `main` left tracking upstream is the failure this catches: parenting a
# candidate onto upstream/main makes upstream commits reachable as if the fork
# had generated them, and the merge-shape check only notices afterwards.
if [[ -n "$PREV" ]] && ! git cat-file -e "$PREV:fork/UPSTREAM" 2>/dev/null; then
  fail_pre "main ($PREV) carries no fork/UPSTREAM — it is not a generated ore main. Delete or repoint it before assembling."
fi

# The series must be lintable before we invest in a rebase.
"$FORK_DIR/lint-series.sh" --base "$BASE_REF" --head "$DELTA" || fail_pre "series lint failed on '$DELTA'"

[[ -n "$WORKTREE" ]] || WORKTREE="$(mktemp -d)/assembly"
[[ -e "$WORKTREE" && -n "$(ls -A "$WORKTREE" 2>/dev/null)" ]] \
  && fail_pre "worktree '$WORKTREE' exists and is not empty"
git rev-parse --verify --quiet "refs/heads/sync-delta/$TAG" >/dev/null \
  && fail_pre "branch sync-delta/$TAG already exists (delete it to re-run: git branch -D sync-delta/$TAG)"

OUT_DIR="$(dirname "$WORKTREE")/assembly-out"
mkdir -p "$OUT_DIR"
APPLY_LOG="$OUT_DIR/apply-log.md"
PASSES_LOG="$OUT_DIR/passes.md"
: >"$APPLY_LOG"; : >"$PASSES_LOG"

TOOLCHAIN=$(sed -n 's/^channel = "\(.*\)"$/\1/p' codex-rs/rust-toolchain.toml)
[[ -n "$TOOLCHAIN" ]] || fail_pre "cannot read the pinned toolchain from codex-rs/rust-toolchain.toml"

info "assemble: $BASE ($BASE_COMMIT) -> $TAG ($TAG_COMMIT)"
info "worktree: $WORKTREE  reports: $OUT_DIR"

# ------------------------------------------------------- 1. rerere restore

# The rr-cache lives in the COMMON git dir (shared across worktrees), and its
# authoritative copy is the orphan branch `rerere-cache` — durable, auditable
# (`git log rerere-cache` shows which sync learned which resolution), and never
# part of any assembled tree.
COMMON_DIR=$(git rev-parse --path-format=absolute --git-common-dir)
RR_CACHE="$COMMON_DIR/rr-cache"
restore_rerere() {
  local ref=""
  if git rev-parse --verify --quiet refs/heads/rerere-cache >/dev/null; then
    ref=refs/heads/rerere-cache
  elif git rev-parse --verify --quiet refs/remotes/origin/rerere-cache >/dev/null; then
    ref=refs/remotes/origin/rerere-cache
  fi
  [[ -n "$ref" ]] || { info "rerere: no cache branch yet (first sync learns from scratch)"; return 0; }
  mkdir -p "$RR_CACHE"
  git archive "$ref" | tar -x -C "$RR_CACHE"
  info "rerere: restored cache from $ref"
}
restore_rerere

# A learned resolution is worth keeping even if a later pass fails, so this is
# invoked from every exit path after the rebase.
snapshot_rerere() {
  [[ -d "$RR_CACHE" && -n "$(ls -A "$RR_CACHE" 2>/dev/null)" ]] || return 0
  local tmpidx tree parent prev_tree new
  # The scratch index path must NOT exist yet: git rejects an existing empty
  # file as a corrupt index.
  tmpidx="$(mktemp -d)/index"
  ( cd "$RR_CACHE" \
    && GIT_INDEX_FILE="$tmpidx" git --git-dir="$COMMON_DIR" --work-tree="$RR_CACHE" add -A ) || return 0
  tree=$(GIT_INDEX_FILE="$tmpidx" git --git-dir="$COMMON_DIR" write-tree)
  rm -rf "$(dirname "$tmpidx")"
  parent=$(git rev-parse --verify --quiet refs/heads/rerere-cache || true)
  if [[ -n "$parent" ]]; then
    prev_tree=$(git rev-parse "$parent^{tree}")
    [[ "$tree" == "$prev_tree" ]] && return 0
  fi
  new=$(GIT_AUTHOR_NAME="$BOT_NAME" GIT_AUTHOR_EMAIL="$BOT_EMAIL" \
        GIT_COMMITTER_NAME="$BOT_NAME" GIT_COMMITTER_EMAIL="$BOT_EMAIL" \
        git commit-tree "$tree" ${parent:+-p "$parent"} -m "rerere: after $BASE -> $TAG")
  git update-ref refs/heads/rerere-cache "$new"
  info "rerere: snapshot -> refs/heads/rerere-cache ($new)"
}

# ------------------------------------------------------- 2. rebase the series

git worktree add -b "sync-delta/$TAG" "$WORKTREE" "$DELTA" >/dev/null
wt() { git -C "$WORKTREE" "$@"; }

# Slugs before the rebase — compared after to surface commits --empty=drop
# removed (upstream absorbed them, or shipped a colliding different fix; the
# reviewer decides which via the Invariant trailer).
slugs_of() { git log --format='%(trailers:key=Fork-Patch,valueonly)' "$1..$2" | sed '/^$/d' | sort; }
SLUGS_BEFORE=$(slugs_of "$BASE_COMMIT" "$DELTA")

{
  echo "# apply-log: $BASE -> $TAG"
  echo
  echo '```'
} >>"$APPLY_LOG"

set +e
wt "${RERERE_CFG[@]}" rebase --onto "$TAG_COMMIT" "$BASE_COMMIT" \
  --empty=drop --no-fork-point >>"$APPLY_LOG" 2>&1
REBASE_RC=$?
set -e

# ----------------------------------------------------------- 2b. agent loop
# The agent is untrusted: it sees only the conflicted files, the wrapper does
# all staging and continuing, and any out-of-scope edit aborts the whole rebase
# (throwing away everything the agent wrote). Contract: $AGENT_CMD is run with
# cwd = worktree, the conflicted paths on stdin (one per line), and the stopped
# commit's full message (intent trailers included) in ORE_STOPPED_COMMIT_MSG.
AGENT_USED=0
agent_loop() {
  local stops=0 deadline=$(( $(date +%s) + AGENT_MAX_SECONDS ))
  local conflicted stopped pre post bad
  while [[ -d "$(wt rev-parse --git-path rebase-merge)" ]]; do
    conflicted=$(wt diff --name-only --diff-filter=U)
    if [[ -z "$conflicted" ]]; then
      # rerere.autoupdate already staged a fully-remembered resolution.
      GIT_EDITOR=true wt "${RERERE_CFG[@]}" rebase --continue >>"$APPLY_LOG" 2>&1 || return 1
      continue
    fi
    [[ -n "$AGENT_CMD" ]] || return 1
    AGENT_USED=1
    stops=$((stops + 1))
    if [[ "$stops" -gt "$AGENT_MAX_STOPS" || "$(date +%s)" -gt "$deadline" ]]; then
      echo "agent: bounds exceeded ($stops stops)" >>"$APPLY_LOG"
      wt rebase --abort
      return 2
    fi
    stopped=$(wt rev-parse REBASE_HEAD)
    echo "agent: stop $stops at $(wt log -1 --format='%h %s' "$stopped")" >>"$APPLY_LOG"
    pre=$(wt status --porcelain=v1 | sort)
    if ! ( cd "$WORKTREE" \
        && ORE_STOPPED_COMMIT_MSG="$(git log -1 --format=%B "$stopped")" \
           $AGENT_CMD <<<"$conflicted" ) >>"$APPLY_LOG" 2>&1; then
      echo "agent: command failed" >>"$APPLY_LOG"
      wt rebase --abort
      return 2
    fi
    post=$(wt status --porcelain=v1 | sort)
    # Enforcement: every path the agent changed must have been conflicted.
    bad=$(comm -13 <(printf '%s\n' "$pre") <(printf '%s\n' "$post") | cut -c4- \
          | grep -vxF -f <(printf '%s\n' "$conflicted") || true)
    if [[ -n "$bad" ]]; then
      printf 'agent: touched out-of-scope paths:\n%s\n' "$bad" >>"$APPLY_LOG"
      wt rebase --abort
      return 2
    fi
    if ( cd "$WORKTREE" && grep -l '^<<<<<<<' $conflicted 2>/dev/null | grep -q . ); then
      echo "agent: conflict markers remain" >>"$APPLY_LOG"
      wt rebase --abort
      return 2
    fi
    ( cd "$WORKTREE" && git add -- $conflicted )
    if [[ -n "$(wt "${RERERE_CFG[@]}" rerere remaining)" ]]; then
      echo "agent: rerere still reports unresolved paths" >>"$APPLY_LOG"
      wt rebase --abort
      return 2
    fi
    echo "agent: resolved $(wc -l <<<"$conflicted" | tr -d ' ') file(s), continuing" >>"$APPLY_LOG"
    GIT_EDITOR=true wt "${RERERE_CFG[@]}" rebase --continue >>"$APPLY_LOG" 2>&1 || return 1
  done
  return 0
}

AGENT_RESOLVED=0
if [[ "$REBASE_RC" -ne 0 ]]; then
  set +e
  agent_loop
  loop_rc=$?
  set -e
  echo '```' >>"$APPLY_LOG"
  snapshot_rerere
  case "$loop_rc" in
    0) AGENT_RESOLVED=$AGENT_USED ;;  # rerere-only continuations are not agent work
    2) fail_conflict "agent violated its contract or exceeded bounds; rebase aborted (see $APPLY_LOG)" ;;
    *) fail_conflict "rebase stopped on conflicts; resolve in $WORKTREE then re-run, or pass --agent (see $APPLY_LOG)" ;;
  esac
else
  echo '```' >>"$APPLY_LOG"
fi

SERIES_HEAD=$(wt rev-parse HEAD)
SLUGS_AFTER=$(slugs_of "$TAG_COMMIT" "$SERIES_HEAD")
DROPPED=$(comm -23 <(printf '%s\n' "$SLUGS_BEFORE") <(printf '%s\n' "$SLUGS_AFTER") | sed '/^$/d' || true)
{
  echo
  echo "## series: $(git rev-list --count "$TAG_COMMIT..$SERIES_HEAD") commit(s) on $TAG"
  git log --reverse --format='- %h %s' "$TAG_COMMIT..$SERIES_HEAD"
  if [[ -n "$DROPPED" ]]; then
    echo
    echo "## DROPPED (empty after rebase) — label PR series-retired, reviewer confirms the Invariant still holds:"
    printf '%s\n' "$DROPPED" | sed 's/^/- /'
  fi
  [[ "$AGENT_RESOLVED" -eq 1 ]] && { echo; echo "## conflicts were agent-resolved — label PR agent-resolved"; }
  echo
  echo "## range-diff vs previous series"
  echo '```'
  git range-diff "$BASE_COMMIT..$DELTA" "$TAG_COMMIT..$SERIES_HEAD" 2>&1 | head -200
  echo '```'
} >>"$APPLY_LOG"
# Review convenience only — the branch is the source of truth, never patch files.
git format-patch --stdout "$TAG_COMMIT..$SERIES_HEAD" >"$OUT_DIR/series.patch"

# ------------------------------------------------------- 3. generated passes

# Each pass opens a header, appends its own stdout under it, and closes with
# the tree delta it produced (staged snapshots compared, so untracked and
# renamed files are included and passes don't blur together).
PREV_TREE=$(wt rev-parse 'HEAD^{tree}')
begin_pass() { echo "### $1" >>"$PASSES_LOG"; }
end_pass() {
  local tree
  wt add -A
  tree=$(wt write-tree)
  {
    if [[ "$tree" == "$PREV_TREE" ]]; then
      echo "(no tree changes)"
    else
      git diff --stat "$PREV_TREE" "$tree" | tail -100
    fi
    echo
  } >>"$PASSES_LOG"
  PREV_TREE="$tree"
}

# --- 3a. workflow relocation (deterministic rebuild; deny-by-default) --------
# Run the WORKTREE's own copy: the tool version that ships in the tree it
# produces, so a --check re-run inside that tree is self-contained.
begin_pass "relocate workflows"
python3 "$WORKTREE/fork/relocate_workflows.py" --root "$WORKTREE" \
  --allow "$WORKTREE/fork/workflows.allow" >>"$PASSES_LOG" 2>&1 \
  || fail_pass "workflow relocation (see $PASSES_LOG)"
end_pass

# --- 3b. substitutions: apply, then prove idempotence, then leak-scan --------
begin_pass "substitutions"
python3 "$WORKTREE/fork/substitute.py" --root "$WORKTREE" \
  --manifest "$WORKTREE/fork/substitutions.yaml" --apply >>"$PASSES_LOG" 2>&1 \
  || fail_pass "substitute --apply (stale rules? see $PASSES_LOG)"
python3 "$WORKTREE/fork/substitute.py" --root "$WORKTREE" \
  --manifest "$WORKTREE/fork/substitutions.yaml" --check >>"$PASSES_LOG" 2>&1 \
  || fail_pass "substitute --check: applying twice would still change the tree"
python3 "$WORKTREE/fork/substitute.py" --root "$WORKTREE" \
  --manifest "$WORKTREE/fork/substitutions.yaml" --audit >>"$PASSES_LOG" 2>&1 \
  || fail_pass "substitute --audit: brand leaks outside the allowlist"
end_pass

# --- 3c. cargo fmt -----------------------------------------------------------
# Literal-length changes can reflow lines; running fmt here keeps the assembled
# main clean under upstream's own `just fmt-check` so ore-ci keeps that check.
# rustfmt is pinned by rust-toolchain.toml, so the output is deterministic.
begin_pass "cargo fmt"
fmt_out=$(mktemp)
( cd "$WORKTREE/codex-rs" && cargo "+$TOOLCHAIN" fmt -- --config imports_granularity=Item ) \
  >"$fmt_out" 2>&1 || { cat "$fmt_out" >>"$PASSES_LOG"; fail_pass "cargo fmt"; }
# rustfmt.toml itself documents this stable-channel warning as ignorable.
grep -v "can't set \`imports_granularity = Item\`" "$fmt_out" >>"$PASSES_LOG" || true
rm -f "$fmt_out"
end_pass

# --- 3c2. ruff format --------------------------------------------------------
# The same reasoning as cargo fmt, for the other language the substitutions
# reach. This was missing, and the first ore-ci run on a generated main caught
# it: rewriting URLs and names in scripts/**.py reflows lines, and three files
# landed on main that `just fmt-check` then rejected.
#
# Scoped to scripts/ because that is the only Python the manifest touches --
# sdk/** is in the manifest's skip_paths -- and it uses the exact invocation
# scripts/format.py uses for that group, so the two cannot drift apart.
begin_pass "ruff format"
command -v uv >/dev/null \
  || fail_pass "ruff format: uv is not installed, so the assembled tree cannot be proven fmt-clean"
ruff_out=$(mktemp)
( cd "$WORKTREE" && uv run --frozen --project scripts ruff format scripts ) \
  >"$ruff_out" 2>&1 || { cat "$ruff_out" >>"$PASSES_LOG"; rm -f "$ruff_out"; fail_pass "ruff format"; }
cat "$ruff_out" >>"$PASSES_LOG"
rm -f "$ruff_out"
end_pass

# --- 3d. scheme-C version + lock regeneration --------------------------------
# fork/VERSION is the single source: 1.{upstream_minor}.{ore_patch}. On a new
# base the minor auto-derives and the patch resets; respins bump the patch by
# hand (a delta edit + --reassemble). The workspace version is written from it
# because CARGO_PKG_VERSION is wire-visible and `ore --version` line 1 must be
# exactly `ore <CARGO_PKG_VERSION>`.
begin_pass "version + lock"
UPSTREAM_MINOR=$(sed -E 's/^rust-v[0-9]+\.([0-9]+)\.[0-9]+$/\1/' <<<"$TAG")
FORK_VERSION=$(tr -d '[:space:]' <"$WORKTREE/fork/VERSION")
FORK_MINOR=$(cut -d. -f2 <<<"$FORK_VERSION")
if [[ "$FORK_MINOR" != "$UPSTREAM_MINOR" ]]; then
  FORK_VERSION="1.$UPSTREAM_MINOR.0"
  printf '%s\n' "$FORK_VERSION" >"$WORKTREE/fork/VERSION"
  info "version: new base minor -> fork/VERSION = $FORK_VERSION"
fi
awk -v ver="$FORK_VERSION" '
  /^\[workspace\.package\]$/ { in_wp = 1; print; next }
  in_wp && /^version = /     { print "version = \"" ver "\""; in_wp = 0; next }
  /^\[/                      { in_wp = 0 }
  { print }
' "$WORKTREE/codex-rs/Cargo.toml" >"$WORKTREE/codex-rs/Cargo.toml.new"
mv "$WORKTREE/codex-rs/Cargo.toml.new" "$WORKTREE/codex-rs/Cargo.toml"
grep -q "^version = \"$FORK_VERSION\"$" "$WORKTREE/codex-rs/Cargo.toml" \
  || fail_pass "workspace version write: [workspace.package] version != $FORK_VERSION"

# Series commits never touch Cargo.lock (lint-enforced), so the tree still
# holds the tag's lock byte-identical and no lock conflict can exist. `update
# --workspace` refreshes member entries and adds fork deps without moving
# upstream's third-party pins; a full re-resolve (generate-lockfile) is
# explicitly rejected. MODULE.bazel.lock / pnpm-lock.yaml stay upstream's.
( cd "$WORKTREE/codex-rs" && cargo "+$TOOLCHAIN" update --workspace ) \
  >>"$PASSES_LOG" 2>&1 || fail_pass "cargo update --workspace"
if [[ "$SKIP_HEAVY" -eq 0 ]]; then
  ( cd "$WORKTREE/codex-rs" && cargo "+$TOOLCHAIN" metadata --locked --format-version 1 >/dev/null ) \
    || fail_pass "cargo metadata --locked: regenerated lock is incomplete"
fi
echo "fork/VERSION = $FORK_VERSION" >>"$PASSES_LOG"
end_pass

# --- 3e. schema regen [heavy] ------------------------------------------------
# Exact-bytes fixture tests exist for all three; regenerate unconditionally
# (byte-identical output when nothing changed). NOTE: `just
# write-app-server-schema` is stale — the python driver is the real entry.
if [[ "$SKIP_HEAVY" -eq 0 ]]; then
  begin_pass "schema regen"
  ( cd "$WORKTREE/codex-rs" && cargo "+$TOOLCHAIN" run -p codex-core --bin codex-write-config-schema ) \
    >>"$PASSES_LOG" 2>&1 || fail_pass "config schema regen"
  python3 "$WORKTREE/codex-rs/app-server-protocol/scripts/write_schema_fixtures.py" \
    >>"$PASSES_LOG" 2>&1 || fail_pass "app-server schema fixtures"
  python3 "$WORKTREE/codex-rs/app-server-protocol/scripts/write_schema_fixtures.py" --experimental \
    >>"$PASSES_LOG" 2>&1 || fail_pass "app-server schema fixtures (experimental)"
  ( cd "$WORKTREE/codex-rs" && cargo "+$TOOLCHAIN" run -p codex-hooks --bin write_hooks_schema_fixtures ) \
    >>"$PASSES_LOG" 2>&1 || fail_pass "hooks schema regen"
  end_pass
else
  echo "### schema regen: SKIPPED (--skip-heavy)" >>"$PASSES_LOG"
fi

# --- 3f. snapshot regen [heavy] ----------------------------------------------
# .snap files are excluded from substitution (padding in rendered TUI boxes is
# width-dependent — a substituted snap no longer matches a re-render); they are
# regenerated instead, and verify re-runs the same suites with INSTA_UPDATE=no.
if [[ "$SKIP_HEAVY" -eq 0 ]]; then
  begin_pass "snapshot regen"
  cargo nextest --version >/dev/null 2>&1 || fail_pass "cargo-nextest is not installed"
  # Regenerate, then report -- do not gate. This pass exists to rewrite .snap
  # files under INSTA_UPDATE=always, where snapshot assertions always succeed;
  # everything else it observes is the test suite, which is ore-ci's job to judge
  # and which it judges through the two filtersets.
  #
  # It used to fail the assembly on any red test. That premise cannot hold: the
  # upstream suite does not pass at upstream's own release tags, and it fails
  # differently on every machine -- so gating here made a complete assembly
  # impossible rather than merely noisy. A build failure is still fatal, because
  # then nothing was regenerated at all; nextest signals that with an exit code
  # other than 100.
  regen_filter=$(cat "$WORKTREE/fork/verify/known-failing-upstream" "$WORKTREE/fork/verify/known-failing" 2>/dev/null \
    | grep -v '^[[:space:]]*#' | grep -v '^[[:space:]]*$' | paste -sd'|' - | sed 's/|/ or /g')
  regen_args=(--no-fail-fast -p codex-tui -p codex-core -p codex-cli)
  [[ -n "$regen_filter" ]] && regen_args+=(-E "not ($regen_filter)")
  regen_rc=0
  # Raw nextest output goes to its own file. It is megabytes for a full run, and
  # the passes log becomes the assembly commit message.
  regen_log="$OUT_DIR/snapshot-regen.log"
  ( cd "$WORKTREE/codex-rs" && INSTA_UPDATE=always RUST_MIN_STACK=8388608 \
      cargo "+$TOOLCHAIN" nextest run "${regen_args[@]}" ) \
    >"$regen_log" 2>&1 || regen_rc=$?
  grep -E '^ +Summary ' "$regen_log" | tail -1 >>"$PASSES_LOG" || true
  echo "(full output: $(basename "$regen_log"))" >>"$PASSES_LOG"
  if [[ "$regen_rc" -ne 0 && "$regen_rc" -ne 100 ]]; then
    fail_pass "snapshot regen: the suite did not build (exit $regen_rc), so nothing was regenerated"
  fi
  if [[ "$regen_rc" -eq 100 ]]; then
    echo "### snapshot regen: snapshots rewritten; tests were red — ore-ci judges those" >>"$PASSES_LOG"
    warn "snapshot regen: tests failed during regeneration; ore-ci is the gate for that"
  fi
  end_pass
else
  echo "### snapshot regen: SKIPPED (--skip-heavy)" >>"$PASSES_LOG"
fi

# --- 3g. fork/UPSTREAM -------------------------------------------------------
begin_pass "fork/UPSTREAM"
TAG_OBJECT=$(git rev-parse "$TAG_REF")
TAG_DATE=$(git for-each-ref --format='%(taggerdate:iso-strict)' "$TAG_REF")
cat >"$WORKTREE/fork/UPSTREAM" <<EOF
# GENERATED by fork/assemble.sh — authoritative on 'main'; a tracked placeholder on 'delta'.
# On delta this may lag the true base between syncs; assemble rewrites it.
tag = "$TAG"
commit = "$TAG_COMMIT"
tag_object = "$TAG_OBJECT"
tag_date = "$TAG_DATE"
assembled_at = "${ORE_ASSEMBLED_AT:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
series_head = "$SERIES_HEAD"
EOF
end_pass

# ------------------------------------------------- 3h. one assembly commit

wt add -A
if wt diff --cached --quiet HEAD; then
  info "generated passes produced no changes (nothing to commit)"
else
  # -F, not -m "$(cat ...)": the passes log is as large as the passes make it,
  # and a full snapshot regen pushed it past ARG_MAX. A file has no such limit.
  { echo "assembly: generated passes for $TAG"; echo; cat "$PASSES_LOG"; } >"$OUT_DIR/assembly-msg.txt"
  # gpgsign=false deliberately. This commit is machine-generated and authored by
  # the bot, not by whoever ran the assembly, so signing it with the operator's
  # key would attest to the wrong thing. It also made assembly fail outright when
  # a keychain locked mid-run, and would fail in CI, where there is no key at all.
  # The signature that matters is on the delta series, which humans write.
  wt -c "user.name=$BOT_NAME" -c "user.email=$BOT_EMAIL" -c commit.gpgsign=false \
    commit --quiet -F "$OUT_DIR/assembly-msg.txt"
fi

# ------------------------------------------- 4. merge candidate (commit-tree)

TREE=$(wt rev-parse 'HEAD^{tree}')

PARENTS=()
if [[ "$REASSEMBLE" -eq 1 ]]; then
  # The tag is already an ancestor of the previous merge; repeating it as a
  # parent would be noise.
  PARENTS=(-p "$PREV")
  SUBJECT="reassemble: $TAG (delta @ $(git rev-parse --short "$DELTA"))"
elif [[ -n "$PREV" ]]; then
  PARENTS=(-p "$PREV" -p "$TAG_COMMIT")
  SUBJECT="sync: $BASE -> $TAG"
else
  # Bootstrap: the very first generated main has only the tag as parent.
  PARENTS=(-p "$TAG_COMMIT")
  SUBJECT="sync: bootstrap $TAG"
fi

M=$(GIT_AUTHOR_NAME="$BOT_NAME" GIT_AUTHOR_EMAIL="$BOT_EMAIL" \
    GIT_COMMITTER_NAME="$BOT_NAME" GIT_COMMITTER_EMAIL="$BOT_EMAIL" \
    git commit-tree "$TREE" "${PARENTS[@]}" -F - <<EOF
$SUBJECT

Upstream: $TAG ($TAG_COMMIT)
Series: $SERIES_HEAD ($(git rev-list --count "$TAG_COMMIT..$SERIES_HEAD") commits)
$(git log --reverse --format='  %h %(trailers:key=Fork-Patch,valueonly,separator=) %s' "$TAG_COMMIT..$SERIES_HEAD")

Apply-log, passes report and semantic review: see the sync PR / run artifacts.
EOF
)

old=$(git rev-parse --verify --quiet "refs/heads/sync/$TAG" || true)
[[ -n "$old" ]] && warn "refs/heads/sync/$TAG existed ($old); overwriting"
git update-ref "refs/heads/sync/$TAG" "$M"

# ------------------------------------------------------- 5. rerere snapshot

snapshot_rerere

# Release the assembly worktree on success. The assembled tree is reachable as
# refs/heads/sync/$TAG and the reports live outside it, so nothing is lost --
# but a worktree left registered holds sync-delta/$TAG checked out, and then the
# "delete it to re-run" remedy this script prints for the next invocation cannot
# actually be carried out. CI runners are ephemeral and never noticed; a local
# reassemble hit it on the second run. Failures still keep their worktree: that
# is where a human resolves the conflict.
if [[ -z "${WORKTREE_KEEP:-}" ]]; then
  git worktree remove --force "$WORKTREE" 2>/dev/null \
    || warn "could not remove the assembly worktree at $WORKTREE; remove it before the next run"
fi

info "done: refs/heads/sync/$TAG = $M"
info "  tree     $TREE"
info "  series   sync-delta/$TAG = $SERIES_HEAD"
info "  reports  $OUT_DIR"
info "push (CI only): safe_push refs/heads/sync/$TAG refs/heads/sync-delta/$TAG refs/heads/rerere-cache"
exit 0
