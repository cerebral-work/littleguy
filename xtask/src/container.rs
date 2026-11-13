//! `cargo xtask container-check`: static supply-chain and non-root invariants
//! for `Dockerfile`, `.dockerignore`, `.github/workflows/ci.yml`, and
//! `.github/workflows/build-image.yml` (`specs/0006`, `REQ-LG-CONT-02..03`).

use std::path::Path;

use regex::Regex;
use smop::prelude::*;

const REQUIRED_DOCKERIGNORE: &[&str] = &[".git", "target", "rust-toolchain.toml", "specs", "docs"];

/// Checks container and workflow invariants at repository `root`.
pub fn check(root: &Path) -> Result<()> {
    let dockerfile = read_required(root, "Dockerfile")?;
    let dockerignore = read_required(root, ".dockerignore")?;
    let ci_yml = read_required(root, ".github/workflows/ci.yml")?;
    let build_image_yml = read_required(root, ".github/workflows/build-image.yml")?;
    check_contents(&dockerfile, &dockerignore, &ci_yml, &build_image_yml)
}

fn read_required(root: &Path, rel: &str) -> Result<String> {
    let path = root.join(rel);
    fs::read_string(&path).with_context(|| format!("missing or unreadable {rel}"))
}

fn check_contents(
    dockerfile: &str,
    dockerignore: &str,
    ci_yml: &str,
    build_image_yml: &str,
) -> Result<()> {
    check_dockerfile(dockerfile)?;
    check_dockerignore(dockerignore)?;
    check_actions_pinned(ci_yml, ".github/workflows/ci.yml")?;
    check_actions_pinned(build_image_yml, ".github/workflows/build-image.yml")?;
    check_ci_workflow(ci_yml)?;
    check_build_image_workflow(build_image_yml)
}

fn check_dockerfile(dockerfile: &str) -> Result<()> {
    let from_pinned = Regex::new(r"^FROM [^\s]+@sha256:[0-9a-f]{64}(\s+AS\s+[A-Za-z0-9_-]+)?$")
        .context("compile FROM regex")?;
    let mut from_count = 0usize;
    let mut last_user: Option<&str> = None;
    for raw_line in dockerfile.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.to_ascii_uppercase().starts_with("FROM ") {
            from_count += 1;
            if !from_pinned.is_match(line) {
                bail!("Dockerfile FROM line is not pinned by sha256 digest: `{line}`");
            }
        }
        if let Some(rest) = line.strip_prefix("USER ") {
            last_user = Some(rest.trim());
        }
    }
    if from_count < 2 {
        bail!("Dockerfile must use a multi-stage build (found {from_count} FROM lines)");
    }
    if last_user != Some("65532:65532") {
        bail!("Dockerfile final USER must be `65532:65532`, got {last_user:?}");
    }
    let uncommented = strip_comment_lines(dockerfile);
    for needle in [
        "sed -i 's|http://|https://|g' /etc/apt/sources.list.d/debian.sources",
        "cargo build -j 2 --release --locked -p littleguy --bin lg",
        "apt-get -o Acquire::https::CaInfo=/etc/ssl/certs/ca-certificates.crt upgrade -y",
        "ENTRYPOINT [\"/usr/bin/tini\", \"--\", \"/usr/local/bin/lg\"]",
        "CMD [\"pool\"]",
    ] {
        if !uncommented.contains(needle) {
            bail!("Dockerfile missing required directive `{needle}`");
        }
    }
    Ok(())
}

fn strip_comment_lines(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_dockerignore(dockerignore: &str) -> Result<()> {
    let entries: Vec<&str> = dockerignore
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    for required in REQUIRED_DOCKERIGNORE {
        if !entries.contains(required) {
            bail!(".dockerignore must exclude `{required}`");
        }
    }
    Ok(())
}

fn check_actions_pinned(yaml: &str, file_label: &str) -> Result<()> {
    let pinned = Regex::new(r"^[A-Za-z0-9_./-]+@[0-9a-f]{40}(\s+#.*)?$")
        .context("compile uses pin regex")?;
    for raw_line in yaml.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let rest = trimmed
            .strip_prefix("- uses:")
            .or_else(|| trimmed.strip_prefix("uses:"));
        if let Some(target) = rest.map(str::trim)
            && !pinned.is_match(target)
        {
            bail!("{file_label} has unpinned GitHub Action `uses: {target}`");
        }
    }
    Ok(())
}

fn check_ci_workflow(ci_yml: &str) -> Result<()> {
    let uncommented = strip_comment_lines(ci_yml);
    for needle in [
        "cargo xtask container-check",
        "container:",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "65532:65532",
    ] {
        if !uncommented.contains(needle) {
            bail!(".github/workflows/ci.yml missing `{needle}`");
        }
    }
    Ok(())
}

