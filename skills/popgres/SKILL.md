---
name: popgres
description: Run disposable local PostgreSQL instances with popgres for development, tests, migrations, schema work, and commands that need DATABASE_URL. Use when an agent is asked to use or install popgres, create an isolated PostgreSQL database, run a task against real local Postgres without Docker, seed or inspect a popgres project, or safely clean up a temporary database.
---

# Popgres

Use popgres to give a project a real local PostgreSQL instance without a system
Postgres installation or Docker. Prefer lifecycle-scoped commands and preserve
instances or data that existed before the task.

## Choose the invocation

1. Run `command -v popgres`.
2. If installed, invoke `popgres` directly.
3. Otherwise, if Node.js is available, invoke `npx --yes @popgres/cli` without
   adding a dependency to the project.
4. If neither is available, ask before installing a persistent dependency or
   toolchain. Supported installs are `npm install --save-dev @popgres/cli` and
   `cargo install popgres`.

Run commands from the intended project directory. Popgres identifies the
project using the nearest `popgres.toml`, then the nearest `.git` root, then the
current directory.

## Prefer scoped execution

For a test, migration, development server, or other single command, run:

```sh
popgres run -- <command> [arguments...]
```

For example:

```sh
popgres run -- npm test
popgres run -- cargo test
popgres run -- npm run migrate
```

Popgres injects `DATABASE_URL`, `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, and
`PGDATABASE` into the child, forwards its exit code, and tears down an instance
that it created. It reuses and leaves alone an instance that was already
running. Keep popgres options before `--`; everything after `--` belongs to the
child command.

Do not add `--keep` unless the user requests persistent data. Check
`popgres.toml` before promising disposal because `keep = true` also preserves
data.

## Coordinate multiple commands

Prefer one project script that performs migrations, tests, and other dependent
steps, then run that script through `popgres run`.

If separate commands must share an instance:

1. Check `popgres status --json`, accepting exit code 12 as "not running".
2. Start with `popgres up --ttl 30m --json` and retain its output without
   printing it; accept exit code 10 when `already_running` is true.
3. Read `url` for child-process environments and `already_running` for cleanup.
4. Run every dependent command with `DATABASE_URL` set.
5. If `already_running` is `false`, call `popgres down --json` in a guaranteed
   cleanup path. If it is `true`, leave the instance running.

Never wipe an instance that was already running. Never use `down --wipe` or
`reset` unless disposing of the data is explicitly intended.

## Guarantee cleanup with a deadline

Pass `--ttl` whenever starting an instance that outlives a single command, so
a crash or an interrupted session cannot leave a database running forever:

```sh
popgres up --ttl 30m --json
```

Use a deadline comfortably longer than the work. Check what a sweep would
remove with `popgres gc --dry-run --json`, which reports without stopping or
wiping anything. `popgres gc --json` then
disposes of anything past its deadline, in any project, and never touches an
instance that has not expired. Prefer `popgres run`, which needs no deadline
because it disposes of what it started. `--ttl` never replaces an explicit
`down`; it only bounds the damage when cleanup never happens.

## Use machine-readable output

Prefer JSON for automation:

- `up --json`: `url`, `host`, `port`, `database`, `expires_at`,
  `already_running`
- `status --json`: `running`, `url`, `port`, `pg_version`, `keep`,
  `expires_at`, `expired`
- `down --json`: `stopped`, `wiped`
- `reset --json`: `reset`, `url`, `port`, `database`
- `url --json`: `url`
- `gc --json`: `reaped` (one object per disposed instance), `examined`,
  `evicted_variants`, `dry_run`
- `cache --json`: `postgres`, `variants`, `instances` (each entry with
  `name`, `size_bytes`, `referenced`), `total_bytes`, `removed`. Only run
  `cache --clean` when the user asks to reclaim disk space.
- `list --json`: `instances` (each with `project_dir`, `status`, `running`,
  `port`, `pg_version`, `database`, `keep`, `expires_at`, `expired`,
  `current`), `count`. Read-only, and never includes connection URLs.
- `run --json`: newline-delimited lifecycle events on stderr; child stdout is
  unchanged

`psql --json` makes wrapper errors machine-readable but leaves psql output
alone. JSON-mode errors are written to stderr. Exit code 10 means `up` adopted
an existing instance; exit code 11 means a requested port is busy; exit code
12 means no running instance was found; exit code 2 is a usage error from a
mistyped invocation. `run` propagates the child's code, including 128 plus
its terminating signal.

Treat `url` and JSON fields containing `url` as secrets because a configured
password may be embedded. Pass connection values through environment
variables; do not echo them into logs, chat, or command arguments.

## Configure only when needed

Create or edit `popgres.toml` only when the user needs reproducible project
settings. Every key is optional:

```toml
pg_version = "18"
database = "db"
password = "change-me"
port = 0
keep = false
seed = "./db/seed.sql"
env_file = ".env.local"
location = "local"
```

Instances live in `.popgres/` inside the project by default; the directory
ignores itself, so never add it to the repository's `.gitignore` or commit
it. Deleting `.popgres/` disposes of the instance data. Set
`location = "global"` for projects inside synced folders (Dropbox, iCloud,
OneDrive) — syncing a live database directory risks corruption.

For vector search, add `extensions = ["vector"]` with `pg_version = "16"`
(pgvector currently ships prebuilt for PostgreSQL 16 only). The extension is
installed and created before the seed hook runs, so seeds and migrations can
use `vector` columns immediately. Changing the extensions of a kept database
requires `popgres reset`.

Use `port = 0` for collision-free allocation. Omit `password` for the default
passwordless loopback-only instance. A `seed` SQL file or shell command runs
only after a fresh initialization. If using `env_file`, ensure it is ignored by
version control; popgres removes its `DATABASE_URL` entry during teardown.

## Inspect and recover

- Use `popgres status --json` before assuming an instance exists.
- Use `popgres psql -- <arguments>` for interactive or one-off `psql` work.
- Use `popgres url` only when a tool cannot inherit the connection environment.
- Use `popgres reset --json` only after confirming that all current data may be
  destroyed.
- Use `popgres down --keep` when stopping while preserving requested data.
- Use `popgres down --wipe` only with explicit authorization to destroy kept
  data.

On failure, report the command and relevant stderr without exposing a
connection URL or password. Attempt cleanup only for an instance created by the
current task.
