# Security policy

## Reporting a vulnerability

Please report security problems privately through GitHub's private
vulnerability reporting: open the repository's **Security** tab and choose
**Report a vulnerability**
(<https://github.com/pwsh/trunkd/security/advisories/new>). Do not open a
public issue for a vulnerability.

Include the trunkd version (`trunkd --version`, or the panel's About page),
the platform, and the steps or input needed to reproduce the problem. You
should get a first reply within a week. Once a fix is released, the advisory
is published with credit to the reporter unless you ask otherwise.

## Supported versions

Only the latest release receives security fixes.

## Scope

In scope: the `trunkd` binary and the crates in this repository, including
the local control panel, the recorder link, the Keyup control channel, the
call upload and the exporters.

Out of scope: decoding of encrypted traffic (trunkd never attempts it, and
reports that it cannot is not a vulnerability), vulnerabilities in a Keyup
server (report those to the Keyup repository), and issues that need an
attacker who already controls the host or its configuration file.

The local control panel is meant for a trusted network. Do not expose its
port to the internet; put it behind an authenticating reverse proxy if you
need remote access.
