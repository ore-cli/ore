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
