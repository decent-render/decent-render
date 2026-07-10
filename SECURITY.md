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
- checksum/signature bypass;
- credential disclosure in logs, status files, crash reports, or release assets;
- protocol-version downgrade or parser divergence.

## Supported versions

Until a stable release line exists, only the latest published `decent-node`
version and current `main` receive security fixes. Historical pre-1.0 versions
may require upgrading rather than backporting.

## Public disclosure

Please allow time to validate and release a fix before public disclosure. The
maintainers will acknowledge the report, coordinate severity and timeline, and
credit reporters who want attribution.
