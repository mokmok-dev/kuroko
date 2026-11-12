# kuroko agent guide

kuroko is a daemon that receives Linear webhooks for issues delegated to AI agents and dispatches them to local coding agents over stdio or websocket.

## Commands

Run everything through the flake. direnv loads the devshell in interactive shells. Non-interactive shells need `nix develop --command <cmd>`.

- `nix flake check` is the full gate. It runs clippy `--all-targets -- --deny warnings`, doctest, nextest, treefmt, and pre-commit with deadnix and statix.
- `nix develop --command cargo nextest run` runs the tests locally.
- `nix develop --command cargo mutants --jobs 2 --iterate` runs the mutation gate. Everything under `crates/` must be 100% killed. `apps/` is exempt. CI runs the same gate in `mutants.yaml`, because a full mutation run takes far longer than every other check together.

The flake source contains only committed files. A new file that is not committed is invisible to `nix flake check`, so commit before trusting a check that should see new files.

## Rust conventions

- Edition 2024. Workspace lints live in the root `Cargo.toml` and clippy warnings are CI errors. No `unsafe`, and no `unwrap`, `expect`, `panic!`, or `todo!` in production code.
- Test files may start with `#![allow(clippy::unwrap_used, clippy::expect_used)]`. The `payment` repo uses the same allowance.
- Comments carry only a non-obvious *why*. Code says how, tests say what, the commit message says why.
- Commits follow Conventional Commits. The subject is short and imperative. The body explains why, not what.

## Architecture rules

- The task lifecycle is one pure transition function. The async runtime and the stateright model both call it. Do not fork the logic between model and runtime.
- Wire types, such as Linear webhook JSON and websocket frames, are parsed at crate boundaries into domain types. The engine consumes domain types only.
- `crates/` holds all logic and must survive `cargo mutants` at 100% killed. Keep I/O thin and push untestable wiring into `apps/`.

## Resource fence

Cargo builds, tests, and mutation runs are resource-intensive. Run them inside a transient systemd user service with memory bounds, never as bare child processes:

```
systemd-run --user --pipe --unit=kuroko-<purpose> --same-dir \
  -p MemoryAccounting=yes -p MemoryHigh=24G -p MemoryMax=32G \
  -p MemorySwapMax=4G -p OOMPolicy=kill \
  bash -lc 'nix develop --command cargo nextest run'
```

Use a distinct unit name per invocation. `cargo mutants` starts at `--jobs 2`. A cgroup OOM is an infrastructure failure, not a test result.
