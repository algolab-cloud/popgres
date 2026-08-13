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

Use `--json` with `up`, `status`, `reset`, and `down` for automation.

## Configuration

Popgres works without configuration. Add `popgres.toml` at the project root
when you need explicit settings:

```toml
pg_version = "18"        # default: latest stable
database = "db"          # default: db
port = 0                 # choose a free port
keep = false             # wipe data when stopped
seed = "./db/seed.sql"   # run after fresh initialization
env_file = ".env.local"  # write DATABASE_URL while running
```

The default instance is passwordless and listens only on loopback. Set
`password` in `popgres.toml` when authentication is required. Keep configured
environment files out of version control.

## AI agents

Use the reusable [popgres agent skill](skills/popgres/SKILL.md) to help coding
agents provision databases, run migrations and tests, protect connection
details, preserve existing instances, and clean up safely.

## Links

- [npm](https://www.npmjs.com/package/@popgres/cli)
- [crates.io](https://crates.io/crates/popgres)
- [Roadmap](PLAN.md)
- [Contributing](CONTRIBUTING.md)
- [Security](SECURITY.md)

## License

[MIT](LICENSE)
