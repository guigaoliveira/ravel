# Contributing

Bug reports, focused fixes, documentation, and examples are welcome.

Before opening a pull request, run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

Ravel requires Rust 1.85.0 or newer. See [MSRV.md](MSRV.md) for the toolchain
policy.
