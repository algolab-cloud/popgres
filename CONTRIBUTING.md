# Contributing to popgres

Thanks for taking the time to help out. popgres is a small Rust CLI, so the
contribution loop is short: clone, `cargo run -- up`, change something, open a PR.

## Ground rules

- Be kind. See the [Code of Conduct](CODE_OF_CONDUCT.md).
- By contributing, you agree your work is licensed under the [MIT License](LICENSE).
- Open an issue before starting anything large (a new subcommand, a new dependency,
  a change to the on-disk state format). Small fixes can go straight to a PR.

## Getting set up

You need a recent stable Rust toolchain (the project is developed on 1.92, edition 2021):

```sh
rustup toolchain install stable
rustup component add rustfmt clippy
```

Then:

```sh
cargo build
cargo run -- up        # first ever run downloads PostgreSQL (~30 MB)
cargo run -- status
cargo run -- down
```

The first `up` fetches real PostgreSQL binaries from
[theseus-rs/postgresql-binaries](https://github.com/theseus-rs/postgresql-binaries)
and caches them globally, so it needs network access once per Postgres version.
If you hit GitHub API rate limits, export a `GITHUB_TOKEN`.

If a run leaves something behind (a killed process, a stale state file), the state
lives under your platform data dir — `~/.local/share/popgres/<project-hash>/` on
Linux, `~/Library/Application Support/popgres/<project-hash>/` on macOS. Deleting
that directory is always a safe reset.

## Before you push

Run the same three things CI runs:

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```

Please keep the working tree free of `cargo fmt` noise unrelated to your change.

## Code conventions

- `src/main.rs` holds the CLI surface (clap `Command` enum) and command handlers;
  `src/state.rs` owns the per-project state file and paths. Keep new persistence
  logic in `state.rs` rather than inlining paths in command handlers.
- Errors use `anyhow` — attach context with `.context("what we were trying to do")`
  instead of bubbling bare I/O errors.
- Every user-facing command must support `--json`, and the JSON must stay stable:
  add fields, don't rename or remove them. Human output goes to stdout as plain
  text; JSON errors go to stderr.
- Commands are non-interactive. No prompts, no confirmations — agents and CI are
  first-class users.
- Doc comments on clap fields *are* the help text. Write them for someone reading
  `popgres --help` for the first time.

## Changing on-disk state

The state file format is a compatibility surface: a newer popgres may find an
instance started by an older one. If you change it, make the change additive and
handle a missing field gracefully, or bump a version marker and describe the
migration in your PR.

## Pull requests

- One logical change per PR; keep the diff reviewable.
- Write a PR description that says what changed and how you verified it — the
  actual commands you ran count as verification.
- Update `README.md` when you change user-visible behavior, and `PLAN.md` when you
  land (or reshape) a roadmap item.
- Rebase on `main` rather than merging it in.

## Reporting bugs

Open an issue with your OS and architecture, `popgres --version`, the exact command,
and the output (add `--json` if it's a command that supports it). If Postgres itself
failed to start, the instance's log file under the project state dir is the useful
part.

## Security

Don't file security issues publicly — see [SECURITY.md](SECURITY.md).
