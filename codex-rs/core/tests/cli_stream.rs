#![expect(clippy::unwrap_used)]

use assert_cmd::Command as AssertCommand;
use codex_core::exec::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;
use uuid::Uuid;
use walkdir::WalkDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

/// Tests streaming chat completions through the CLI using a mock server.
/// This test:
/// 1. Sets up a mock server that simulates OpenAI's chat completions API
/// 2. Configures codex to use this mock server via a custom provider
/// 3. Sends a simple "hello?" prompt and verifies the streamed response
/// 4. Ensures the response is received exactly once and contains "hi"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_mode_stream_cli() {
    if std::env::var(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
        println!(
            "Skipping test because it cannot execute when network is disabled in a Codex sandbox."
        );
        return;
    }

    let server = MockServer::start().await;
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{}}]}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let home = TempDir::new().unwrap();
    let provider_override = format!(
        "model_providers.mock={{ name = \"mock\", base_url = \"{}/v1\", env_key = \"PATH\", wire_api = \"chat\" }}",
        server.uri()
    );
    let mut cmd = AssertCommand::new("cargo");
    cmd.arg("run")
        .arg("-p")
        .arg("codex-cli")
        .arg("--quiet")
        .arg("--")
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(&provider_override)
        .arg("-c")
        .arg("model_provider=\"mock\"")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .arg("hello?");
    cmd.env("CODEX_HOME", home.path())
        .env("OPENAI_API_KEY", "dummy")
        .env("OPENAI_BASE_URL", format!("{}/v1", server.uri()));

    let output = cmd.output().unwrap();
    println!("Status: {}", output.status);
    println!("Stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    println!("Stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let hi_lines = stdout.lines().filter(|line| line.trim() == "hi").count();
    assert_eq!(hi_lines, 1, "Expected exactly one line with 'hi'");

    server.verify().await;
}

/// Tests streaming responses through the CLI using a local SSE fixture file.
/// This test:
/// 1. Uses a pre-recorded SSE response fixture instead of a live server
/// 2. Configures codex to read from this fixture via CODEX_RS_SSE_FIXTURE env var
/// 3. Sends a "hello?" prompt and verifies the response
/// 4. Ensures the fixture content is correctly streamed through the CLI
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_api_stream_cli() {
    if std::env::var(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
        println!(
            "Skipping test because it cannot execute when network is disabled in a Codex sandbox."
        );
        return;
    }

    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cli_responses_fixture.sse");

    let home = TempDir::new().unwrap();
    let mut cmd = AssertCommand::new("cargo");
    cmd.arg("run")
        .arg("-p")
        .arg("codex-cli")
        .arg("--quiet")
        .arg("--")
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .arg("hello?");
    cmd.env("CODEX_HOME", home.path())
        .env("OPENAI_API_KEY", "dummy")
        .env("CODEX_RS_SSE_FIXTURE", fixture)
        .env("OPENAI_BASE_URL", "http://unused.local");

    let output = cmd.output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fixture hello"));
}

