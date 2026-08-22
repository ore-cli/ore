#!/usr/bin/env bash
#
# fork/agent-resolve.sh — safety wrapper around an UNTRUSTED conflict agent.
#
#   agent-resolve.sh [--prompt <file>]          one rebase stop (the assemble.sh --agent contract)
#   agent-resolve.sh --loop [--prompt <file>]   drive an already-stopped rebase to the end
#
# Per-stop contract (what `assemble.sh --agent fork/agent-resolve.sh` provides):
# cwd = the rebase worktree root, the conflicted paths on stdin (one per line),
# the stopped commit's full message in ORE_STOPPED_COMMIT_MSG. On success the
# resolved files are staged and this script exits 0; the CALLER continues the
# rebase and owns the stop/time bounds. Any non-zero exit makes assemble.sh
# abort the rebase and exit 2, which is what the sync workflow turns into a
# sync-blocked issue. --loop exists for driving a stopped rebase by hand: the
# same guards per stop, plus staging, `rebase --continue`, skip-when-absorbed,
# and its own hard bounds; there, any rejection aborts the rebase itself.
#
# The agent command comes from $ORE_AGENT_CMD, else $AGENT_CMD (the repository
# variable of that name, exported by the sync workflow). There is NO default:
# unset means fail, never a guessed agent. ORE_AGENT_CMD exists because
# assemble.sh keeps its --agent value in a shell variable also named AGENT_CMD;
# if the caller exported AGENT_CMD, assemble's assignment would overwrite the
# exported value with THIS script's own path before it reaches us.
#
# Containment: the agent never sees the repository. It runs in a throwaway
# sandbox directory holding only PROMPT.md (the conflict prompt + the stopped
# commit's intent trailers + the file list, also fed on stdin) and copies of
# the conflicted files; its environment points GIT_DIR/GIT_WORK_TREE/
# GIT_INDEX_FILE at a nonexistent path and fences discovery with
# GIT_CEILING_DIRECTORIES, so git run inside the sandbox finds no repository
# to read, mutate, or push from. Nothing is copied back to the worktree until
# every guard passes; a rejected stop discards the agent's work entirely.
#
# Guards — each one REJECTS the stop (exit 2):
#   G1  the agent exits non-zero (its documented way to say "the invariant
#       cannot be preserved") or exceeds ORE_AGENT_STOP_SECONDS
#   G2  the worktree changed at all while the agent ran (status snapshot, HEAD
#       and rebase state compared) — covers out-of-scope edits AND created
#       untracked files, because only this wrapper writes to the worktree,
#       and only ever the conflicted paths
#   G3  a conflicted path is missing from the sandbox afterwards, or is not a
#       regular file (symlink substitution would smuggle foreign content in)
#   G4  conflict markers remain in any conflicted file
#   G5  fork/** or .github/workflows/** is conflicted — policy surfaces are
#       never auto-resolved; checked before the agent even runs
#   G6  `git rerere remaining` still lists paths after staging
#
# Honest limits: cwd confinement is not an OS sandbox — a hostile agent binary
# can still write absolute paths; G2 catches worktree damage after the fact
# and the whole rebase is then abandoned, and pushing is stopped by
# safe_push()'s allowlist and the server-side tag ruleset, not here.
#
# Environment: ORE_AGENT_CMD / AGENT_CMD (required, see above),
# ORE_AGENT_STOP_SECONDS (per-stop wall clock, default 900),
# ORE_AGENT_MAX_STOPS / ORE_AGENT_MAX_SECONDS (--loop bounds, default 25/3600
# to match assemble.sh).
#
# Exit codes: 0 resolved · 2 stop rejected or bounds exceeded (--loop aborts
# the rebase; per-stop mode leaves the abort to the caller, which assemble.sh
# performs on any non-zero exit) · 4 precondition failure.

set -euo pipefail

FORK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$FORK_DIR/lib.sh"

RERERE_CFG=(-c rerere.enabled=true -c rerere.autoupdate=true -c gc.auto=0)
STOP_SECONDS="${ORE_AGENT_STOP_SECONDS:-900}"
MAX_STOPS="${ORE_AGENT_MAX_STOPS:-25}"
MAX_SECONDS="${ORE_AGENT_MAX_SECONDS:-3600}"

