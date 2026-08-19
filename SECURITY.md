# Security policy

## Reporting a vulnerability

Do not open a public issue for vulnerabilities involving credentials, worker
identity, tenant isolation, job artifacts, purge behavior, or remote execution.
Use GitHub's private vulnerability reporting for this repository:

https://github.com/decent-render/decent-render/security/advisories/new

Include affected versions/commits, reproduction steps using synthetic data,
impact, and any proposed mitigation. Do not attach customer content, real worker
tokens, dispatch secrets, or private render payloads.

## Scope priorities

High-priority security boundaries include:

- bypassing `purgeAfter: true` or retaining job data after termination;
- accepting real jobs without explicit operator opt-in;
- worker/operator/platform identity spoofing;
- tenant/job crossover;
- arbitrary payload execution outside the verified versioned-payload contract;
- checksum/signature bypass, including the render path's bundle sha256 gate in
  `packages/runner-core` (a bundle whose bytes do not match the advertised
  sha256 must never reach a renderer);
- job artifacts surviving the render (`packages/runner-core` purges the per-job
  working directory in a `finally`);
- credential disclosure in logs, status files, crash reports, or release assets;
- protocol-version downgrade or parser divergence.

## Payload provenance is out of scope

The render payload an operator executes is compiled and published by the closed
platform from `packages/runner-core` plus a pinned `@remotion/renderer`. Builds
are not reproducible, so "the downloaded binary does not correspond to the
published source" is not currently a verifiable property and is not treated as a
vulnerability report. Reports that the supervisor accepts a payload whose bytes
do not match the sha256 dispatch advertised **are** in scope.

## Supported versions

Until a stable release line exists, only the latest published `decent-node`
version and current `main` receive security fixes. Historical pre-1.0 versions
may require upgrading rather than backporting.

## Public disclosure

Please allow time to validate and release a fix before public disclosure. The
maintainers will acknowledge the report, coordinate severity and timeline, and
credit reporters who want attribution.
