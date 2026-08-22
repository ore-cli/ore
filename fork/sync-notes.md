# Sync rehearsal — what one real upstream hop actually costs

Run locally on 2026-08-22 against the finished Phase-1/2 series, to answer the
question the plan's effort estimate rests on: does the delta series survive a
real upstream version bump, and how much human work does one sync need?

## Method

`rust-v0.148.0` and `rust-v0.149.0` are adjacent upstream stable tags. The
series lives on 0.149.0, so the hop was rehearsed **backwards** —
`git rebase --onto rust-v0.148.0 rust-v0.149.0` with rerere enabled. The
conflict surface is the same in both directions (it is a function of what
upstream changed at the seams), so this measures a forward hop without needing
a tag that does not exist yet.

## Upstream churn across the hop

    1139 files changed, 67891 insertions(+), 13505 deletions(-)

At the fork's own seam files:

| file | churn |
|---|---|
| `codex-rs/cli/src/main.rs` | +299 / −21 |
| `codex-rs/core/src/config/mod.rs` | +109 / −26 |
| `codex-rs/config/src/loader/mod.rs` | +99 / −24 |
| `codex-rs/core/src/client.rs` | +46 / −9 |

That is a large release, and it lands directly on top of four of the fork's
seams.

## Result

The series replayed commit by commit and stopped **twice**:

1. `telemetry-feedback-off` on `codex-rs/feedback/src/lib.rs` —
   `upload_feedback` is synchronous and single-argument at 0.148.0 and async
   with an `HttpClientFactory` at 0.149.0. The patch was written against the
   0.149 signature, so it cannot apply to the 0.148 one.
2. `desktop-app-gate` on `codex-rs/cli/src/doctor/updates.rs` and
   `codex-rs/tui/src/tooltips.rs` — both files churned heavily.

Nineteen of the twenty-one commits replayed with no human involvement.

## What the intent headers were worth

Conflict 1 was resolved from its own commit message. The `Invariant:` line —
"Feedback reports cannot be sent, and no report destination ships in the
binary" — says what to preserve, and the `Conflict-notes:` line —
"Upstream reintroducing a default destination will collide here; the invariant
is 'no destination', not 'this constant'" — says how. The resolution is
mechanical once those two sentences are in front of you: apply the force-error
to whatever signature the target version has, and keep the DSN deleted.

This is the case for the trailer discipline. Neither the diff nor the code
around it tells a resolver what the commit was *for*; the header does, and the
conflict agent is handed exactly that header and nothing else.

## rerere

Three preimages were recorded across the two stops, and the resolution produced
one postimage. A repeat of the same hop replays that resolution automatically,
which is the property the orphan `rerere-cache` branch exists to carry between
CI runs.

## Read-across

Two stops per release, both at seams whose files upstream is actively
rewriting, both resolvable from the commit's own header. Nothing here suggests
the series is fragile; it suggests the fragile commits are exactly the ones the
seam registry (`fork/seams.yaml`) already names, and that the semantic review's
seam-proximity warning is pointed at the right files.

---

# Verify red-team — does the suite actually bite?

Same session. Six mutations applied to a clean tree, each the kind of regression
a sync could plausibly introduce, to check that the invariant suite fails rather
than passes quietly.

| # | mutation | caught |
|---|---|---|
| 1 | Reintroduce the Statsig OTLP endpoint literal | yes — `strings-source` |
| 2 | Rewrite the ChatGPT originator inside `codex-rs/login/` | **NO** (fixed) |
| 3 | Add an unallowlisted workflow with an upstream tag glob | yes — `workflows` |
| 4 | Point `fork/UPSTREAM` at the wrong base tag | yes — `upstream` |
| 5 | Add an undeclared package to `Cargo.lock` | yes — `locks` |
| 6 | Drop a new source file inside the auth fence | **NO** (fixed) |

## What 2 and 6 were

`auth_fence.py` ran `git diff <base> HEAD`, which compares two commits and never
looks at the working tree, so an uncommitted rewrite of `codex_cli_rs` — the one
change the fence exists to prevent — was reported as "auth fence intact". A new
file inside a fenced path was invisible for a different reason: a file that did
not exist at the base produces no diff hunk to compare.

Both are fixed: the check diffs the base against the working tree, and refuses
any untracked file under a fenced path. In CI the tree and HEAD are the same, so
neither mutation could have been caught by the run that mattered either.

The lesson worth keeping is not "the fence had a bug" but that the four checks
which passed the red-team were the four written against tree contents, and the
one that failed was the one written against commit history. A check whose
subject is "what ships" has to read what ships.

---

# Tag policy — the server-side layer, verified live

The fork defends the "never push an upstream-name tag" rule in four places:
upstream tags are fetched into `refs/upstream/tags/*` so they are not local tags
at all; `safe_push()` allowlists the refs it will push; `workflows_check.py`
asserts no fork workflow carries an upstream tag glob; and a repository ruleset
refuses the ref server-side.

Only the fourth one holds against a human who bypasses the tooling, so it is the
one worth testing rather than assuming. Installed on `ore-cli/ore` and tested by
pushing an annotated `rust-v0.0.1-oreguardtest` from an owner account:

    remote: error: GH013: Repository rule violations found for
            refs/tags/rust-v0.0.1-oreguardtest.
    remote: - Cannot create ref due to creations being restricted.
     ! [remote rejected] (push declined due to repository rule violations)

`bypass_actors` is empty and the API reports `current_user_can_bypass: never`,
so the owner is refused too -- which is the ratified shape, and the reason the
mirrored rusty-v8 assets use `ore-rusty-v8-v*` tags rather than a bypass.

The test was run with Actions disabled, deliberately: had the ruleset not been
in force, the tag would have landed and upstream's `rust-release.yml` would have
been sitting in the tree it fired from.
