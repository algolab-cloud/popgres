# popgres

Real disposable PostgreSQL for local development and tests. No Docker, no
system-wide Postgres installation, and no Rust toolchain required.

```sh
npx @popgres/cli run -- npm test
```

Popgres starts PostgreSQL, injects `DATABASE_URL` and the standard `PG*`
variables into your command, forwards its exit code, then stops and wipes the
database. PostgreSQL binaries are downloaded once and cached.

## Install

```sh
npm install --save-dev @popgres/cli
npx popgres run -- npm run dev
```

For a zero-install one-off command:

```sh
npx @popgres/cli up
```

## Common commands

| Command | Purpose |
| --- | --- |
| `popgres run -- <command>` | Run a command with a disposable database |
| `popgres up` | Start this project's database |
| `popgres status` | Show status, version, port, and expiry |
| `popgres url` | Print the connection URL |
| `popgres psql` | Open a `psql` shell |
| `popgres testdb` | Clone a disposable database from the seeded template |
| `popgres reset` | Wipe and recreate the database |
| `popgres down` | Stop and wipe the database |
| `popgres down --keep` | Stop and preserve its data |
| `popgres list` | List every instance on this machine |
| `popgres gc` | Dispose of instances past their TTL |
| `popgres cache` | Show disk usage and reclaim unused cache entries |

For a database shared by several commands, give it an expiry deadline so an
interrupted CI job or agent session cannot leave it running indefinitely:

```sh
npx popgres up --ttl 30m
npx popgres gc --dry-run   # report what has expired, touching nothing
npx popgres gc
```

Every command accepts `--json` for automation. Popgres also provides stable
exit codes, serializes concurrent lifecycle changes, and verifies postmaster
identity before adopting or wiping an instance.

## Extensions included

Declare extensions once in `popgres.toml`; popgres creates them before the
seed runs, and every working or test database inherits them:

```toml
extensions = ["pg_trgm", "uuid-ossp", "pgcrypto"]
```

The standard PostgreSQL contrib set—around 46 extensions including `hstore`,
`citext`, `ltree`, `cube`, `btree_gin`, and `postgres_fdw`—ships inside the
PostgreSQL binaries already downloaded by popgres. There is no additional
download or disk cost. Popgres also supports prebuilt downloaded extensions,
including pgvector:

```toml
pg_version = "16"
extensions = ["vector", "pg_trgm"]
```

## Isolated parallel test databases

A fresh instance seeds a locked template. Create one private, fully seeded
clone per parallel test worker in about a tenth of a second:

```sh
DATABASE_URL=$(npx popgres testdb)
npx popgres testdb --clean
```

The working database is also cloned from that template, so extensions and
seed data are identical everywhere. A running `popgres reset` rebuilds it on
the same port without repeating a full PostgreSQL initialization.

By default instance data lives in the project's self-ignoring `.popgres/`
directory. Set `location = "global"` for projects in synced folders.

## How the npm package works

This package installs the prebuilt `popgres` binary for the current platform as
an optional dependency and launches it directly. It runs no install scripts and
does not require a Rust toolchain. Prebuilt packages support macOS on Apple
Silicon and Intel, Linux glibc on ARM64 and x64, and Windows on x64.

See the [project README](https://github.com/algolab-cloud/popgres#readme) for
configuration, seeding, agent-skill installation, and the complete automation
contract. Standalone binaries are available from
[GitHub Releases](https://github.com/algolab-cloud/popgres/releases).
