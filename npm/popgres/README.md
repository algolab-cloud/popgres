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
| `popgres reset` | Wipe and recreate the database |
| `popgres down` | Stop and wipe the database |
| `popgres down --keep` | Stop and preserve its data |
| `popgres list` | List every instance on this machine |
| `popgres gc` | Dispose of instances past their TTL |

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

## How the npm package works

This package installs the prebuilt `popgres` binary for the current platform as
an optional dependency and launches it directly. It runs no install scripts and
does not require a Rust toolchain. Prebuilt packages support macOS on Apple
Silicon and Intel, Linux glibc on ARM64 and x64, and Windows on x64.

See the [project README](https://github.com/algolab-cloud/popgres#readme) for
configuration, seeding, agent-skill installation, and the complete automation
contract. Standalone binaries are available from
[GitHub Releases](https://github.com/algolab-cloud/popgres/releases).
