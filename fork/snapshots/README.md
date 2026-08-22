# `fork/snapshots/` — CLI surface fixtures

Captured and compared by `fork/verify/snapshots.py`.  Contents:

```
help/root.txt      `ore --help`
help/<sub>.txt     `ore <sub> --help` for every top-level subcommand
version.txt        `ore --version`, version normalised
```

These freeze the user-visible brand surface: the rebranded program name, the
`ore <subcommand>` usage lines, the absence of hidden subcommands, and no
stray "codex" in help text outside allowlisted wire terms.  A missed
substitution after a sync shows up here as a readable diff instead of a
silent regression.

Normalisation (so fixtures survive releases and machines):

- the reported semver becomes `{{VERSION}}`; the base line's tag/SHA become
  `{{BASE_TAG}}`/`{{BASE_SHA}}` (`version.txt` is otherwise a format check —
  `version_check.py` owns the strict rules)
- temp paths (`/tmp/…`, `/var/folders/…`, `$TMPDIR`) become `{{TMP}}`
- trailing whitespace is stripped; capture runs with stdout as a pipe and
  `COLUMNS` unset, so clap wraps at its deterministic non-tty default

Updating (expected on most syncs — upstream reworks help text constantly):

```
fork/verify/snapshots.py --update --bin <path-to-built-ore>
```

Commit the result on `delta` so the diff is reviewed by a human.  Resist the
temptation to demote these to warn-only: help churn is exactly the signal
that the substitution manifest needs regeneration.

The directory ships empty of fixtures until the first assembled binary
exists; `snapshots.py` reports that state as pending, not as a failure.
