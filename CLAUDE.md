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

Do **not** hand-edit an upstream file to change "Ore" to "Ore". Add a rule to
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

## Before you claim something works

Absence of output is not evidence of absence. A test filter that matches nothing
prints `running 0 tests` and exits 0; a command whose flags are wrong prints usage
to stderr and exits non-zero. Check that the thing you ran actually ran.
