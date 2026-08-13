# popgres

Disposable Postgres for every project — pops up when you start, pops away when you stop.

No system install. No Docker. Real PostgreSQL binaries are downloaded once (per version), cached globally, and run as a plain local process on a free port.

```
popgres run -- npm run dev   # DB lives exactly as long as your dev process
popgres up                   # start this project's Postgres, prints DATABASE_URL
popgres status               # running? which version, which port?
popgres url                  # print the connection string
popgres psql                 # open a psql shell into the instance
popgres reset                # wipe the data and start fresh
popgres down                 # stop and wipe — poof!
popgres down --keep          # stop but keep the data for next time
popgres down --wipe          # stop and wipe even if it was set to keep
```

`popgres run` is the one to reach for: it starts Postgres, hands your command a
`DATABASE_URL` (plus `PGHOST`/`PGPORT`/`PGUSER`/`PGPASSWORD`/`PGDATABASE`), and
disposes of the database when the command exits — Ctrl-C included. It exits with
your command's exit code, so it drops into a `package.json` script or CI job as-is:

```json
{ "scripts": { "dev": "popgres run -- vite dev" } }
```

Most commands support `--json` for scripts and AI agents.

## Configuring a project

Everything works with no config at all. Drop a `popgres.toml` in your project root
when you want to pin things down — every key is optional, and a command-line flag
always wins over the file:

```toml
pg_version = "18"        # default: latest stable
database = "db"          # created on first start
password = "hunter2"     # omit for no password at all (the default)
port = 0                 # 0 = pick a free port each run
keep = false             # persist data between runs
seed = "./db/seed.sql"   # run once, after a fresh initdb
env_file = ".env.local"  # write DATABASE_URL here while the instance is up
```

By default the database is called `db` and takes **no password**, so the URL is
just `postgresql://postgres@127.0.0.1:<port>/db` — nothing secret to leak into a
log, a screenshot, or an agent's context. The server listens on loopback only and
dies with the project. Set `password` if you want authentication anyway; popgres
then requires it for real (`database` and `password` apply to a fresh database —
an existing one keeps the name and credentials it was created with).

`seed` is either a `.sql` file (fed to psql, stopping on the first error) or any
shell command — `"npm run migrate"`, `"sqlx migrate run"` — which gets the same
environment as `popgres run`. It runs only on a genuinely fresh database, never
over data you resumed with `--keep`.

`env_file` is rewritten in place: popgres replaces the `DATABASE_URL` line, leaves
your other variables alone, and removes the line again on `down`. The file holds a
password, so popgres creates it `0600` — keep it out of version control.

popgres identifies a project by its root — the nearest directory up the tree with a
`popgres.toml`, or failing that a `.git`. So `popgres up` in the repo root and
`popgres url` three directories down talk about the same instance.

## Try it (from source)

```
cargo run -- up
cargo run -- status
cargo run -- psql -- -c "select version()"
cargo run -- down
```

The first ever `up` downloads PostgreSQL (~30 MB) from
[theseus-rs/postgresql-binaries](https://github.com/theseus-rs/postgresql-binaries);
every start after that is seconds. If you hit GitHub API rate limits, set a
`GITHUB_TOKEN` environment variable.

## Roadmap

See [PLAN.md](PLAN.md) — including `popgres run -- <cmd>` (DB lives exactly as
long as your dev process), seeding, TTL auto-disposal for agents, MCP server
mode, and npm/npx distribution.

## Contributing

Bug reports and PRs are welcome — start with [CONTRIBUTING.md](CONTRIBUTING.md).
Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).
Found a security issue? See [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
