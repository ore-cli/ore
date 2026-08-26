# Security policy

## Reporting a vulnerability

Report privately through GitHub Security Advisories: open the
[Security tab](https://github.com/ore-cli/ore/security) of this repository and
choose **Report a vulnerability**. Please don't open a public issue for
something exploitable.

**If the bug is in code ore inherited from Codex, report it to OpenAI as
well** — [their Bugcrowd program](https://bugcrowd.com/engagements/openai) is
where it gets fixed for everybody, including us. Most of this codebase is
theirs, so most vulnerabilities will be. ore's own changes are the telemetry
removal, the Anthropic, Gemini and Chat Completions providers, the layered
`~/.ore` config home, the release and update plumbing, and the rename; a bug in
those is ours alone, and the Security tab above is the place for it. The
provider adapters are the part of that list which handles credentials and talks
to the network, so they are the part most likely to matter here.

ore is a small project with no security team and no bounty.

## Scope

ore runs commands and edits files on your machine on a model's instructions.
That is the feature, so "the agent did something I didn't want" is not by
itself a vulnerability. What counts is the sandbox and approval system failing
to hold: a command escaping the sandbox, an approval prompt bypassed,
credentials leaking somewhere they shouldn't, or a hostile repository or MCP
server escalating into code execution outside the boundary.

See [docs/sandbox.md](./docs/sandbox.md) and
[docs/execpolicy.md](./docs/execpolicy.md) for what those boundaries are meant
to be.

## Untrusted models

ore will talk to any OpenAI-compatible endpoint you configure, which means you
can point it at a model nobody has vetted. Everything it emits — commands to
run, files to write, URLs to fetch — reaches the same execution path as a
frontier model's output, and the sandbox is what stands between the two.
Choose the endpoint as carefully as you would choose anything else you grant
shell access.

## Sign-in and credentials

The ChatGPT sign-in path — endpoints, client identity, token handling and
storage — is kept byte-identical to upstream Codex on purpose, and CI enforces
that fence. Your credentials are requested, stored and sent exactly as Codex
does it, to the same places and nowhere new. The one behavioural difference is
subtractive: ore removes all telemetry, so no analytics, metrics or crash
reports leave your machine at all.
