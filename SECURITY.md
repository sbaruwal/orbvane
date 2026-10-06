# Security

## Reporting a vulnerability

Please report security problems privately through GitHub:
[Report a vulnerability](https://github.com/sbaruwal/orbvane/security/advisories/new) (the
Security tab → Advisories). Don't open a public issue for them.

Include what you found, how to reproduce it, the Orbvane version (Orbvane → About Orbvane) and
your macOS version. You'll get a reply as soon as possible, and credit in the advisory and the
release notes when the fix ships, unless you'd rather not be named.

## Supported versions

Fixes go into the latest release. Orbvane updates itself (`update.mode`), so please check that a
problem still happens there.

## What's in scope

Orbvane itself: the app, its updater (downloads checked against their digest and the signing
team), extension installs (Open VSX packages checked against their signature), the credential
prompts git and ssh show through it, and the editor tools it offers the Assistant's agents.

Programs Orbvane starts but doesn't make (language servers, debug adapters, git, the agents'
CLIs) are reported to their own projects; if Orbvane runs one of them in an unsafe way, that's
ours.
