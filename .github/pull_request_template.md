<!-- Thanks! Two ground rules from CONTRIBUTING.md worth restating up front:
     one focused change per PR, and no AI co-author trailers on commits —
     use whatever tools you want, but you own every line. -->

## What this does

<!-- Plain description, plus the *why* if it isn't obvious from the diff. -->

## How it was verified

<!-- A lot of compositor behavior can't be unit tested. Say what you actually
     ran: nested session? standalone TTY? which client (kitty, mpv, OBS, a
     throwaway test binary)? "cargo test + clippy pass" alone is fine only
     for pure-logic changes. -->

- [ ] `cargo fmt` and `cargo clippy` pass
- [ ] `cargo test` passes
- [ ] Behavior verified live (described above), or this is a pure-logic change

## Anything touching the RAM budget?

<!-- Per-frame allocations, caches, new dependencies. If yes: release-build
     PSS numbers before/after. If no: delete this section. -->
