# popgres

Disposable PostgreSQL for local development and tests. No Docker or system-wide
Postgres installation required.

```sh
npx @popgres/cli run -- npm test
```

Popgres starts a real PostgreSQL instance, sets `DATABASE_URL` and the standard
`PG*` variables for your command, then stops and wipes the database when the
command exits. PostgreSQL binaries are downloaded on first use and cached.

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

Or install with Cargo:

```sh
cargo install popgres
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
| `popgres gc` | Dispose of instances past their `--ttl` |

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
popgres gc            # stops everything past its deadline, in every project
```

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
```

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
- [Roadmap](PLAN.md)
- [Contributing](CONTRIBUTING.md)
- [Security](SECURITY.md)

## License

[MIT](LICENSE)
