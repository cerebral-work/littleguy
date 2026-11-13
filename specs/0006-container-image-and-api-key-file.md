---
status: implemented
created: 2026-11-13
tags: [littleguy, container, docker, harbor, pool, secret-file, littleman]
priority: high
depends_on:
  - 0005-lg-pool-consumer
---

# 0006-container-image-and-api-key-file

## Summary

Part 1 of Stage B containerizes `lg` (`lg pool` and `lg serve`) as a hardened,
non-root OCI image (`65532:65532`), adds native `--api-key-file`
(`LG_API_KEY_FILE`) loading so provider API keys never enter Pod `env:` blocks,
process `argv`, or shell wrapper scripts, and adds deterministic static, PR
smoke, and Harbor publish gates.

Parts 2 and 3 of Stage B (seeding OpenBao secrets / `ExternalSecret` objects and
deploying the Cygnus `StatefulSet` in `unsigned-paas`) remain out of scope for
this repository and land in `unsigned-paas` after the signed image digest is
published to `harbor.unsigned.gg/platform/littleguy`.

## Design

### 1. Mounted API Key File (`--api-key-file` / `LG_API_KEY_FILE`)

`CLAUDE.md` forbids shell or Python wrapper scripts in `littleguy`. Passing
model API keys via Kubernetes `env:` exposes credentials in Pod specs and
`/proc/<pid>/environ`, whereas reading a mounted `0600` regular file directly in
Rust keeps secrets out of process arguments and environment variables.

`ServeArgs` (shared by `lg serve` and `lg pool` via `#[command(flatten)]`) adds:

- `--api-key-file <PATH>` (`env = "LG_API_KEY_FILE"`): optional path to a
  regular `0600` file owned by `geteuid()` holding the model API key for
  `--model claude`, `--model openai`, or `--model openrouter`.

`littleguy::secret_file::SecretFile` enforces the same descriptor-level security
contract as `PoolToken` (`specs/0005`, `REQ-LG-POOL-01`):

1. Open with `O_RDONLY | O_NOFOLLOW | O_NONBLOCK` (refusing symlinks without
   following them and never blocking on FIFOs).
2. Verify `is_file()` on the opened descriptor (rejecting directories, FIFOs,
   sockets, and device nodes).
3. Require exact permission mode `0600` (`mode & 0o7777 == 0o600`).
4. Require owner UID equal to `geteuid()`.
5. Enforce a `MAX_SECRET_FILE_BYTES` (`4_096` bytes) cap on both `fstat` length
   and bounded read.
6. Require valid UTF-8 whose trailing `\r`/`\n`-trimmed slice is non-empty and
   consists solely of graphic ASCII bytes (`0x21..=0x7E`).
7. Redact `Debug` (`SecretFile([REDACTED])`) and omit secret bytes from every
   error variant.

When `--api-key-file` is supplied, `build_model` validates it at startup (even
when `--model offline` is selected, so misconfigured secret mounts fail closed
immediately) and uses its secret ahead of `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, or `OPENROUTER_API_KEY`. When `--api-key-file` is omitted,
`build_model` preserves existing environment-variable lookup unchanged.

**Kubernetes `StatefulSet` contract (Stage B Part 3):** Because kubelet projects
`Secret` volumes through `..data` symlinks owned by `uid 0`, a Stage B Pod runs
a minimal non-root `initContainer` using the same `littleguy` image
(`/usr/bin/install -m 0600` from the read-only `ExternalSecret` mount into an
`emptyDir: { medium: Memory }` at `/run/littleman/{token,llm-key}` owned by
`65532:65532`) so both `--token-file` and `--api-key-file` satisfy `O_NOFOLLOW`,
`0600`, and `geteuid()` checks without relaxing descriptor validation.

### 2. Multi-Stage Non-Root `Dockerfile` and `.dockerignore`

- **Builder stage:** `rust:1.99.1-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7`
  (matching `rust-toolchain.toml` `1.99.1` and `rust-version = "1.97.0"`).
  Rewrites `http://` apt sources to `https://` (`OPS-281`), installs
  `ca-certificates` and `pkg-config`, copies `Cargo.toml`, `Cargo.lock`,
  `crates`, `patches`, and `xtask` (`rust-toolchain.toml` is excluded via
  `.dockerignore` so `rustup` never performs redundant network downloads), and
  compiles `lg` with host-throttled jobs:
  `CARGO_BUILD_JOBS=2 cargo build -j 2 --release --locked -p littleguy --bin lg`.
- **Runtime stage:** `debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2`.
  Copies `/etc/ssl/certs/ca-certificates.crt` from the builder stage first,
  rewrites `http://` apt mirrors to `https://`, runs `apt-get update`,
  `apt-get upgrade -y` (applying Debian security updates so Trivy's
  `HIGH,CRITICAL --ignore-unfixed` gate stays green), and installs only
  `ca-certificates` and `tini`. Creates system group and user `nonroot`
  (`65532:65532`) plus `/data` (`0700`, `65532:65532`), sets
  `HOME=/data`, `XDG_CONFIG_HOME=/data/config`, `XDG_DATA_HOME=/data`, and
  `USER=nonroot`, switches to `USER 65532:65532` and `WORKDIR /data`, and sets
  `ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/lg"]` with `CMD ["pool"]`.

