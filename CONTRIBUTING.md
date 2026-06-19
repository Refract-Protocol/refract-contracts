# Contributing to Refract Contracts

Thanks for your interest in improving Refract! This repo holds the Soroban
contracts, so contributions here touch protocol-critical code — we keep the bar
high, but we'll help you get there.

## Ground rules

- Be respectful. We follow the [Code of Conduct](./CODE_OF_CONDUCT.md).
- Discuss non-trivial changes in an issue **before** opening a PR.
- Every behavioural change needs a test.
- Security-sensitive findings go to the process in [`SECURITY.md`](./SECURITY.md), **not** a public issue.

## Getting set up

```bash
rustup target add wasm32-unknown-unknown
cargo test          # should pass on a clean checkout
```

## Development workflow

1. Fork and create a branch: `git checkout -b feat/short-description`.
2. Make your change, with tests alongside it (see `*/src/test.rs`).
3. Run the full local gate — CI runs exactly this:
   ```bash
   cargo fmt --all --check
   cargo clippy --all-targets -- -D warnings
   cargo test --all
   cargo build --target wasm32-unknown-unknown --release
   ```
4. Open a PR against `main` and fill in the template.

## Coding standards

- Code is `#![no_std]`; **never** introduce floating-point arithmetic. Use 1e7
  fixed-point integers (`i128`) consistently.
- Prefer typed `#[contracterror]` returns over `panic!` for recoverable errors
  on the pool's public surface. (The registry/oracle currently panic on
  misuse — converging these is a welcome contribution.)
- Authorize every state-changing entrypoint with `require_auth()` and verify the
  authorized principal.
- Emit an event for every state transition.
- Keep storage keys in the `DataKey` enum; document the value type in a comment.

## What we'd love help with

See the issue tracker and [`ROADMAP`](https://github.com/refract-protocol) for
open items. Good first contributions:

- Property/fuzz tests for premium and share-price math.
- Converting registry/oracle `panic!`s to typed errors.
- Wiring `RefractPool.buy_policy` to call `RefractPolicyRegistry.register_policy`.
- An `expire_policy` sweep that frees coverage from lapsed policies.

## Commit & PR conventions

- Use [Conventional Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `test:`, `docs:`, `refactor:`, `chore:`.
- Keep PRs focused and reasonably small. Reference the issue they close.
