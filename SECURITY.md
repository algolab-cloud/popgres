# Security Policy

## Supported versions

popgres is pre-1.0. Security fixes land on the latest release only.

## Reporting a vulnerability

Please **do not** open a public issue.

Report privately through
[GitHub Security Advisories](https://github.com/algolab-cloud/popgres/security/advisories/new).

Include what you can: affected version, platform, reproduction steps, and impact.
You can expect an acknowledgement within a few days and an assessment shortly
after. Please give a reasonable window for a fix before disclosing publicly.

## Threat model

popgres runs a real PostgreSQL server as a local, unprivileged process. By design:

- The server binds to `127.0.0.1` on an ephemeral port — it is not reachable from
  the network.
- **By default an instance takes no password**: popgres writes a `pg_hba.conf` that
  trusts loopback connections. The trade is deliberate — a disposable, loopback-only
  database in exchange for a `DATABASE_URL` with no secret in it. The consequence is
  equally deliberate: **anyone with an account on the machine can connect to it as
  superuser.** On a shared or multi-tenant host, set `password` in `popgres.toml`,
  which makes popgres require real password authentication.
- With a password configured, the connection URL contains it, so `popgres url`
  output, `--json` output, and any `env_file` popgres writes become secrets. Keep
  generated env files out of version control (popgres creates them `0600`).
- PostgreSQL binaries are downloaded from
  [theseus-rs/postgresql-binaries](https://github.com/theseus-rs/postgresql-binaries)
  and cached globally, shared across projects on the machine.

Things that are **not** vulnerabilities in popgres: the passwordless default described
above (it is documented and configurable), and other local users on a shared machine
being able to read your data directory (protect it with normal filesystem permissions).
