# Working in ore

ore is a rebranded, telemetry-free fork of openai/codex that tracks upstream stable
tags forever. The design objective that outranks every other is **minimize the
upstream diff surface**. Almost everything below follows from that.

## The two branches are not what they look like

- **`delta`** is the source of truth: an ordered series of single-purpose commits
  sitting on a pinned upstream tag. This is where you work.
- **`main` is GENERATED.** It is built by `fork/assemble.sh` from `delta` plus the
  generated passes, and appended with `git commit-tree`. **Never commit to `main`,
  never merge into it, and never use the GitHub merge button on a sync PR** — a
  fresh merge commit has different parents and breaks main's append-only spine.

`delta` is the *un-substituted* series. It must never contain rebranded text,
relocated workflows, or a regenerated lock — that content is main's, produced at
assemble time. A substituted `delta` makes the next assemble apply rebrand rules
to already-rebranded text and the run fails.

## Every commit on delta needs trailers

`fork/lint-series.sh` enforces them and CI runs it:

```
Fork-Patch: <stable-slug>     # identity across rebases; a dropped slug is how an
Purpose: <one line>           #   upstream-absorbed commit gets noticed
Invariant: <what must hold>
Verify: <how to check it>
Conflict-notes: <optional>
```

Keep the trailer block contiguous — a blank line before `Co-Authored-By:` splits it
and the trailers stop being seen.

## Never commit these

- **`codex-rs/Cargo.lock`** — regenerated during assembly; committing it guarantees
  rebase conflicts. It is in `lint-series.sh`'s forbidden set. Revert it before
  every commit (`git checkout -- codex-rs/Cargo.lock`).
- **`fork/upstream-workflows/`** — exists only on generated main.
- **Schema fixtures without their generator inputs** — generated output; committing
  them alone means inputs and outputs diverged.

## Rebranding is a substitution, not an edit

Do **not** hand-edit an upstream file to change "Codex" to "Ore". Add a rule to
`fork/substitutions.yaml`; it is applied to upstream files at assemble time, which
is what keeps those files free of fork commits. A rule that matches nothing fails
the stale-rule check, so rules must stay anchored to the text they target.

Because substitutions run at assemble time and ore-ci tests `delta` unsubstituted,
**a rebrand that breaks an upstream test is invisible until a sync PR builds a
candidate.** If an upstream test asserts a string you rebranded, substitute the
test's copy too rather than retiring the test.

## The auth fence

`codex-rs/login/`, `model-provider-info/src/lib.rs`, `app-server-protocol` and
friends are held byte-identical to upstream except for narrowly sanctioned edits,
checked by `fork/verify/auth_fence.py` against `fork/verify/allowed-fence.diff`.
Do not rebrand the wire originator (`codex_cli_rs`), the `CODEX_INTERNAL_*` env var
names, or anything else `fork/verify/strings.toml` protects — the backend keys off
them.

## Pushing

Every push goes through `safe_push()` in `fork/lib.sh`, whose allowlist is
`main`, `delta`, `sync/*`, `sync-delta/*`, `rerere-cache`, and `ore-*` tags.
**Never push an upstream-named tag** (`rust-v*`) — it would publish upstream's
coordinates under ore's.

## Local hooks

`git config core.hooksPath fork/hooks` turns on two hooks that refuse what CI or
the sync machinery would reject later: a commit touching `codex-rs/Cargo.lock` or
`fork/upstream-workflows/`, one whose Markdown fails the `prettier --check` that
`pnpm run format` runs, and one on `delta` missing its trailers. They are cheap
and local; the authoritative checks stay in CI.

The pre-commit hook mirrors `package.json`'s `format` glob exactly rather than
inventing its own. Casting wider blocks commits CI would accept — `fork/*.yaml`
sits outside that glob deliberately, and prettier cannot parse
`fork/substitutions.yaml` at all.

## Reproducing CI locally

The workspace needs a prebuilt V8; `cargo build` fails on `v8` without it. CI
downloads one, and so can you — two env vars, and then everything builds:

