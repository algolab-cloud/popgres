# popgres

Disposable Postgres for local development, with no system install or Docker.

```sh
npm install --save-dev @popgres/cli
npx popgres run -- npm run dev
```

For a zero-install one-off command:

```sh
npx @popgres/cli up
```

This package installs the prebuilt `popgres` binary for the current platform as
an optional dependency and launches it directly. It runs no install scripts and
does not require a Rust toolchain. Prebuilt packages support macOS on Apple
Silicon and Intel, Linux glibc on ARM64 and x64, and Windows on x64.

See the [project README](https://github.com/algolab-cloud/popgres#readme) for
commands and configuration, or download standalone binaries from
[GitHub Releases](https://github.com/algolab-cloud/popgres/releases).
