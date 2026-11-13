# `cargo xtask`

| Command | What it runs |
|---|---|
| `cargo xtask fmt` | `cargo fmt --all --check` |
| `cargo xtask clippy` | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| `cargo xtask test` | `cargo test --workspace --locked` |
| `cargo xtask ci` | `fmt`, `clippy`, `container-check`, then `test`. Run it before each push. |
| `cargo xtask container-check` | Checks `Dockerfile`, `.dockerignore`, and `.github/workflows/*.yml` for digest pins, non-root `65532:65532`, `-j 2` Cargo throttling, 40-SHA action pins, Trivy/Cosign gates, and missing-secret skip guards. |
| `cargo xtask coverage [--fail-under N]` | `cargo llvm-cov` over every crate except `xtask`. Fails under N% lines, 85 by default. |
| `cargo xtask verus` | Downloads the pinned Verus release, checks its SHA-256, and verifies `formal/verus`. Set `VERUS` to use a local binary. |
