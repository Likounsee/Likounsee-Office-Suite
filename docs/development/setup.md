# Development setup

Requirements: Rust stable, rustfmt, Clippy and Git. Future GUI dependencies will
be documented after framework selection.

Run from the repository root:

```sh
rustc --version
cargo --version
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace --all-targets
```

Run a foundation executable with `cargo run -p document-editor`,
`cargo run -p spreadsheet` or `cargo run -p presentation`.
Set `RUST_LOG=debug` for verbose console logs.
