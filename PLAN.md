# popgres — Plan

A CLI that gives every project its own throwaway PostgreSQL instance. No
system-wide install, no Docker. Start it and a real Postgres comes up on a free
port; stop it and the data is wiped, unless you asked to keep it.

Written in Rust on top of [`postgresql_embedded`](https://github.com/theseus-rs/postgresql-embedded),
which downloads official PostgreSQL binaries on first use, caches them per
version, and wraps `initdb` / `pg_ctl`.

For what the commands do and how to configure a project, see the
[README](README.md). This file is the design and the roadmap.

## Architecture

**Project identity.** A project is the nearest ancestor directory holding a
`popgres.toml`, else the nearest one holding a `.git`, else the current
directory. Its state lives under the platform data dir keyed by a hash of that
path — `~/.local/share/popgres/<hash>/` on Linux,
`~/Library/Application Support/popgres/<hash>/` on macOS — holding the data
directory, `state.json` (host, port, credentials, database, version, keep, and
an optional expiry deadline), and the password file. Postgres binaries are
cached once globally and shared by all projects.

**Port and credentials.** Bound to `127.0.0.1` on a random free port, so
projects never clash. By default popgres writes its own `pg_hba.conf` trusting
loopback, so the instance takes no password and `DATABASE_URL` carries no
secret. Setting `password` in `popgres.toml` leaves initdb's password auth in
place instead.

**Lifecycle.** `popgres run` starts the instance, injects `DATABASE_URL` and the
`PG*` variables into the child, and tears everything down when the child exits —
signals included. It exits with the child's exit code. An instance that was
already running is reused and left running. Lifecycle transitions take an
advisory per-project lock, and liveness checks verify the recorded postmaster
identity before adopting, stopping, or wiping an instance.

The crate's `PostgreSQL` value stops the server when it drops, so the start path
deliberately forgets its handle and the command layer owns every teardown.

**Fresh-start seeding.** Because the default is a pristine database every run,
the `seed` hook — a `.sql` file or a shell command — runs after a fresh
`initdb`, and never over data resumed with `--keep`.

## Built

1. **MVP.** `up` / `down` / `url` / `status`; random port; state file;
   wipe-on-down with `--keep`, plus `down --wipe` to override it.
2. **The killer command.** `run -- <cmd>` with env injection and signal-safe
   teardown; `psql`; `reset`.
3. **Project config + seeding.** `popgres.toml`, seed hook, `env_file` writing,
   `--pg <version>` selection.
4. **Safe automation.** Verified postmaster liveness, serialized lifecycle
   transitions, private credential state, failure-safe cleanup, JSON output on
   every command, stable exit codes, and child signal exit propagation.
5. **Expiry and global cleanup.** Optional `--ttl`/`ttl` deadlines persisted in
   state, plus a lock-safe `popgres gc` sweep that disposes only expired
   instances and honors kept data.

## Next

**Agent-ready layer.** Agents are first-class users: an agent should spin up a
real Postgres, use it, and dispose of it without a human, a TTY, or leftover
processes when it forgets to clean up.

- Named ad-hoc instances (`up --name test-run-42`) for scratch databases
  unrelated to any project directory, and `popgres list` to see them all.
  Random ports make running many at once free.
- `popgres sql "select 1"` for one-off queries against a running instance.
- `popgres mcp` (stdio) exposing the same core as MCP tools, so agents needn't
  shell out at all.

**Teardown.** Forward SIGTERM to the child on signal instead of waiting out the
grace period and killing it — needs a `libc`/`nix` dependency.

**Distribution.** Wired up in `.github/workflows/release.yml`, triggered by a
`v*` tag: a build matrix over five targets, a GitHub release with per-target
archives and checksums, `cargo publish`, and npm packages built by
`npm/build.mjs` — one `@popgres/<platform>` package per target plus a launcher
that declares them as `optionalDependencies`, so no postinstall script runs and
no Rust toolchain is needed. The pipeline shipped `v0.1.0` end to end to
[GitHub Releases](https://github.com/algolab-cloud/popgres/releases),
[crates.io](https://crates.io/crates/popgres), and
[npm](https://www.npmjs.com/package/@popgres/cli). npm and crates.io enforce
Trusted Publishing from `release.yml`; both registries use short-lived GitHub
OIDC credentials, so no registry tokens are stored as repository secrets.
Still open: a `bundled` build for offline installs, and musl/Alpine binaries.

## Caveats

- The first run for a given Postgres version needs network access; later starts
  take a second or two from cache.
- CI builds and tests the CLI on macOS, Linux, and Windows. The full PostgreSQL
  lifecycle has primarily been exercised on macOS; Windows has no POSIX
  signals, so `run`'s teardown needs additional platform-specific testing.