usage() { sed -n '2,7p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

MODE=stop PROMPT_FILE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --loop)    MODE=loop; shift ;;
    --prompt)  PROMPT_FILE="${2:?--prompt needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *)         usage >&2; printf 'agent-resolve: unknown argument %s\n' "$1" >&2; exit 4 ;;
  esac
done

fail_pre() { printf 'agent-resolve: precondition: %s\n' "$*" >&2; exit 4; }

reject() {
  printf 'agent-resolve: REJECTED: %s\n' "$*" >&2
  if [[ "$MODE" == loop ]]; then
    warn "aborting the rebase — a rejected stop is never continued past"
    git rebase --abort || warn "rebase --abort failed; clean the worktree up by hand"
  fi
  exit 2
}

# ------------------------------------------------------------- preconditions

AGENT="${ORE_AGENT_CMD:-${AGENT_CMD:-}}"
[[ -n "$AGENT" ]] || fail_pre "no agent command: set ORE_AGENT_CMD (or the AGENT_CMD repo variable); this wrapper never invents one"
# Recursion guard: assemble's --agent is this script; the env var must be the
# actual agent, or every stop would re-enter here forever.
for word in $(printf '%s\n' "$AGENT" | awk '{ print $1; if ($2) print $2 }'); do
  if [[ "$(basename "$word")" == "agent-resolve.sh" ]]; then
    fail_pre "ORE_AGENT_CMD/AGENT_CMD points back at this wrapper; it must name the real agent command"
  fi
