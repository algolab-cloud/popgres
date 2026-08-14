# Changelog

All notable changes to popgres are documented here. This project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html); while the version
is below 1.0, minor releases may change behavior.

## 0.3.0

### Added

- `popgres list` shows every instance on this machine — status, port, version,
  remaining TTL, and project — marking the current project with `*`. It is
  strictly read-only: it starts nothing, stops nothing, and creates no files.
  Connection URLs are omitted because it spans every project; `popgres url`
  still prints the current one.

### Fixed

- A lock released moments earlier could be reported as still held, making
  `gc` skip a project that nothing was using. Callers that must not block now
  wait out a brief release grace period before concluding a project is busy;
  genuinely held work is still reported promptly.

## 0.2.1

### Added

- `popgres gc --dry-run` reports expired instances without stopping servers,
  wiping data, clearing environment files, or creating missing lock files.
- CI now builds with the declared minimum supported Rust version.

### Fixed

- Dry-run applies the same fail-safe liveness checks as a real sweep, so an
  unverifiable instance is reported as skipped rather than eligible to reap.
- Building from source now declares Rust **1.94** as the minimum supported
  version, producing a clear toolchain error instead of failing on standard
  library file-locking APIs.

## 0.2.0

Safe unattended use: popgres can now be handed to CI jobs and AI agents that
run it concurrently, without a human watching.

### Changed

- **`popgres up` now exits `10` instead of `0` when it finds the instance
  already running.** Scripts written against 0.1.0 that relied on
  `popgres up && …` under `set -e` will stop at that point even though the
  database is available. Treat `10` as success-with-adoption, or read
  `already_running` from `up --json`.
- Coded exits start at `10` (`10` already running, `11` port busy, `12` no
  running instance) so they can never collide with the `2` that clap returns
  for a mistyped invocation.
- `popgres status` exits `12` when no instance is running, so automation can
  branch without parsing output.
- `--json` is a single global flag accepted after any subcommand.

### Added

- `--ttl` (and `ttl` in `popgres.toml`) records a deadline for an instance,
  plus `popgres gc` to dispose of everything past its deadline across every
  project on the machine. Deadlines are opt-in; an expired instance configured
  with `keep = true` has its server stopped but its data preserved.
- Machine-readable output everywhere: `url --json`, newline-delimited
  lifecycle events from `run --json`, `expires_at`/`expired` in
  `status --json`, and errors as JSON on stderr.
- `run` propagates `128 + signal` when its child is terminated by a signal.

### Fixed

- Liveness is verified by postmaster identity (PID file, data directory, port)
  and a PostgreSQL wire handshake rather than a bare TCP connect, so popgres no
  longer adopts an unrelated service that claimed a recycled port. When
  liveness genuinely cannot be determined, commands now say so instead of
  wiping a data directory that may still be live.
- Lifecycle transitions are serialized by a per-project advisory lock, so
  concurrent `up`/`run` invocations can no longer both drive `initdb` against
  the same data directory. A contended lock reports that it is waiting instead
  of hanging silently.
- `run` disposes of the instance it created when the child command fails to
  spawn or the seed hook fails, and still reports the child's exit code when
  teardown afterwards fails.
- `reset` performs its stop and start under a single lock, keeping the port —
  and so the connection URL — stable.
- `state.json` is written `0600`; it may contain a configured password.
- Resuming a kept data directory whose PostgreSQL major version disagrees with
  the saved state now fails with an actionable message instead of a catalog
  mismatch from deep inside Postgres.

## 0.1.0

Initial release: `run`, `up`, `down`, `status`, `url`, `psql`, `reset`,
`popgres.toml` configuration with seeding and `env_file` writing, and
distribution via crates.io, npm, and GitHub Releases.
