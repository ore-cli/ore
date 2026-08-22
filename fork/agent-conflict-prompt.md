# Resolving a delta-series rebase conflict

You are resolving merge conflicts for `ore`, a fork of openai/codex. The fork
is an ordered series of small, single-purpose commits (the *delta series*) that
gets rebased onto each new upstream release. One of those commits has just
stopped mid-rebase. Your working directory contains ONLY:

- `PROMPT.md` — this text, with the stopped commit's intent header appended;
- the conflicted files, at their repository-relative paths, containing standard
  git conflict markers.

There is no repository here and nothing else to read. Edit the conflicted files
in place; that is your entire interface.

## Which side is which

This is a rebase onto a NEW upstream release, so the sides read backwards:

- `<<<<<<< HEAD` up to `=======` is **upstream's new code** from the fresh
  release (plus any series commits already replayed).
- `=======` down to `>>>>>>>` is **the fork's old patch text**, written against
  the previous release.

## The one rule: the Invariant outranks the diff

Every series commit carries an intent header. `Invariant:` states the behaviour
the commit exists to guarantee — e.g. *"Config.analytics_enabled is Some(false)
for every layer combination"*. The old patch text is merely how that invariant
was expressed against last release's code. Your job is to re-express the same
invariant against upstream's new code, never to restore last release's text.

Concretely:

1. **Upstream wins everywhere the invariant is not at stake.** Take the HEAD
   side verbatim — new signatures, renamed locals, moved blocks, reordered
   fields — and re-apply only the fork's minimal change on top of it.
2. **The seam is usually a single token or line.** Most series commits pin one
   value at one point. If upstream rewrote or relocated the surrounding
   function, the right resolution is upstream's new function with the fork's
   token re-applied at the equivalent point. `Conflict-notes:` often says
   exactly where; e.g. *"the seam is the single read of cfg.analytics, not this
   line number"*.
3. **If upstream now does what the invariant demands**, resolve to the pure
   HEAD side. The commit then becomes empty and is dropped as absorbed — a
   correct outcome, not a failure.
4. **Never widen the change.** No refactoring, no reformatting, no unrelated
   fixes, no comment cleanups, no new files, and no edits outside the listed
   conflicted files.

## What the fork pins (the seams you are protecting)

| Where | The fork's pin |
|---|---|
| `codex-rs/core/src/config/mod.rs` | `analytics_enabled: Some(false)` at the single read of `cfg.analytics`; `feedback_enabled: false` |
| `codex-rs/config/src/types.rs`, `codex-rs/core/src/config/otel.rs`, `codex-rs/otel/src/config.rs` | every default otel exporter is `None`, and the built-in Statsig route (endpoint + `statsig-api-key`) stays deleted |
| `codex-rs/utils/home-dir/src/lib.rs` | `find_codex_home()` honours `ORE_HOME` first, defaults to `~/.ore`; `find_legacy_codex_home()` exists |
| `codex-rs/config/src/loader/mod.rs` | the legacy `~/.codex` config layer sits beneath ore's own user layer |
| `codex-rs/tui/src/updates.rs`, `codex-rs/tui/src/npm_registry.rs` | update checks point at ore's release feed, never `openai/codex` |
| `codex-rs/app-server-daemon/src/update_loop.rs` | the hourly self-updater never starts |
| `scripts/codex_package/targets.py` | `executable_stem = "ore"` while `cargo_bin` stays `"codex"` — that distinction is the whole patch |
| `codex-rs/app-server-daemon/src/managed_install.rs` | the managed binary's shipped name is `ore` |
| `codex-rs/cli/src/main.rs` (version identity) | `--version` line 1 is exactly `ore <version>`; line 2 reports the codex base and ends in `)` |
| `codex-rs/Cargo.toml` | fork-added crates stay in `members`; upstream's ordering and everything else wins |
| `codex-rs/model-provider-info/src/lib.rs`, `codex-rs/core/src/client.rs`, `codex-rs/model-provider/src/provider.rs` | fork-added `WireApi` variants and their match/factory arms stay alongside upstream's `Responses` arm |

## When to stop instead of guessing

If the invariant genuinely cannot be preserved — the code it patched is gone
with no equivalent, or upstream's redesign contradicts it — **do not guess**.
Leave the conflict markers in place, print one short paragraph on stdout
explaining exactly what changed and why the invariant no longer fits, and exit
non-zero. A rejected stop goes to a human with your explanation attached; a
guessed resolution silently ships wrong behaviour.

## Mechanics

- Remove every `<<<<<<<`, `|||||||`, `=======`, `>>>>>>>` marker line from each
  file you resolve; each resolved file must be the complete file as it should
  exist in the new tree.
- Do not create, rename or delete files. Do not run git — there is no
  repository here. Do not fetch anything.
- The wrapper stages, verifies and continues the rebase itself. Any file it
  finds changed outside the conflicted set, any leftover marker, or any leftover
  unresolved path causes the whole rebase to be rejected and your work
  discarded.
