# Rust support (MSRV)

MSRV means **minimum supported Rust version**. Ravel currently requires Rust
1.85.0 or newer.

CI and local development use the stable channel from `rust-toolchain.toml`.
The committed lockfile keeps builds reproducible while dependency updates
remain explicit.