```bash
VER=$(python3 .github/scripts/rusty_v8_bazel.py resolved-v8-crate-version)
T=aarch64-apple-darwin              # or x86_64-unknown-linux-musl, etc.
P=ptrcomp_sandbox_release
BASE=https://github.com/openai/codex/releases/download/rusty-v8-v$VER
mkdir -p /tmp/v8
curl -fsSL "$BASE/librusty_v8_${P}_${T}.a.gz"  -o /tmp/v8/lib.a.gz
curl -fsSL "$BASE/src_binding_${P}_${T}.rs"    -o /tmp/v8/binding.rs
export RUSTY_V8_ARCHIVE=/tmp/v8/lib.a.gz RUSTY_V8_SRC_BINDING_PATH=/tmp/v8/binding.rs
```

Worth knowing before you conclude a failure "needs CI": it does not. A sync that
looked unresolvable stayed that way only until this was set up, after which
`git bisect run` named the offending commit in one pass.

### Never fetch shallow

Comparing against an upstream ref is routine here, and `git fetch --depth=…` (or
cloning shallow) writes `.git/shallow`, which truncates history for everything
fetched afterwards. The damage is invisible where you would look for it: objects
read fine, `git fsck` is clean, and `rev-list --objects` succeeds, because none
of those need ancestry. Only SERVING a pack walks parents, so the first symptom
is a push the remote rejects for an object you have never had:

```
remote: fatal: did not receive expected object <sha>
error: remote unpack failed: index-pack failed
```

`git rev-list --count <base tag>` is the tell — 81 where the neighbouring tag
reaches 9784. `git fetch --unshallow upstream` is the fix, and
`fork/preflight.sh` now refuses to pass on a shallow repository.

## Which tree to test on

`delta` is not what ships. A substitution contradiction exists only *after*
substitution, so a test that passes on the series proves nothing about main — a
tui test built `home.join(".codex")` and asserted `~/.ore/…`, passed on delta,
and failed on every assembled tree. Run substitution-sensitive tests on a
checkout of the candidate.

The two trees also answer different questions for the invariants:
`version_check --root` on the series and `--tag` on the candidate are separate
assertions, and passing the second says nothing about the first.

## Repairing a blocked sync

When a sync fails, the repair does **not** go on `delta`. It goes on a staging
branch — `delta-staging`, cut from `delta` — and `assemble.sh --delta
delta-staging` rebases that onto the new tag.

The reason is not stylistic. A `known-failing` entry for the new base names a
test that does not exist yet at the old one, so `known_failing_check.py` reports
it dead and `delta`'s own CI goes red the moment you push. The entry is correct;
it is just early. After the rebase it resolves, which is why it has to travel
with the rebase rather than ahead of it.

```bash
git branch delta-staging delta          # repair work goes here
fork/assemble.sh --tag <new> --base <old> --delta delta-staging --worktree /tmp/asm
fork/preflight.sh <candidate>           # run from a checkout of sync-delta/<new>
```

`preflight.sh` runs the series half against `--root .`, so run it from a
checkout of the **rebased** series (`sync-delta/<tag>`), not from
`delta-staging` — on the pre-rebase tree the new entries look dead for the same
reason.

Landing force-pushes `sync-delta/<tag>` onto `delta`, so the staging branch is
consumed by the sync and should be deleted afterwards. `assemble.sh` refuses to
run if `sync-delta/<tag>` already exists; delete the local branch to re-run.

## Before landing a sync

```bash
fork/preflight.sh <candidate-ref>     # run from a series checkout
```

It runs both trees' checks together, which is the step whose absence let a green
sync turn `delta` red one push later.

Triage rules it prints, worth repeating: a CI failure set that *changes* between
runs is contention, one that *repeats* is real — re-run once before triaging.
And before adding a `known-failing` entry, demonstrate the cause (pass before
the seam commit and fail at the tip, or fail on a pristine upstream checkout).
Resemblance to an existing entry is not evidence; at rust-v0.150.1 it pointed at
the wrong cause twice.

## Before you claim something works

Absence of output is not evidence of absence. A test filter that matches nothing
prints `running 0 tests` and exits 0; a command whose flags are wrong prints usage
to stderr and exits non-zero. Check that the thing you ran actually ran.
