# Contributing

This project is built with Rust. The minimum Rust version is 1.99.1.
A debug build is `cargo build --workspace --locked`.
Read `AGENTS.md` before changing code.

Before a change is done, the checks in the Verify section of `AGENTS.md`
must all pass.

Read the ADRs a change touches before changing code. A change that
reverses a decision changes the ADR first, in its own docs-only commit.

Planned work lives in `docs/backlog/`, one file per item. A pull request
says which ADRs it touches.
