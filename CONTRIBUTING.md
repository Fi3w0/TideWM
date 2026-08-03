# Contributing to TideWM

TideWM is a solo/free-time project for now. It's not actively soliciting contributors yet, but it's built to eventually take them. These are the ground rules for when it does (and for anyone poking at a fork in the meantime).

## Getting Started

1. Fork and clone the repo.
2. Install a Rust stable toolchain, 1.86 or newer (`rustup default stable`) plus `rust-src`, `clippy`, `rustfmt`. See TECHNICAL_REPORT.md's "Building" section for how the MSRV was verified.
3. Make sure your system has `pkg-config` and the native libs TECHNICAL_REPORT.md's "Building" section lists, mapped to the exact Smithay features TideWM enables.

## Build

```bash
cargo build --release --locked   # what CI and releases actually build
cargo run --locked                # runs nested inside your current session
```

`--locked` keeps everyone building against the exact versions in the committed `Cargo.lock` -- don't `cargo update` as a side effect of an unrelated PR.

## Development Workflow

- Open an issue before starting anything non-trivial. Saves everyone a rewrite.
- Keep PRs focused. One feature or fix per PR, not a grab-bag.
- Run `cargo fmt` and `cargo clippy` before pushing; CI will reject anything that fails either.

## Code Conventions

- **Match the surrounding module's style** rather than introducing a new pattern for the same problem.
- **No AI co-author trailers on commits. Author must be a human.** Use AI tools if you want, but you own and understand every line you submit.
- **Small, focused commits.** One logical change per commit, plain imperative subject line (no `feat:`/`fix:` prefixes). Add a short body explaining *why*, when the *why* isn't obvious from the diff.
- **Mind the RAM budget.** 1.5GB is the real target ceiling for normal use, not just a number to approach; 3GB is the line where active optimization has to start, not an acceptable resting place. Avoid per-frame allocations, unbounded caches, or dependencies that drag in a large runtime for a small feature. When actually measuring, use a `--release` build and PSS (`grep Pss /proc/<pid>/smaps_rollup`), not a debug build's raw RSS -- a debug binary and its shared driver stack can dwarf the real marginal cost of a change.
- **No `unsafe` without a comment justifying it** and, ideally, a narrower safe wrapper around it.

## Testing

- `cargo test` for anything with unit-testable logic.
- For compositor behavior, describe how you manually verified it (nested run, specific client tested against) in the PR description — a lot of Wayland behavior can't be unit tested meaningfully.

## Pull Requests

- Describe *what* changed and *why*, not just a restatement of the diff.
- Note any RAM/perf impact if the change touches rendering, animation, or anything per-frame.
- Update `README.md` / `TECHNICAL_REPORT.md` / `CHANGELOG.md` / `DOCUMENTATION.md` if the change affects features, build steps, or config. See [SECURITY.md](SECURITY.md) instead if it's a security-relevant change.
