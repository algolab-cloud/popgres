# popgres

Disposable Postgres for local development, with no system install or Docker.

```sh
npx popgres run -- npm run dev
```

This package installs the prebuilt `popgres` binary for the current platform as
an optional dependency and launches it directly. It runs no install scripts and
does not require a Rust toolchain.

See the [project README](https://github.com/ericmaro/popgres#readme) for commands,
configuration, and source installation instructions.