/// End-to-end: create a session (writes rollout), verify the file, then resume and confirm append.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_creates_and_checks_session_file() {
    // Honor sandbox network restrictions for CI parity with the other tests.
    if std::env::var(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
        println!(
            "Skipping test because it cannot execute when network is disabled in a Codex sandbox."
        );
        return;
    }

    // 1. Temp home so we read/write isolated session files.
    let home = TempDir::new().unwrap();

    // 2. Unique marker we'll look for in the session log.
    let marker = format!("integration-test-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    // 3. Use the same offline SSE fixture as responses_api_stream_cli so the test is hermetic.
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cli_responses_fixture.sse");

    // 4. Run the codex CLI through cargo (ensures the right bin is built) and invoke `exec`,
    //    which is what records a session.
    let mut cmd = AssertCommand::new("cargo");
    cmd.arg("run")
        .arg("-p")
        .arg("codex-cli")
        .arg("--quiet")
        .arg("--")
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .arg(&prompt);
    cmd.env("CODEX_HOME", home.path())
        .env("OPENAI_API_KEY", "dummy")
        .env("CODEX_RS_SSE_FIXTURE", &fixture)
        // Required for CLI arg parsing even though fixture short-circuits network usage.
        .env("OPENAI_BASE_URL", "http://unused.local");

    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "codex-cli exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Wait for sessions dir to appear.
    let sessions_dir = home.path().join("sessions");
    let dir_deadline = Instant::now() + Duration::from_secs(5);
    while !sessions_dir.exists() && Instant::now() < dir_deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(sessions_dir.exists(), "sessions directory never appeared");

    // Find the session file that contains `marker`.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut matching_path: Option<std::path::PathBuf> = None;
    while Instant::now() < deadline && matching_path.is_none() {
        for entry in WalkDir::new(&sessions_dir) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            if !entry.file_name().to_string_lossy().ends_with(".jsonl") {
                continue;
            }
            let path = entry.path();
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let mut lines = content.lines();
            if lines.next().is_none() {
                continue;
            }
            for line in lines {
                if line.trim().is_empty() {
                    continue;
                }
                let item: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if item.get("type").and_then(|t| t.as_str()) == Some("message") {
                    if let Some(c) = item.get("content") {
                        if c.to_string().contains(&marker) {
                            matching_path = Some(path.to_path_buf());
                            break;
                        }
                    }
                }
            }
        }
        if matching_path.is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    let path = match matching_path {
        Some(p) => p,
        None => panic!("No session file containing the marker was found"),
    };

    // Basic sanity checks on location and metadata.
    let rel = match path.strip_prefix(&sessions_dir) {
        Ok(r) => r,
        Err(_) => panic!("session file should live under sessions/"),
    };
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        comps.len(),
        4,
        "Expected sessions/YYYY/MM/DD/<file>, got {rel:?}"
    );
    let year = &comps[0];
    let month = &comps[1];
    let day = &comps[2];
    assert!(
        year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()),
        "Year dir not 4-digit numeric: {year}"
    );
    assert!(
        month.len() == 2 && month.chars().all(|c| c.is_ascii_digit()),
        "Month dir not zero-padded 2-digit numeric: {month}"
    );
    assert!(
        day.len() == 2 && day.chars().all(|c| c.is_ascii_digit()),
        "Day dir not zero-padded 2-digit numeric: {day}"
    );
    if let Ok(m) = month.parse::<u8>() {
        assert!((1..=12).contains(&m), "Month out of range: {m}");
    }
    if let Ok(d) = day.parse::<u8>() {
        assert!((1..=31).contains(&d), "Day out of range: {d}");
    }

    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("Failed to read session file"));
    let mut lines = content.lines();
    let meta_line = lines
        .next()
        .ok_or("missing session meta line")
        .unwrap_or_else(|_| panic!("missing session meta line"));
    let meta: serde_json::Value = serde_json::from_str(meta_line)
        .unwrap_or_else(|_| panic!("Failed to parse session meta line as JSON"));
    assert!(meta.get("id").is_some(), "SessionMeta missing id");
    assert!(
        meta.get("timestamp").is_some(),
        "SessionMeta missing timestamp"
    );

    let mut found_message = false;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(item) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if item.get("type").and_then(|t| t.as_str()) == Some("message") {
            if let Some(c) = item.get("content") {
                if c.to_string().contains(&marker) {
                    found_message = true;
                    break;
                }
            }
        }
    }
    assert!(
        found_message,
        "No message found in session file containing the marker"
    );

    // Second run: resume and append.
    let orig_len = content.lines().count();
    let marker2 = format!("integration-resume-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");
    // Cross‑platform safe resume override.  On Windows, backslashes in a TOML string must be escaped
    // or the parse will fail and the raw literal (including quotes) may be preserved all the way down
    // to Config, which in turn breaks resume because the path is invalid. Normalize to forward slashes
    // to sidestep the issue.
    let resume_path_str = path.to_string_lossy().replace('\\', "/");
    let resume_override = format!("experimental_resume=\"{resume_path_str}\"");
    let mut cmd2 = AssertCommand::new("cargo");
    cmd2.arg("run")
        .arg("-p")
        .arg("codex-cli")
        .arg("--quiet")
        .arg("--")
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(&resume_override)
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .arg(&prompt2);
    cmd2.env("CODEX_HOME", home.path())
        .env("OPENAI_API_KEY", "dummy")
        .env("CODEX_RS_SSE_FIXTURE", &fixture)
        .env("OPENAI_BASE_URL", "http://unused.local");

    let output2 = cmd2.output().unwrap();
    assert!(output2.status.success(), "resume codex-cli run failed");

    // The rollout writer runs on a background async task; give it a moment to flush.
    let mut new_len = orig_len;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut content2 = String::new();
    while Instant::now() < deadline {
        if let Ok(c) = std::fs::read_to_string(&path) {
            let count = c.lines().count();
            if count > orig_len {
                content2 = c;
                new_len = count;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if content2.is_empty() {
        // last attempt
        content2 = std::fs::read_to_string(&path).unwrap();
        new_len = content2.lines().count();
    }
    assert!(new_len > orig_len, "rollout file did not grow after resume");
    assert!(content2.contains(&marker), "rollout lost original marker");
    assert!(
        content2.contains(&marker2),
        "rollout missing resumed marker"
    );
}

/// Integration test to verify git info is collected and recorded in session files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_git_info_unit_test() {
    // This test verifies git info collection works independently
    // without depending on the full CLI integration

    // 1. Create temp directory for git repo
    let temp_dir = TempDir::new().unwrap();
    let git_repo = temp_dir.path().to_path_buf();

    // 2. Initialize a git repository with some content
    let init_output = std::process::Command::new("git")
        .args(["init"])
        .current_dir(&git_repo)
        .output()
        .unwrap();
    assert!(init_output.status.success(), "git init failed");

    // Configure git user (required for commits)
    std::process::Command::new("git")
        .args(["config", "user.name", "Integration Test"])
        .current_dir(&git_repo)
        .output()
        .unwrap();

    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&git_repo)
        .output()
        .unwrap();

    // Create a test file and commit it
    let test_file = git_repo.join("test.txt");
    std::fs::write(&test_file, "integration test content").unwrap();

    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&git_repo)
        .output()
        .unwrap();

    let commit_output = std::process::Command::new("git")
        .args(["commit", "-m", "Integration test commit"])
        .current_dir(&git_repo)
        .output()
        .unwrap();
    assert!(commit_output.status.success(), "git commit failed");

    // Create a branch to test branch detection
    std::process::Command::new("git")
        .args(["checkout", "-b", "integration-test-branch"])
        .current_dir(&git_repo)
        .output()
        .unwrap();

    // Add a remote to test repository URL detection
    std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/example/integration-test.git",
        ])
        .current_dir(&git_repo)
        .output()
        .unwrap();

    // 3. Test git info collection directly
    let git_info = codex_core::git_info::collect_git_info(&git_repo).await;

    // 4. Verify git info is present and contains expected data
    assert!(git_info.is_some(), "Git info should be collected");

    let git_info = git_info.unwrap();

    // Check that we have a commit hash
    assert!(
        git_info.commit_hash.is_some(),
        "Git info should contain commit_hash"
    );
    let commit_hash = git_info.commit_hash.as_ref().unwrap();
    assert_eq!(commit_hash.len(), 40, "Commit hash should be 40 characters");
    assert!(
        commit_hash.chars().all(|c| c.is_ascii_hexdigit()),
        "Commit hash should be hexadecimal"
    );

    // Check that we have the correct branch
    assert!(git_info.branch.is_some(), "Git info should contain branch");
    let branch = git_info.branch.as_ref().unwrap();
    assert_eq!(
        branch, "integration-test-branch",
        "Branch should match what we created"
    );

    // Check that we have the repository URL
    assert!(
        git_info.repository_url.is_some(),
        "Git info should contain repository_url"
    );
    let repo_url = git_info.repository_url.as_ref().unwrap();
    assert_eq!(
        repo_url, "https://github.com/example/integration-test.git",
        "Repository URL should match what we configured"
    );

    println!("✅ Git info collection test passed!");
    println!("   Commit: {commit_hash}");
    println!("   Branch: {branch}");
    println!("   Repo: {repo_url}");

    // 5. Test serialization to ensure it works in SessionMeta
    let serialized = serde_json::to_string(&git_info).unwrap();
    let deserialized: codex_core::git_info::GitInfo = serde_json::from_str(&serialized).unwrap();

    assert_eq!(git_info.commit_hash, deserialized.commit_hash);
    assert_eq!(git_info.branch, deserialized.branch);
    assert_eq!(git_info.repository_url, deserialized.repository_url);

    println!("✅ Git info serialization test passed!");
}
