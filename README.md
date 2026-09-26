# popgres

Disposable PostgreSQL for local development and tests. No Docker or system-wide
Postgres installation required.

```sh
npx @popgres/cli run -- npm test
```

Popgres starts a real PostgreSQL instance, sets `DATABASE_URL` and the standard
`PG*` variables for your command, then stops and wipes the database when the
command exits. PostgreSQL binaries are downloaded on first use and cached.

## Features

- **Real PostgreSQL.** Test against the same database engine you deploy, not an
  in-memory substitute.
- **No Docker daemon.** Popgres downloads a platform binary once and runs it as
  an ordinary local process.
- **Scoped cleanup.** `run` owns the database lifecycle, including command
  failures and signals: Ctrl-C or SIGTERM never leaves a database behind.
- **Fast fresh starts.** The [seed cache](#seed-cache) turns initdb plus
  your seed into a copy of a ready-made database, so a fresh `run` takes
  about half a second.
- **Tuned for tests.** [`fast = true`](#faster-tests) drops durability you
  don't need for throwaway data, and `[settings]` takes any server setting.
- **Isolated parallel tests.** [`popgres testdb`](#test-databases-for-parallel-workers)
  clones a private, fully seeded database per test worker in about 0.1 s.
- **Extensions included.** The ~46 contrib [extensions](#extensions) cost
  nothing extra, and pgvector is one line of config.
- **Safe unattended use.** JSON output, stable exit codes, per-project locking,
  verified liveness, optional TTLs, and global garbage collection support CI
  jobs and [AI agents](#ai-agents).

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
| `popgres testdb` | Clone a disposable test database from the seeded template |
| `popgres reset` | Recreate the database from its seed (fast when the seed is unchanged) |
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

`run` never leaves a database behind on a signal. Ctrl-C (or SIGTERM or
SIGHUP) while the database is still starting lets startup finish, tears it
down, and exits `128 + signal` without running the command; a second signal
exits at once. Once the command is running, a SIGTERM sent to popgres is
passed on to it, and the command gets 10 seconds to exit before it is killed —
or none, if you interrupt again.

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
fast = true              # trade durability for speed (see below)

[settings]               # any PostgreSQL server setting
max_connections = 200
log_statement = "all"
```

### Seeding

`seed` runs once after each fresh initialization, never over data resumed
with `keep = true`. It is either a `.sql` file, run through the instance's
own `psql`, or a shell command run from the project root with
`DATABASE_URL` and the `PG*` variables set:

```toml
seed = "db/seed.sql"
# or
seed = "npx prisma migrate reset --force --skip-generate"
```

The seed runs against the template database, which popgres then locks and
clones your working database from. Everything it creates reaches every
`testdb` clone, and `reset` can rebuild in a clone's time. A failed seed
leaves `up`'s instance running so you can inspect it; `run` tears it down.

### Faster tests

`fast = true` turns off `fsync`, `synchronous_commit` and
`full_page_writes`. A disposable database doesn't need crash durability, and
write-heavy suites often run noticeably faster without it. Don't combine it
with `keep = true` for data you care about: a crash can corrupt it.

`[settings]` takes any PostgreSQL server setting and wins over `fast`.
Settings are applied on every start, so a change takes effect the next time
the instance starts, including when a kept instance resumes.

### Seed cache

initdb is most of a fresh start, and a seed can take much longer. After a
fresh instance is initialized and seeded, popgres keeps a copy of it. The
next fresh start with the same inputs copies it into place instead: no
initdb, no seed. A `run` then takes a fraction of a second.

The copy is keyed by everything that shapes it: the PostgreSQL version,
extensions, password, settings, and the seed. A `.sql` seed is hashed by
content, so editing it invalidates the cache. A command seed (`seed = "npm
run db:setup"`) reads files popgres can't guess, so it is cached only when
you list them:

```toml
seed = "npm run db:setup"
seed_inputs = ["db/migrations", "db/seeds", "package-lock.json"]
```

Directories are hashed recursively. If a `.sql` seed includes other files
(`\i`), list those too. `seed_cache = false` turns the cache off. `popgres
reset` benefits as well: when the seed hasn't changed, it re-clones the
seeded template instead of running the seed again.

Each project keeps its three most recent entries. `popgres gc` evicts
entries unused for a week, and `popgres cache --clean` removes any not used
in the last hour.

## Test databases for parallel workers

A fresh instance seeds a *template* database, locks it, and clones your
working database from it. `popgres testdb` clones it again — a private,
fully seeded database in about a tenth of a second:

```sh
DATABASE_URL=$(popgres testdb)   # one per test worker
popgres testdb --clean           # drop every generated clone
```

Give each parallel test worker (`jest -w`, `pytest -n`) its own clone in a
global setup hook and they stop colliding on shared rows and truncations.
Clones are real databases: use transaction rollback or truncation *within* a
worker as usual. `--name worker_1` names a clone; named clones are yours to
drop. Because the working database is itself a clone of the template,
`popgres reset` on a running instance is also fast: it re-clones in well
under a second, on the same port, re-running your seed only if it changed.

## Extensions

Declare PostgreSQL extensions in `popgres.toml` and popgres creates them
before your seed runs — so seeds, migrations, and every `testdb` clone find
them ready:

```toml
extensions = ["pg_trgm", "uuid-ossp", "pgcrypto"]
```

**The ~46 contrib extensions ship inside the PostgreSQL binaries popgres
already downloads** — `pg_trgm`, `hstore`, `pgcrypto`, `citext`,
`uuid-ossp`, `pg_stat_statements`, `ltree`, `cube`, `btree_gin`,
`postgres_fdw`, and the rest of the standard set. Listing them costs
nothing: no download, no extra disk.

Downloaded extensions are also available — currently `vector` (pgvector,
prebuilt for PostgreSQL 16) and `vectors` (pgvecto.rs):

```toml
pg_version = "16"
extensions = ["vector", "pg_trgm"]
```

```sh
popgres psql -- -c "SELECT '[1,2,3]'::vector <-> '[2,2,2]';"
```

The pristine PostgreSQL install is never modified. A project with downloaded
extensions runs from a *variant* — an immutable copy of the base with the
extensions installed — stored globally, built once per
version-and-extension combination, and shared read-only by every project
that wants the same one (building takes seconds; reuse is instant).
Contrib-only projects skip variants entirely. `popgres gc` evicts variants
no instance references anymore.

Downloaded versions can be pinned with `[extensions_versions]` and follow
each source repository's own numbering; contrib versions follow
`pg_version`. Changing the extensions of a `keep = true` database requires
`popgres reset`, and popgres says so rather than letting the postmaster
fail. Extensions that no one publishes portable prebuilt binaries for
(PostGIS, TimescaleDB, pg_cron) are not available; the error for an unknown
name says what is.

## Disk usage

`popgres cache` shows everything popgres keeps on disk — PostgreSQL versions,
extension variants, and each instance — with what is in use and what is not.
`popgres cache --clean` removes unused extension variants and cached seeded
databases (anything used in the last hour is spared, in case a start is
picking it up); adding `--all`
also removes PostgreSQL versions no popgres instance references (the download
cache may be shared with other tools built on postgresql-embedded, so this
step is opt-in). Instance data is never touched — that is what `down` and
`gc` are for.

## Continuous integration

`popgres run` needs nothing but the binary, so it drops into any CI job. To
make fresh starts fast there too, cache the PostgreSQL download and
popgres's seed cache between runs. On a GitHub Actions Linux runner:

```yaml
- uses: actions/cache@v4
  with:
    path: |
      ~/.theseus/postgresql
      ~/.local/share/popgres/seeds
      ~/.local/share/popgres/variants
    key: popgres-${{ runner.os }}-${{ hashFiles('popgres.toml', 'db/**') }}
    restore-keys: popgres-${{ runner.os }}-
- run: npx @popgres/cli run -- npm test
```

Without the cache, each job downloads PostgreSQL once and pays for initdb
and the seed as usual. For a database shared by several steps, start it with
`popgres up --ttl 30m` and finish with `popgres down` (or `popgres gc`), so
a cancelled job can't leave it running.

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
`password` in `popgres.toml` when authentication is required. Popgres passes
it to its own `psql` calls through `PGPASSWORD`, never on the command line
where other users could see it, and percent-encodes it in connection URLs.
Keep configured environment files out of version control.

## Troubleshooting

**`error while loading shared libraries: libxml2.so.2`.** The official
PostgreSQL builds link against libxml2 2.13 or older. Distributions that
ship libxml2 2.14+ (such as Arch Linux) no longer provide `libxml2.so.2`.
Install your distribution's libxml2 compatibility package (on Arch,
`libxml2-legacy` from the AUR), then run the command again.

**`the instance is already running … but … was requested`.** `up` and `run`
reuse a running instance and won't quietly hand back one with a different
`--pg` or `--port`. Stop it with `popgres down` first.

**`waiting for another popgres process to finish`.** Another popgres command
holds this project's lock, often while downloading PostgreSQL or running a
seed. It continues on its own once the lock is released.

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
