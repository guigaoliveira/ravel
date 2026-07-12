# Rust support

Ravel pins its CI and local development toolchain in `rust-toolchain.toml` and
uses the same `stable` channel in CI. The minimum supported Rust version is
1.85.0. The lockfile is committed so a checkout is reproducible while
dependency updates remain explicit.
