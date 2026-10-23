# Security Policy

## Reporting a vulnerability

Please **do not** open a public issue for a security problem.

Report it privately through GitHub:
[**Report a vulnerability**](https://github.com/driftwerk/workforce/security/advisories/new)
(the "Security" tab → "Report a vulnerability").

Include what you found, how to reproduce it, the version or commit you tested,
and the impact you expect. We aim to acknowledge reports within three working
days and to agree a disclosure date with you once a fix is ready.

## Scope

In scope: the code in this repository — the hub, CLI, agent runtime, skills,
web UI and the published crates.

Out of scope: vulnerabilities in third-party dependencies that are already
public (report those upstream), and issues that need an already-compromised
host or administrator account.

## Supported versions

Security fixes land on `main`. Run a recent build of `main` or the latest
release.

See [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) for what Workforce isolates,
what it doesn't, and what operators are responsible for.