done
# The agent runs with cwd = its sandbox, so a relative agent path would resolve
# against the sandbox; pin it to the invocation directory. Bare commands (no
# slash) still go through PATH untouched.
AGENT_FIRST="${AGENT%%[[:space:]]*}"
if [[ "$AGENT_FIRST" == */* && "$AGENT_FIRST" != /* ]]; then
  AGENT="$PWD/$AGENT_FIRST${AGENT#"$AGENT_FIRST"}"
fi

git rev-parse --is-inside-work-tree >/dev/null 2>&1 || fail_pre "not inside a git worktree"
[[ -z "$(git rev-parse --show-prefix)" ]] || fail_pre "run from the worktree root (conflicted paths are root-relative)"
REBASE_DIR="$(git rev-parse --git-path rebase-merge)"
[[ -d "$REBASE_DIR" ]] || fail_pre "no rebase in progress in $PWD"

[[ -n "$PROMPT_FILE" ]] || PROMPT_FILE="$FORK_DIR/agent-conflict-prompt.md"
[[ -r "$PROMPT_FILE" ]] || fail_pre "prompt file '$PROMPT_FILE' is not readable"

SCRATCH="$(mktemp -d)"
AGENT_PID=""
# shellcheck disable=SC2329  # invoked via the EXIT trap
cleanup() {
  if [[ -n "$AGENT_PID" ]]; then kill -KILL "$AGENT_PID" 2>/dev/null || true; fi
  rm -rf "$SCRATCH"
}
trap cleanup EXIT

csort() { LC_ALL=C sort; }
current_conflicts() { git diff --name-only --diff-filter=U | csort; }

# --------------------------------------------------------------- one stop

CONFLICT_ARR=()
load_conflicts() {  # $1 = newline-separated paths -> CONFLICT_ARR
  CONFLICT_ARR=()
  local p
  while IFS= read -r p; do
    if [[ -n "$p" ]]; then CONFLICT_ARR+=("$p"); fi
  done <<<"$1"
}

policy_check() {  # G5 + path sanity, before the agent sees anything
  local p
  for p in ${CONFLICT_ARR[@]+"${CONFLICT_ARR[@]}"}; do
    if [[ "$p" == /* ]] || [[ "$p" =~ (^|/)\.\.(/|$) ]]; then
      fail_pre "conflict path escapes the worktree: '$p'"
    fi
    case "$p" in
      fork/*|.github/workflows/*)
        reject "'$p' is conflicted — fork/ and workflow files are never auto-resolved; a human owns this stop" ;;
    esac
  done
}

stop_message() {
  # assemble.sh exports the stopped commit's message; --loop reads it per stop.
  if [[ "$MODE" == stop && -n "${ORE_STOPPED_COMMIT_MSG:-}" ]]; then
    printf '%s\n' "$ORE_STOPPED_COMMIT_MSG"
  else
    git log -1 --format=%B REBASE_HEAD
  fi
}

SANDBOX="" AGENT_OUT="" AGENT_RC=0
build_sandbox() {  # $1 = per-stop ordinal (unique dir per stop in --loop)
  SANDBOX="$SCRATCH/stop-$1"
  AGENT_OUT="$SCRATCH/agent-$1.log"
  mkdir -p "$SANDBOX"
  local p msg subject trailers
  for p in "${CONFLICT_ARR[@]}"; do
    if [[ ! -f "./$p" || -L "./$p" ]]; then
      reject "cannot auto-resolve '$p': not a regular file in the worktree (delete/rename conflicts need a human)"
    fi
    mkdir -p "$SANDBOX/$(dirname "$p")"
    cp "./$p" "$SANDBOX/$p"
  done
  msg="$(stop_message)"
  subject="$(head -n 1 <<<"$msg")"
  trailers="$(printf '%s\n' "$msg" | git interpret-trailers --parse)"
  {
    cat "$PROMPT_FILE"
    printf -- '\n---\n\n## The stopped commit\n\n    %s\n\n' "$subject"
    if [[ -n "$trailers" ]]; then
      printf '%s\n' "$trailers" | sed 's/^/    /'
    else
      printf '    (no intent trailers found on this commit)\n'
    fi
    printf '\n## Conflicted files (the only files you may edit)\n\n'
    printf -- '- %s\n' "${CONFLICT_ARR[@]}"
  } >"$SANDBOX/PROMPT.md"
}

run_agent() {  # $1 = budget in seconds; result in AGENT_RC (124 = timed out)
  local budget="$1" waited=0
  ( cd "$SANDBOX" \
    && GIT_DIR="$SCRATCH/no-repo" GIT_WORK_TREE="$SCRATCH/no-repo" \
       GIT_INDEX_FILE="$SCRATCH/no-repo" GIT_CEILING_DIRECTORIES="$SCRATCH" \
       GIT_TERMINAL_PROMPT=0 \
       exec $AGENT <PROMPT.md ) >"$AGENT_OUT" 2>&1 &
  AGENT_PID=$!
  while kill -0 "$AGENT_PID" 2>/dev/null; do
    if [[ "$waited" -ge "$budget" ]]; then
      kill -TERM "$AGENT_PID" 2>/dev/null || true
      sleep 5
      kill -KILL "$AGENT_PID" 2>/dev/null || true
      wait "$AGENT_PID" 2>/dev/null || true
      AGENT_PID=""
      AGENT_RC=124
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  AGENT_RC=0
  wait "$AGENT_PID" || AGENT_RC=$?
  AGENT_PID=""
}

show_agent_tail() {
  if [[ -s "$AGENT_OUT" ]]; then
    echo "agent-resolve: agent output (untrusted; last 20 lines):"
    tail -n 20 "$AGENT_OUT" | sed 's/^/  | /'
  fi
}

resolve_stop() {  # $1 = ordinal, $2 = budget seconds
  local p f extra remaining pre_status pre_head
  policy_check
  build_sandbox "$1"
  pre_status="$(git status --porcelain=v1 | csort)"
  pre_head="$(git rev-parse HEAD)"

  info "stop $1: $(git log -1 --format='%h %s' REBASE_HEAD) — ${#CONFLICT_ARR[@]} conflicted file(s), ${2}s budget"
  run_agent "$2"
  show_agent_tail

  # G1
  if [[ "$AGENT_RC" -eq 124 ]]; then reject "agent exceeded its ${2}s budget for this stop"; fi
  if [[ "$AGENT_RC" -ne 0 ]]; then reject "agent exited $AGENT_RC (declined or failed; see its output above)"; fi
  # G2 — the wrapper is the only writer; ANY worktree drift means the agent
  # escaped the sandbox, and none of its work can be trusted.
  if [[ ! -d "$REBASE_DIR" ]]; then reject "the rebase state vanished while the agent ran"; fi
  if [[ "$(git rev-parse HEAD)" != "$pre_head" ]]; then reject "HEAD moved while the agent ran"; fi
  if [[ "$(git status --porcelain=v1 | csort)" != "$pre_status" ]]; then
    reject "the worktree changed while the agent ran (out-of-scope edits or new files)"
  fi
  # G3 + G4, checked in the sandbox so nothing tainted ever reaches the tree
  for p in "${CONFLICT_ARR[@]}"; do
    f="$SANDBOX/$p"
    if [[ ! -f "$f" || -L "$f" ]]; then reject "agent removed or replaced '$p' with a non-regular file"; fi
    if grep -Eq '^(<{7}|\|{7}|>{7})( |$)|^={7}$' "$f"; then reject "conflict markers remain in '$p'"; fi
  done
  # Scratch files the agent left beside its copies never leave the sandbox.
  extra="$(cd "$SANDBOX" && find . -type f ! -name PROMPT.md | sed 's|^\./||' \
           | grep -vxF -f <(printf '%s\n' "${CONFLICT_ARR[@]}") || true)"
  if [[ -n "$extra" ]]; then info "ignoring sandbox scratch file(s): $(tr '\n' ' ' <<<"$extra")"; fi

  for p in "${CONFLICT_ARR[@]}"; do
    cp "$SANDBOX/$p" "./$p"
  done
  git add -- "${CONFLICT_ARR[@]}"
  # G6
  remaining="$(git "${RERERE_CFG[@]}" rerere remaining)"
  if [[ -n "$remaining" ]]; then reject "paths still unresolved after staging: $(tr '\n' ' ' <<<"$remaining")"; fi
  info "stop $1: ${#CONFLICT_ARR[@]} file(s) resolved and staged"
}

# ------------------------------------------------------------------ modes

if [[ "$MODE" == stop ]]; then
  GIVEN=""
  if [[ ! -t 0 ]]; then GIVEN="$(cat)"; fi
  ACTUAL="$(current_conflicts)"
  [[ -n "$ACTUAL" ]] || fail_pre "no unmerged paths at this rebase stop"
  if [[ -n "$GIVEN" ]]; then
    # stdin is convenience, git is authority: a caller-supplied list may never
    # widen or narrow the scope the guards enforce.
    if [[ "$(printf '%s\n' "$GIVEN" | sed '/^$/d' | csort)" != "$ACTUAL" ]]; then
      fail_pre "conflict list on stdin does not match git's unmerged set"
    fi
  fi
  load_conflicts "$ACTUAL"
  resolve_stop 1 "$STOP_SECONDS"
  exit 0
fi

# --loop: same guards per stop, plus continuation and hard bounds.
advance() {
  # One rebase --continue (or --skip when the resolution emptied the commit —
  # upstream absorbed it). A later pick stopping on conflicts also makes
  # --continue exit non-zero; the while loop re-inspects state, so that is not
  # a failure here.
  local rc=0 out="$SCRATCH/continue.log"
  GIT_EDITOR=true git "${RERERE_CFG[@]}" rebase --continue >"$out" 2>&1 || rc=$?
  if [[ "$rc" -ne 0 && -d "$(git rev-parse --git-path rebase-merge)" && -z "$(current_conflicts)" ]]; then
    if git diff --cached --quiet HEAD 2>/dev/null; then
      info "commit became empty (absorbed upstream); skipping"
      git rebase --skip >"$out" 2>&1 || true
    else
      sed 's/^/  /' "$out" >&2
      reject "rebase --continue failed"
    fi
  fi
}

STARTED="$(date +%s)"
DEADLINE=$((STARTED + MAX_SECONDS))
STOPS=0
while [[ -d "$(git rev-parse --git-path rebase-merge)" ]]; do
  CONFLICTS="$(current_conflicts)"
  if [[ -z "$CONFLICTS" ]]; then
    # rerere.autoupdate already staged a fully-remembered resolution.
    advance
    continue
  fi
  STOPS=$((STOPS + 1))
  NOW="$(date +%s)"
  if [[ "$STOPS" -gt "$MAX_STOPS" || "$NOW" -ge "$DEADLINE" ]]; then
    reject "bounds exceeded ($STOPS stops / $((NOW - STARTED))s elapsed; limits $MAX_STOPS/$MAX_SECONDS)"
  fi
  BUDGET=$((DEADLINE - NOW))
  if [[ "$BUDGET" -gt "$STOP_SECONDS" ]]; then BUDGET="$STOP_SECONDS"; fi
  load_conflicts "$CONFLICTS"
  resolve_stop "$STOPS" "$BUDGET"
  advance
done
info "rebase complete after $STOPS agent stop(s)"
exit 0