fn check_build_image_workflow(build_image_yml: &str) -> Result<()> {
    if build_image_yml.contains(":latest") {
        bail!(".github/workflows/build-image.yml must never tag or pull `:latest`");
    }
    let uncommented = strip_comment_lines(build_image_yml);
    let curl_argv_auth = Regex::new(r"(?m)\bcurl\b[^\n]*(\s-u\b|\s--user\b)")
        .context("compile curl argv auth regex")?;
    let collapsed = uncommented.replace("\\\n", " ");
    if curl_argv_auth.is_match(&collapsed) {
        bail!(
            ".github/workflows/build-image.yml must not pass credentials via `curl -u` / `--user` argv; use `--netrc-file`"
        );
    }
    require_job_environment(build_image_yml, "preflight", "harbor-publish")?;
    require_job_environment(build_image_yml, "publish", "harbor-publish")?;
    for needle in [
        "harbor.unsigned.gg",
        "platform/littleguy",
        "HARBOR_PLATFORM_PUSH_USERNAME",
        "HARBOR_PLATFORM_PUSH_PASSWORD",
        "COSIGN_PRIVATE_KEY",
        "COSIGN_PASSWORD",
        "github.event.workflow_run.event == 'push'",
        "github.event.workflow_run.head_repository.full_name == github.repository",
        "github.event.workflow_run.head_branch == 'main'",
        "if: needs.preflight.outputs.ready == 'true'",
        "git merge-base --is-ancestor",
        "--netrc-file",
        "trivy image",
        "--ignore-unfixed --exit-code 1",
        "cosign sign",
        "--tlog-upload=false",
        "cosign attest",
    ] {
        if !uncommented.contains(needle) {
            bail!(".github/workflows/build-image.yml missing `{needle}`");
        }
    }
    Ok(())
}

