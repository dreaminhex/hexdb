# Security Policy

HexDB stores other people's data, so security reports get priority over feature work. Thank you for taking the time to report a problem responsibly.

## Supported versions

Security fixes are released as patch versions of the current minor release. Upgrade to the latest patch to get them; a lattice is upgraded one hex at a time without downtime (see [Restarts and upgrades](MANUAL.md#restarts-and-upgrades)).

| Version | Supported |
| --- | --- |
| 2.3.x | Yes |
| 2.2.x | Critical fixes only, until 2.4.0 is released |
| 2.1.x and earlier | No |

The same applies to everything released with HexDB: the server and `hexdb` CLI, the admin UI, the container image and Helm chart, the installers and packages, and the drivers (JavaScript, Python, .NET, Entity Framework Core, ODBC, JDBC). The VS Code extension, HexDB for VS Code, is supported in its latest version.

## Reporting a vulnerability

**Please don't open a public issue, discussion or pull request for a vulnerability.**

Report it privately, either way:

- **GitHub:** [Report a vulnerability](https://github.com/dreaminhex/hexdb/security/advisories/new) (the repository's Security tab). This keeps the report, the discussion and the fix in one private advisory.
- **Email:** support@hexdb.app, with "Security" in the subject.

Please include what you can of:

- the HexDB version (`hexdb --version`, or `GET /health` signed in) and the platform;
- the configuration that matters (with secrets removed): TLS, replication and slicing, roles, row filters and field masks, plugins;
- steps to reproduce, or a proof of concept;
- what an attacker gains (read or change data they shouldn't, escalate privileges, crash or stall a hex, recover keys) and what access they need first.

## What happens next

| Step | Within |
| --- | --- |
| We acknowledge your report | 3 business days |
| We confirm or decline it, with our assessment of its severity | 7 days |
| Updates on the fix | at least every 7 days until it's released |
| Fix released, for a critical or high-severity issue | 30 days, sooner where we can |

- **Accepted:** we fix it in the supported versions, publish a GitHub security advisory (requesting a CVE where it applies) and release notes, and credit you by name or handle unless you'd rather not be named.
- **Declined:** we explain why, for example that it's expected behavior, out of scope (below) or not reproducible. You're welcome to send more detail.
- **Disclosure:** please give us until the fix is released, or 90 days from your report, whichever comes first, before you publish. If we need longer we'll say why and agree a date with you.

HexDB doesn't run a paid bug bounty.

## Scope

In scope:

- this repository: the server, CLI, admin UI, drivers, plugins SDK, VS Code extension, container image, Helm chart and packaging;
- HexDB Enterprise and the website at hexdb.app.

Out of scope:

- **Things administrators can do by design.** Administrators can run code on the server (script functions, process plugins), change any role, read any data and change the lattice configuration. A finding that needs an administrator account isn't a vulnerability unless it crosses a boundary an administrator shouldn't, such as reading the storage key or another lattice's data.
- **The public playground's demo login.** Its credentials are published on purpose and its data resets every hour. Problems that let a visitor do more than the demo roles allow are in scope.
- Denial of service by traffic volume, social engineering, physical access, and attacks that need a compromised machine or a malicious plugin the administrator installed.
- Plain HTTP between hexes or to clients: HexDB supports TLS on every link and recommends it (see [Transport](MANUAL.md#transport) and [Between hexes](MANUAL.md#between-hexes)).
- Vulnerabilities in third-party dependencies without a way to exploit them through HexDB. Report those upstream; tell us too if HexDB is affected.
- Reports from automated scanners without a demonstrated impact (missing headers, version banners, TLS cipher grades).

## Testing guidelines

We won't pursue or support legal action against good-faith research that follows these guidelines:

- Test against a HexDB you run yourself. It's free and starts with `hexdb start` or one `docker run`.
- On the public playground, stay within the demo login, don't degrade it for other visitors, and don't try to reach its administrator account or other hexes' infrastructure.
- Never test against HexDB Enterprise lattices or other people's deployments, and don't access, change or keep data that isn't yours. If you reach someone else's data by accident, stop, don't keep it, and tell us.
- Don't run tests that degrade service for others, and don't use social engineering or phishing.

## Security design

How HexDB authenticates and authorizes requests, encrypts data at rest and between hexes, and audits access is described in the manual's [Security](MANUAL.md#19-security) chapter. It's a good place to start before testing, and it says what HexDB protects against and what it doesn't.
