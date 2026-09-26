# Changelog

All notable changes to popgres are documented here. This project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html); while the version
is below 1.0, minor releases may change behavior.

## 0.5.0

### Added

- **Seed cache: fresh starts in a fraction of a second.** After a fresh
  instance is initialized and seeded, popgres keeps a copy of its data
  directory. The next fresh start with the same inputs copies it into place
  instead of running initdb and the seed. On a 20,000-row seed, `popgres run`
  went from about 2.2 s to 0.45 s. The cache key covers the PostgreSQL
  version, extensions, password, settings and the seed; a `.sql` seed is
  hashed by content, and a command seed is cached only when `seed_inputs`
  lists the files it reads. `seed_cache = false` opts out.
- **`reset` skips an unchanged seed.** With the seed unchanged, `popgres
  reset` re-clones the working database from the seeded template instead of
  re-running the seed.
- **`fast = true`** turns off `fsync`, `synchronous_commit` and
  `full_page_writes` for faster write-heavy tests on disposable data.
- **`[settings]`** sets any PostgreSQL server setting from `popgres.toml`.
  Settings are applied on every start, so changes reach a resumed kept
  instance too.
- `popgres cache` reports cached seeded databases, and `cache --clean` and
  `gc` reclaim them. Each project keeps its three most recent entries.
- `up --json` and `run`'s `ready` event include `restored_from_cache`;
  `reset --json` includes `reseeded`.

## 0.4.2

### Fixed

- **`run` no longer leaves a database behind when interrupted during
  startup.** Ctrl-C, SIGTERM or SIGHUP while the database was starting or
  seeding killed popgres outright, orphaning the instance. The first signal
  now lets startup finish, tears the database down and exits `128 + signal`
  without running the command; a second signal exits immediately.
- **SIGTERM reaches `run`'s command.** A SIGTERM sent to popgres alone (by CI
  or a supervisor) is forwarded to the child instead of waiting out the
  10-second grace period and killing it. A second signal kills it at once. A
  duplicate of the same signal within 250 ms — the terminal and the npm
  launcher both delivering one Ctrl-C — counts once.
- **Passwords stay out of the process list.** popgres's own `psql` calls
  (`popgres psql`, seeding, template setup, extensions) pass a configured
  password in `PGPASSWORD` instead of on the command line.
- `up` and `run` on an already-running instance fail clearly when it does not
  match a requested `--pg` or `--port`, instead of silently handing back the
  instance that is running.
- `cache --clean` spares variants and PostgreSQL installs used in the last
  hour, as `gc` already did, so it cannot pull one out from under a start.

## 0.4.1

### Fixed

- **A failed first start no longer strands the project.** Any failure after
  `initdb` — an unknown extension, a busy port, a failed download — left a
  data directory without a state file, and every later `up` refused it until
  it was deleted by hand. A failed fresh start now removes what it created.
- **Passwords with URL-reserved characters work.** `password = "p@ss/word"`
  produced a malformed `DATABASE_URL` and failed the start; user, password
  and database are now percent-encoded.
- `state.json` is written atomically, so a crash mid-write cannot leave it
  corrupt.
- `popgres reset` of a stopped instance no longer insists on its old port,
  and a full reset keeps the instance's TTL deadline, as the in-place reset
  already did.
- `popgres gc` can no longer evict an extension variant that a concurrent
  start has just picked up.
- A `seed` naming a missing `.sql` file reports the missing file instead of a
  shell "not found"; `export DATABASE_URL=…` lines in `env_file` are replaced
  rather than duplicated; generated `testdb` names respect PostgreSQL's
  63-byte limit for non-ASCII database names.

## 0.4.0

### Changed

- **Instances now live in `.popgres/` inside the project by default**, like
  `.git` or `node_modules`: delete the project and its database goes with it.
  The directory ignores itself and carries a `CACHEDIR.TAG` so version
  control and backup tools skip it. Set `location = "global"` in
  `popgres.toml` for the old behavior — recommended for projects in synced
  folders (Dropbox, iCloud), where syncing a live data directory risks
  corruption. A project with an existing global instance keeps using it until
  that instance is wiped; the next fresh start is local.

### Added

- **Test databases from a template.** A fresh instance now seeds a template
  database (`popgres_template`), locks it against connections, and clones the
  working database from it. `popgres testdb` clones it again in ~0.1 s — one
  private, fully seeded database per parallel test worker — and
  `testdb --clean` drops every generated clone. Seeds now run against the
  template (`DATABASE_URL` points there during seeding), so extensions and
  seed data are inherited by every clone. `popgres reset` on a running
  instance uses the same machinery: same port, seed re-run, well under a
  second instead of a full re-initialization.
- **PostgreSQL extensions.** `extensions = [...]` in `popgres.toml` creates
  extensions before the seed hook runs. The ~46 bundled contrib extensions
  (`pg_trgm`, `hstore`, `pgcrypto`, `citext`, `uuid-ossp`,
  `pg_stat_statements`, …) work on every PostgreSQL version with no download
  and no extra disk. Downloaded extensions — `vector` (pgvector) and
  `vectors` (pgvecto.rs) — install into shared variants. The pristine
  PostgreSQL install is never modified: projects with downloaded extensions
  run from an immutable, globally shared *variant* built once per
  version-and-extension combination. The first build takes seconds, every
  later project reuses it instantly, and `popgres gc` evicts variants nothing
  references. Version pins go in `[extensions_versions]`; changing a kept
  database's extensions asks for `popgres reset` instead of letting the
  postmaster fail. pgvector currently ships prebuilt for PostgreSQL 16, and
  the error says exactly that if another version is requested.
- A small global registry lets `list` and `gc` find local instances across
  the machine; entries whose project or instance has been deleted are pruned
  automatically.
- `popgres cache` reports popgres's disk footprint — PostgreSQL versions,
  extension variants, and instances, each marked in use or unused — and
  `cache --clean` reclaims unused variants (`--all` extends to PostgreSQL
  versions nothing references; the download cache can be shared with other
  postgresql-embedded tools, so that step is opt-in). Instance data is never
  touched.

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