fn require_job_environment(build_image_yml: &str, job: &str, env_name: &str) -> Result<()> {
    let expected_job = format!("{job}:");
    let expected_env = format!("environment: {env_name}");
    let mut in_jobs = false;
    let mut in_target_job = false;
    let mut found_env = false;
    for raw_line in build_image_yml.lines() {
        if raw_line.trim().is_empty() || raw_line.trim_start().starts_with('#') {
            continue;
        }
        let uncommented = raw_line
            .split_once('#')
            .map_or(raw_line, |(head, _)| head)
            .trim_end();
        if !uncommented.starts_with(' ') {
            in_jobs = uncommented == "jobs:";
            in_target_job = false;
            continue;
        }
        if !in_jobs {
            continue;
        }
        if let Some(rest) = uncommented.strip_prefix("  ")
            && !rest.starts_with(' ')
        {
            in_target_job = rest == expected_job;
            continue;
        }
        if in_target_job
            && let Some(rest) = uncommented.strip_prefix("    ")
            && !rest.starts_with(' ')
            && rest == expected_env
        {
            found_env = true;
        }
    }
    if !found_env {
        bail!(
            ".github/workflows/build-image.yml job `{job}` must declare `environment: {env_name}`"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_DOCKERFILE: &str = "\
FROM rust:1.99.1-slim-trixie@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7 AS builder
RUN sed -i 's|http://|https://|g' /etc/apt/sources.list.d/debian.sources
RUN CARGO_BUILD_JOBS=2 cargo build -j 2 --release --locked -p littleguy --bin lg
FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2 AS runtime
RUN sed -i 's|http://|https://|g' /etc/apt/sources.list.d/debian.sources && apt-get -o Acquire::https::CaInfo=/etc/ssl/certs/ca-certificates.crt upgrade -y
USER 65532:65532
ENTRYPOINT [\"/usr/bin/tini\", \"--\", \"/usr/local/bin/lg\"]
CMD [\"pool\"]
";

    const VALID_DOCKERIGNORE: &str = ".git\ntarget\nrust-toolchain.toml\nspecs\ndocs\n";

    const VALID_CI_YML: &str = "\
jobs:
  lint:
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - run: cargo xtask container-check
  container:
    steps:
      - run: docker inspect 65532:65532 && docker run --read-only --cap-drop=ALL --security-opt=no-new-privileges lg --version
";

    const VALID_BUILD_IMAGE_YML: &str = "\
jobs:
  preflight:
    if: github.event.workflow_run.event == 'push' && github.event.workflow_run.head_repository.full_name == github.repository && github.event.workflow_run.head_branch == 'main'
    environment: harbor-publish
    steps:
      - env:
          U: ${{ secrets.HARBOR_PLATFORM_PUSH_USERNAME }}
          P: ${{ secrets.HARBOR_PLATFORM_PUSH_PASSWORD }}
          K: ${{ secrets.COSIGN_PRIVATE_KEY }}
          W: ${{ secrets.COSIGN_PASSWORD }}
  publish:
    if: needs.preflight.outputs.ready == 'true'
    environment: harbor-publish
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - run: |
          git merge-base --is-ancestor \"$TARGET_SHA\" refs/remotes/origin/main
          curl -s --netrc-file \"$netrc\" https://harbor.unsigned.gg/v2/platform/littleguy/tags/list
          ref=harbor.unsigned.gg/platform/littleguy:git-1234567
          trivy image --severity HIGH,CRITICAL --ignore-unfixed --exit-code 1 \"$ref\"
          cosign sign --tlog-upload=false \"$ref\"
          cosign attest --tlog-upload=false \"$ref\"
";

    #[test]
    fn repository_files_satisfy_container_invariants() {
        check(&crate::repo_root()).unwrap();
    }

    #[test]
    fn synthetic_valid_inputs_pass_and_violations_fail() {
        check_contents(
            VALID_DOCKERFILE,
            VALID_DOCKERIGNORE,
            VALID_CI_YML,
            VALID_BUILD_IMAGE_YML,
        )
        .unwrap();

        let unpinned_from = VALID_DOCKERFILE.replace(
            "@sha256:f6c4a35d21dbb3c89d7a8d056c193e94fc09037f2b58dd858b09c39a47dd02d7",
            "",
        );
        assert!(
            check_contents(
                &unpinned_from,
                VALID_DOCKERIGNORE,
                VALID_CI_YML,
                VALID_BUILD_IMAGE_YML
            )
            .is_err()
        );

        let root_user = VALID_DOCKERFILE.replace("USER 65532:65532", "USER 0:0");
        assert!(
            check_contents(
                &root_user,
                VALID_DOCKERIGNORE,
                VALID_CI_YML,
                VALID_BUILD_IMAGE_YML
            )
            .is_err()
        );

        let missing_ignore = ".git\ntarget\n";
        assert!(
            check_contents(
                VALID_DOCKERFILE,
                missing_ignore,
                VALID_CI_YML,
                VALID_BUILD_IMAGE_YML
            )
            .is_err()
        );
    }

    #[test]
    fn synthetic_workflow_violations_fail() {
        let unpinned_action = VALID_CI_YML.replace(
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
            "actions/checkout@v4",
        );
        assert!(
            check_contents(
                VALID_DOCKERFILE,
                VALID_DOCKERIGNORE,
                &unpinned_action,
                VALID_BUILD_IMAGE_YML
            )
            .is_err()
        );

        let latest_tag =
            format!("{VALID_BUILD_IMAGE_YML}\n# harbor.unsigned.gg/platform/littleguy:latest\n");
        assert!(
            check_contents(
                VALID_DOCKERFILE,
                VALID_DOCKERIGNORE,
                VALID_CI_YML,
                &latest_tag
            )
            .is_err()
        );

        let no_skip_guard =
            VALID_BUILD_IMAGE_YML.replace("if: needs.preflight.outputs.ready == 'true'", "");
        assert!(
            check_contents(
                VALID_DOCKERFILE,
                VALID_DOCKERIGNORE,
                VALID_CI_YML,
                &no_skip_guard
            )
            .is_err()
        );

        let no_fork_guard = VALID_BUILD_IMAGE_YML.replace(
            "github.event.workflow_run.head_repository.full_name == github.repository",
            "true",
        );
        assert!(
            check_contents(
                VALID_DOCKERFILE,
                VALID_DOCKERIGNORE,
                VALID_CI_YML,
                &no_fork_guard
            )
            .is_err()
        );

        let curl_argv_secret = VALID_BUILD_IMAGE_YML.replace(
            "curl -s --netrc-file \"$netrc\"",
            "curl -s \\\n            -u \"$U:$P\" \\\n            --netrc-file \"$netrc\"",
        );
        assert!(
            check_contents(
                VALID_DOCKERFILE,
                VALID_DOCKERIGNORE,
                VALID_CI_YML,
                &curl_argv_secret
            )
            .is_err()
        );
    }

    #[test]
    fn synthetic_environment_and_comment_violations_fail() {
        let missing_preflight_env =
            VALID_BUILD_IMAGE_YML.replacen("    environment: harbor-publish\n", "", 1);
        let err = check_contents(
            VALID_DOCKERFILE,
            VALID_DOCKERIGNORE,
            VALID_CI_YML,
            &missing_preflight_env,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("job `preflight` must declare `environment: harbor-publish`"));

        let missing_publish_env = VALID_BUILD_IMAGE_YML.replace(
            "  publish:\n    if: needs.preflight.outputs.ready == 'true'\n    environment: harbor-publish\n",
            "  publish:\n    if: needs.preflight.outputs.ready == 'true'\n",
        );
        let err = check_contents(
            VALID_DOCKERFILE,
            VALID_DOCKERIGNORE,
            VALID_CI_YML,
            &missing_publish_env,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("job `publish` must declare `environment: harbor-publish`"));

        let no_head_branch_guard = VALID_BUILD_IMAGE_YML
            .replace("github.event.workflow_run.head_branch == 'main'", "true");
        assert!(
            check_contents(
                VALID_DOCKERFILE,
                VALID_DOCKERIGNORE,
                VALID_CI_YML,
                &no_head_branch_guard
            )
            .is_err()
        );

        let commented_merge_base = VALID_BUILD_IMAGE_YML.replace(
            "git merge-base --is-ancestor",
            "# git merge-base --is-ancestor",
        );
        assert!(
            check_contents(
                VALID_DOCKERFILE,
                VALID_DOCKERIGNORE,
                VALID_CI_YML,
                &commented_merge_base
            )
            .is_err()
        );
    }
}
