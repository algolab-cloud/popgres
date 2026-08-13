## What changed

<!-- One or two sentences. Link the issue if there is one: Fixes #123 -->

## How it was verified

<!-- The commands you actually ran, e.g. cargo run -- up / status / down, and what you saw. -->

## Checklist

- [ ] `cargo fmt --all` is clean
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test` passes
- [ ] User-visible changes are reflected in `README.md` / `--help` text
- [ ] `--json` output only gained fields (no renames or removals)
- [ ] On-disk state changes are backward compatible, or the migration is described above