### 3. `cargo xtask container-check`, PR Smoke Gate, and Harbor Workflow

- `cargo xtask container-check` (invoked by `cargo xtask ci` and the `lint` CI
  job) statically checks `Dockerfile`, `.dockerignore`,
  `.github/workflows/ci.yml`, and `.github/workflows/build-image.yml` for base
  digest pins, `-j 2` cargo throttling, final `USER 65532:65532`, `tini`
  entrypoint, `.dockerignore` exclusions, 40-SHA GitHub Action pins, absence of
  `:latest` tags, Trivy + Cosign digest signing/attestation, and the missing-secret
  skip guard.
- `.github/workflows/ci.yml` adds a `container` job on `ubuntu-latest` that
  builds `Dockerfile` and runs `id -u`, `id -g`, `lg --version`, and
  `lg pool --help` under `--read-only --cap-drop=ALL --security-opt=no-new-privileges`.
- `.github/workflows/build-image.yml` runs after `CI` succeeds on `main` or via
  `workflow_dispatch` on `main`. A lightweight `preflight` job checks whether
  repository secrets `HARBOR_PLATFORM_PUSH_USERNAME`,
  `HARBOR_PLATFORM_PUSH_PASSWORD`, `COSIGN_PRIVATE_KEY`, and `COSIGN_PASSWORD`
  are configured; while any secret is absent, `preflight` emits `ready=false`
  with a notice and the `publish` job (`if: needs.preflight.outputs.ready == 'true'`)
  skips cleanly without failing CI or creating GitHub Environments. Once armed,
  `publish` builds `Dockerfile`, verifies runtime hardening and layer-size caps
  (`< 99 MB`, `OPS-810`), runs Trivy (`HIGH,CRITICAL --ignore-unfixed --exit-code 1`),
  pushes immutable tags `git-<12sha>` and `<ver>-<7sha>` (plus `v<ver>` if
  absent, never `:latest`) to `harbor.unsigned.gg/platform/littleguy`, signs the
  `sha256` manifest digest with Cosign (`--tlog-upload=false`), attaches and
  verifies a Syft CycloneDX 1.5 attestation, and writes the pinned digest to
  `$GITHUB_STEP_SUMMARY`.

## Requirements

- **REQ-LG-CONT-01:** When `--api-key-file <PATH>` (or `LG_API_KEY_FILE`) is
  passed to `lg serve` or `lg pool`, `lg` shall open `<PATH>` with
  `O_NOFOLLOW | O_NONBLOCK`, refuse to start if `<PATH>` is not a regular file,
  has mode other than `0600`, is not owned by `geteuid()`, exceeds `4_096`
  bytes, or does not hold non-empty graphic ASCII after trailing `\r`/`\n`
  trimming, redact secret bytes from `Debug` and error messages, and supply the
  loaded key to `claude`, `openai`, or `openrouter` ahead of provider
  environment variables.
- **REQ-LG-CONT-02:** The repository root `Dockerfile` and `.dockerignore` shall
  build `lg` in a multi-stage `debian:trixie-slim` image with `@sha256:`-pinned
  base images, HTTPS apt mirrors, `-j 2` Cargo build throttling, `ca-certificates`
  and `tini`, writable `0700` `/data` owned by `65532:65532`, and final runtime
  identity `USER 65532:65532` compatible with `readOnlyRootFilesystem: true`,
  `--cap-drop=ALL`, and `allowPrivilegeEscalation: false`.
- **REQ-LG-CONT-03:** `cargo xtask container-check` (wired into `cargo xtask ci`),
  `.github/workflows/ci.yml` (`container` job), and
  `.github/workflows/build-image.yml` shall enforce container, workflow, Trivy,
  and Cosign supply-chain invariants while skipping the Harbor `publish` job
  cleanly whenever Harbor or Cosign repository secrets are absent.

## Verification

- `crates/littleguy/src/secret_file.rs` unit tests (`SecretFile` happy path,
  symlink/directory/FIFO/mode/owner/oversize/non-graphic rejections, and `Debug`
  / error redaction).
- `crates/littleguy/src/main.rs` unit tests (`ServeArgs` / `PoolArgs` CLI
  parsing and `build_model` precedence and fail-closed startup checks).
- `crates/littleguy/tests/api_key_file.rs` subprocess integration test spawning
  `lg pool` against a local stub Cortex + stub OpenAI `/v1/chat/completions`
  server with `--api-key-file` and no `OPENAI_API_KEY` environment variable.
- `xtask/src/container.rs` unit tests verifying repository files pass and
  synthetic violations fail.
- Local `CARGO_BUILD_JOBS=2 cargo xtask ci` and hardened `docker build` +
  `docker run --read-only --cap-drop=ALL --security-opt=no-new-privileges`
  smoke execution.
