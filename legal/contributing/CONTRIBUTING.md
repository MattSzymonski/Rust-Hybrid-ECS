# Contributing to Pill Engine

Thank you for considering a contribution. Bug reports, documentation,
examples, tests and code are all welcome.

**You do not need to sign anything to use Pill Engine.** Signing a
Contributor License Agreement (CLA) is required only to contribute.

## Workflow

1. Open or find an issue. Discuss significant changes (architecture, public
   interfaces, new dependencies) before starting on them.
2. Fork the repository and branch from `main`: `feature/<name>`,
   `fix/<name>`, `docs/<name>` or `refactor/<name>`, one topic per branch.
3. Make the change with tests. Bug fixes include a test that fails without
   the fix where practical; changes to the core, host or extension crates
   are also checked at runtime, since a passing build does not show that hot
   reload still works.
4. Open a pull request against `main` explaining what changed and why, and
   accept the CLA when asked.

The Licensor, or a maintainer they appoint, reviews every change for
correctness, reload safety, performance, clarity and tests; the Licensor
makes the final decision.

## Coding standards

Follow the
[Pill Engine coding standards](https://docs.pillengine.org/guide/other/coding-standards.html).

## Contributor License Agreements

- **Individuals** accept the [Individual CLA](CLA.md) once per CLA
  version.
- If your **employer**, client or university may own your work, it signs the
  [Corporate CLA](CCLA.md) and lists you, or gives you written permission to
  contribute under the Individual CLA. Contact contact@pillengine.org.

## Third-party code and dependencies

- New dependencies and any third-party files need the Licensor's approval.
- Accepted licenses: MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC,
  Zlib, 0BSD, BSL-1.0, CC0-1.0, Unlicense, Unicode-3.0 and MPL-2.0 (an
  unchanged dependency only); OFL-1.1 and the Ubuntu Font Licence for
  fonts; CC-BY for assets. Other copyleft (GPL, LGPL and similar),
  non-commercial and proprietary licenses are not accepted without explicit
  approval. For Rust crates, [`modules/about.toml`](../../modules/about.toml)
  enforces this list.
- Identify the origin and license of anything you did not write yourself.
- If you used an AI tool, check that what it generated does not copy
  anyone else's code or infringe their rights, and that the tool's terms
  let you contribute it. You do not need to mark which parts it wrote.
- Redistributed files are recorded in
  [`legal/THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md).

## Contact

Licensing and security issues: contact@pillengine.org. Please report
security issues there, not in public issues.
