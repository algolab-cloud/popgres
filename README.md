# popgres

Disposable PostgreSQL for local development and tests. No Docker or system-wide
Postgres installation required.

```sh
npx @popgres/cli run -- npm test
```

Popgres starts a real PostgreSQL instance, sets `DATABASE_URL` and the standard
`PG*` variables for your command, then stops and wipes the database when the
command exits. PostgreSQL binaries are downloaded on first use and cached.

## Why popgres

- **Real PostgreSQL.** Test against the same database engine you deploy, not an
  in-memory substitute.
- **No Docker daemon.** Popgres downloads a platform binary once and runs it as
  an ordinary local process.
- **Scoped cleanup.** `run` owns the database lifecycle, including command
  failures and signals.
- **Safe unattended use.** JSON output, stable exit codes, per-project locking,
  verified liveness, optional TTLs, and global garbage collection support CI
  jobs and AI agents.

## Install

Run without installing:

```sh
npx @popgres/cli up
```

Install in a Node.js project:

```sh
npm install --save-dev @popgres/cli
npx popgres run -- npm run dev
```

Or install with Cargo (requires Rust 1.94 or newer):

```sh
cargo install popgres
popgres run -- cargo test
```

Prebuilt npm binaries support macOS ARM64/x64, Linux glibc ARM64/x64, and
Windows x64. Standalone archives are available on
[GitHub Releases](https://github.com/algolab-cloud/popgres/releases).

## Commands

| Command | Purpose |
| --- | --- |
| `popgres run -- <command>` | Run a command with a disposable database |
| `popgres up` | Start this project's database |
| `popgres status` | Show its status, version, and port |
| `popgres url` | Print its connection URL |
| `popgres psql` | Open a `psql` shell |
| `popgres reset` | Wipe and recreate the database |
| `popgres down` | Stop and wipe the database |
| `popgres down --keep` | Stop and preserve its data |
| `popgres list` | List every instance on this machine |
| `popgres gc` | Dispose of instances past their `--ttl` |
| `popgres cache` | Show disk usage; `--clean` reclaims unused items |

Every command accepts `--json`. `run --json` keeps the child command's stdout
untouched and writes newline-delimited lifecycle events to stderr. `psql
--json` changes wrapper errors only; the interactive client still owns its
output.

Automation can rely on these exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Command completed successfully |
| `1` | Popgres failed |
| `10` | `up` found the instance already running |
| `11` | A requested port is already in use |
| `12` | `status` found no running instance |

Coded exits start at 10 so they can never be confused with a usage error,
which exits `2`. `run` returns the child command's exit code instead,
including `128 + signal` when the child is terminated by a signal.

## Expiring instances

An instance started with a deadline becomes eligible for disposal even if
whoever started it never comes back:

```sh
popgres up --ttl 30m
popgres list          # every instance on this machine, and what is expiring
popgres gc --dry-run  # report what has expired, touching nothing
popgres gc            # stops everything past its deadline, in every project
```

`popgres list` is read-only — it starts, stops, and creates nothing — and shows
each instance's status, port, version, remaining TTL, and project, marking the
current project with `*`. Connection URLs are deliberately omitted because it
spans every project; use `popgres url` for this one.

Deadlines are opt-in: without `--ttl` (or `ttl` in `popgres.toml`) an instance
lives until it is stopped. `up` replaces this project's own expired instance
rather than handing it back, and `gc` is the only command that touches other
projects — it never destroys anything that has not expired. An expired
instance configured with `keep = true` has its server stopped but its data
preserved.

Run `popgres gc` periodically from a cron job, a CI cleanup step, or an agent's
teardown to guarantee expired instances are not left behind.

## Configuration

Popgres works without configuration. Add `popgres.toml` at the project root
when you need explicit settings:

```toml
pg_version = "18"        # default: latest stable
database = "db"          # default: db
port = 0                 # choose a free port
keep = false             # wipe data when stopped
ttl = "30m"              # dispose of the instance after this long
seed = "./db/seed.sql"   # run after fresh initialization
env_file = ".env.local"  # write DATABASE_URL while running
location = "local"       # or "global": keep the project tree free of db files
```

## Extensions

Declare PostgreSQL extensions in `popgres.toml` and popgres installs and
creates them before your seed runs:

```toml
pg_version = "16"        # pgvector currently ships prebuilt for PostgreSQL 16
extensions = ["vector"]
```

```sh
popgres psql -- -c "SELECT '[1,2,3]'::vector <-> '[2,2,2]';"
```

The pristine PostgreSQL install is never modified. A project with extensions
runs from a *variant* — an immutable copy of the base with the extensions
installed — stored globally, built once per version-and-extension
combination, and shared read-only by every project that wants the same one
(building takes seconds; reuse is instant). `popgres gc` evicts variants no
instance references anymore.

Available extensions: `vector` (pgvector) and `vectors` (pgvecto.rs).
Versions can be pinned with `[extensions_versions]` and follow each source
repository's own numbering. Changing the extensions of a `keep = true`
database requires `popgres reset`, and popgres says so rather than letting
the postmaster fail.

## Disk usage

`popgres cache` shows everything popgres keeps on disk — PostgreSQL versions,
extension variants, and each instance — with what is in use and what is not.
`popgres cache --clean` removes unused extension variants; adding `--all`
also removes PostgreSQL versions no popgres instance references (the download
cache may be shared with other tools built on postgresql-embedded, so this
step is opt-in). Instance data is never touched — that is what `down` and
`gc` are for.

## Where the database lives

By default the instance lives in `.popgres/` inside the project, like `.git`
or `node_modules`: delete the project and its database is gone with it. The
directory ignores itself (its own `.gitignore`) and carries a `CACHEDIR.TAG`
so backup tools skip it — nothing to add to your repository.

Set `location = "global"` to keep the instance in the per-user data directory
instead. Do this when the project lives in a synced folder (Dropbox, iCloud,
OneDrive): syncing a live database directory risks corruption. Projects with
an existing global instance keep using it until it is wiped; the next fresh
start is local.

The default instance is passwordless and listens only on loopback. Set
`password` in `popgres.toml` when authentication is required. Keep configured
environment files out of version control.

## AI agents

The reusable [popgres agent skill](skills/popgres/SKILL.md) works with Claude
Code, Codex, Cursor, Gemini CLI, GitHub Copilot, OpenCode, and other tools that
support the portable Agent Skills format. It helps agents provision databases,
run migrations and tests, protect connection details, preserve existing
instances, and clean up safely.

Install it for every supported agent, including Claude Code:

```sh
npx skills add algolab-cloud/popgres --skill popgres --agent '*'
```

Add `--global` to make the skill available across all projects.

## Links

- [npm](https://www.npmjs.com/package/@popgres/cli)
- [crates.io](https://crates.io/crates/popgres)
- [Changelog](CHANGELOG.md)
- [Roadmap](PLAN.md)
- [Contributing](CONTRIBUTING.md)
- [Security](SECURITY.md)

## License

[MIT](LICENSE)
