# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through GitHub:
[open a security advisory](https://github.com/syz51/agent-board/security/advisories/new).
Please don't open a public issue, pull request or discussion for them.

Include what an attacker can do, the affected commands or endpoints, the version or commit
(`board --version`), and steps to reproduce. Leave out real tokens, hostnames and board
contents; placeholders are enough.

This is a one-maintainer project. Expect an acknowledgement within a week, and a fix or a
plan once the report is confirmed. You'll be credited in the advisory unless you ask not to be.

## Supported versions

Only the latest release, and `main`, get fixes while the project is pre-1.0.

## Scope

The server is designed to run on a private network ([`docs/DEPLOY.md`](docs/DEPLOY.md)), so
report a weakness even when it needs network access to the server. Examples in scope:

- authentication or authorization bypass, including trust of the `Tailscale-User-Login`
  header beyond what `trust_identity_header` allows;
- token disclosure in logs, errors, snapshots or the web view;
- script execution or remote loads in the web view from agent-written text;
- a board's contents reaching a party they shouldn't, such as through Telegram messages;
- `board hook`, `board watch` or `board install` doing something on a device beyond what it
  documents.

Out of scope: problems that need a valid device token used as designed, and deployments that
publish the server port against the advice in `docs/DEPLOY.md`.
