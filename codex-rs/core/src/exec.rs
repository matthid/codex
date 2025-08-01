#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::Notify;
use tracing::trace;

use crate::error::CodexErr;
use crate::error::Result;
use crate::error::SandboxErr;
use crate::protocol::SandboxPolicy;

// Maximum we send for each stream, which is either:
// - 10KiB OR
// - 256 lines
const MAX_STREAM_OUTPUT: usize = 10 * 1024;
const MAX_STREAM_OUTPUT_LINES: usize = 256;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;

// Hardcode these since it does not seem worth including the libc crate just
// for these.
const SIGKILL_CODE: i32 = 9;
const TIMEOUT_CODE: i32 = 64;

const MACOS_SEATBELT_BASE_POLICY: &str = include_str!("seatbelt_base_policy.sbpl");

/// When working with `sandbox-exec`, only consider `sandbox-exec` in `/usr/bin`
/// to defend against an attacker trying to inject a malicious version on the
/// PATH. If /usr/bin/sandbox-exec has been tampered with, then the attacker
/// already has root access.
const MACOS_PATH_TO_SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";

/// Experimental environment variable that will be set to some non-empty value
/// if both of the following are true:
///
/// 1. The process was spawned by Codex as part of a shell tool call.
/// 2. SandboxPolicy.has_full_network_access() was false for the tool call.
///
/// We may try to have just one environment variable for all sandboxing
/// attributes, so this may change in the future.
pub const CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR: &str = "CODEX_SANDBOX_NETWORK_DISABLED";

#[derive(Debug, Clone)]
pub struct ExecParams {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub timeout_ms: Option<u64>,
    pub env: HashMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SandboxType {
    None,

    /// Only available on macOS.
    MacosSeatbelt,

    /// Only available on Linux.
    LinuxSeccomp,
}

pub async fn process_exec_tool_call(
    params: ExecParams,
    sandbox_type: SandboxType,
    ctrl_c: Arc<Notify>,
    sandbox_policy: &SandboxPolicy,
    codex_linux_sandbox_exe: &Option<PathBuf>,
) -> Result<ExecToolCallOutput> {
    let start = Instant::now();

    let raw_output_result: std::result::Result<RawExecToolCallOutput, CodexErr> = match sandbox_type
    {
        SandboxType::None => exec(params, sandbox_policy, ctrl_c).await,
        SandboxType::MacosSeatbelt => {
            let ExecParams {
                command,
                cwd,
                timeout_ms,
                env,
            } = params;
            let child = spawn_command_under_seatbelt(
                command,
                sandbox_policy,
                cwd,
                StdioPolicy::RedirectForShellTool,
                env,
            )
            .await?;
            consume_truncated_output(child, ctrl_c, timeout_ms).await
        }
        SandboxType::LinuxSeccomp => {
            let ExecParams {
                command,
                cwd,
                timeout_ms,
                env,
            } = params;

            let codex_linux_sandbox_exe = codex_linux_sandbox_exe
                .as_ref()
                .ok_or(CodexErr::LandlockSandboxExecutableNotProvided)?;
            let child = spawn_command_under_linux_sandbox(
                codex_linux_sandbox_exe,
                command,
                sandbox_policy,
                cwd,
                StdioPolicy::RedirectForShellTool,
                env,
            )
            .await?;

            consume_truncated_output(child, ctrl_c, timeout_ms).await
        }
    };
    let duration = start.elapsed();
    match raw_output_result {
        Ok(raw_output) => {
            let stdout = String::from_utf8_lossy(&raw_output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&raw_output.stderr).to_string();

            #[cfg(target_family = "unix")]
            match raw_output.exit_status.signal() {
                Some(TIMEOUT_CODE) => return Err(CodexErr::Sandbox(SandboxErr::Timeout)),
                Some(signal) => {
                    return Err(CodexErr::Sandbox(SandboxErr::Signal(signal)));
                }
                None => {}
            }

            let exit_code = raw_output.exit_status.code().unwrap_or(-1);

            // NOTE(ragona): This is much less restrictive than the previous check. If we exec
            // a command, and it returns anything other than success, we assume that it may have
            // been a sandboxing error and allow the user to retry. (The user of course may choose
            // not to retry, or in a non-interactive mode, would automatically reject the approval.)
            if exit_code != 0 && sandbox_type != SandboxType::None {
                return Err(CodexErr::Sandbox(SandboxErr::Denied(
                    exit_code, stdout, stderr,
                )));
            }

            Ok(ExecToolCallOutput {
                exit_code,
                stdout,
                stderr,
                duration,
            })
        }
        Err(err) => {
            tracing::error!("exec error: {err}");
            Err(err)
        }
    }
}

pub async fn spawn_command_under_seatbelt(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    cwd: PathBuf,
    stdio_policy: StdioPolicy,
    env: HashMap<String, String>,
) -> std::io::Result<Child> {
    let args = create_seatbelt_command_args(command, sandbox_policy, &cwd);
    let arg0 = None;
    spawn_child_async(
        PathBuf::from(MACOS_PATH_TO_SEATBELT_EXECUTABLE),
        args,
        arg0,
        cwd,
        sandbox_policy,
        stdio_policy,
        env,
    )
    .await
}

/// Spawn a shell tool command under the Linux Landlock+seccomp sandbox helper
/// (codex-linux-sandbox).
///
/// Unlike macOS Seatbelt where we directly embed the policy text, the Linux
/// helper accepts a list of `--sandbox-permission`/`-s` flags mirroring the
/// public CLI. We convert the internal [`SandboxPolicy`] representation into
/// the equivalent CLI options.
pub async fn spawn_command_under_linux_sandbox<P>(
    codex_linux_sandbox_exe: P,
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    cwd: PathBuf,
    stdio_policy: StdioPolicy,
    env: HashMap<String, String>,
) -> std::io::Result<Child>
where
    P: AsRef<Path>,
{
    let args = create_linux_sandbox_command_args(command, sandbox_policy, &cwd);
    let arg0 = Some("codex-linux-sandbox");
    spawn_child_async(
        codex_linux_sandbox_exe.as_ref().to_path_buf(),
        args,
        arg0,
        cwd,
        sandbox_policy,
        stdio_policy,
        env,
    )
    .await
}

/// Converts the sandbox policy into the CLI invocation for `codex-linux-sandbox`.
fn create_linux_sandbox_command_args(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    cwd: &Path,
) -> Vec<String> {
    #[expect(clippy::expect_used)]
    let sandbox_policy_cwd = cwd.to_str().expect("cwd must be valid UTF-8").to_string();

    #[expect(clippy::expect_used)]
    let sandbox_policy_json =
        serde_json::to_string(sandbox_policy).expect("Failed to serialize SandboxPolicy to JSON");

    let mut linux_cmd: Vec<String> = vec![
        sandbox_policy_cwd,
        sandbox_policy_json,
        // Separator so that command arguments starting with `-` are not parsed as
        // options of the helper itself.
        "--".to_string(),
    ];

    // Append the original tool command.
    linux_cmd.extend(command);

    linux_cmd
}

fn create_seatbelt_command_args(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    cwd: &Path,
) -> Vec<String> {
    let (file_write_policy, extra_cli_args) = {
        if sandbox_policy.has_full_disk_write_access() {
            // Allegedly, this is more permissive than `(allow file-write*)`.
            (
                r#"(allow file-write* (regex #"^/"))"#.to_string(),
                Vec::<String>::new(),
            )
        } else {
            let writable_roots = sandbox_policy.get_writable_roots_with_cwd(cwd);
            let (writable_folder_policies, cli_args): (Vec<String>, Vec<String>) = writable_roots
                .iter()
                .enumerate()
                .map(|(index, root)| {
                    let param_name = format!("WRITABLE_ROOT_{index}");
                    let policy: String = format!("(subpath (param \"{param_name}\"))");
                    let cli_arg = format!("-D{param_name}={}", root.to_string_lossy());
                    (policy, cli_arg)
                })
                .unzip();
            if writable_folder_policies.is_empty() {
                ("".to_string(), Vec::<String>::new())
            } else {
                let file_write_policy = format!(
                    "(allow file-write*\n{}\n)",
                    writable_folder_policies.join(" ")
                );
                (file_write_policy, cli_args)
            }
        }
    };

    let file_read_policy = if sandbox_policy.has_full_disk_read_access() {
        "; allow read-only file operations\n(allow file-read*)"
    } else {
        ""
    };

    // TODO(mbolin): apply_patch calls must also honor the SandboxPolicy.
    let network_policy = if sandbox_policy.has_full_network_access() {
        "(allow network-outbound)\n(allow network-inbound)\n(allow system-socket)"
    } else {
        ""
    };

    let full_policy = format!(
        "{MACOS_SEATBELT_BASE_POLICY}\n{file_read_policy}\n{file_write_policy}\n{network_policy}"
    );
    let mut seatbelt_args: Vec<String> = vec!["-p".to_string(), full_policy];
    seatbelt_args.extend(extra_cli_args);
    seatbelt_args.push("--".to_string());
    seatbelt_args.extend(command);
    seatbelt_args
}

#[derive(Debug)]
pub struct RawExecToolCallOutput {
    pub exit_status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
pub struct ExecToolCallOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

async fn exec(
    ExecParams {
        command,
        cwd,
        timeout_ms,
        env,
    }: ExecParams,
    sandbox_policy: &SandboxPolicy,
    ctrl_c: Arc<Notify>,
) -> Result<RawExecToolCallOutput> {
    let (program, args) = command.split_first().ok_or_else(|| {
        CodexErr::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command args are empty",
        ))
    })?;
    let arg0 = None;
    let child = spawn_child_async(
        PathBuf::from(program),
        args.into(),
        arg0,
        cwd,
        sandbox_policy,
        StdioPolicy::RedirectForShellTool,
        env,
    )
    .await?;
    consume_truncated_output(child, ctrl_c, timeout_ms).await
}

#[derive(Debug, Clone, Copy)]
pub enum StdioPolicy {
    RedirectForShellTool,
    Inherit,
}

/// Spawns the appropriate child process for the ExecParams and SandboxPolicy,
/// ensuring the args and environment variables used to create the `Command`
/// (and `Child`) honor the configuration.
///
/// For now, we take `SandboxPolicy` as a parameter to spawn_child() because
/// we need to determine whether to set the
/// `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` environment variable.
async fn spawn_child_async(
    program: PathBuf,
    args: Vec<String>,
    #[cfg_attr(not(unix), allow(unused_variables))] arg0: Option<&str>,
    cwd: PathBuf,
    sandbox_policy: &SandboxPolicy,
    stdio_policy: StdioPolicy,
    env: HashMap<String, String>,
) -> std::io::Result<Child> {
    trace!(
        "spawn_child_async: {program:?} {args:?} {arg0:?} {cwd:?} {sandbox_policy:?} {stdio_policy:?} {env:?}"
    );

    let mut cmd = Command::new(&program);
    #[cfg(unix)]
    cmd.arg0(arg0.map_or_else(|| program.to_string_lossy().to_string(), String::from));
    cmd.args(args);
    cmd.current_dir(cwd);
    cmd.env_clear();
    cmd.envs(env);

    if !sandbox_policy.has_full_network_access() {
        cmd.env(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR, "1");
    }

    // If this Codex process dies (including being killed via SIGKILL), we want
    // any child processes that were spawned as part of a `"shell"` tool call
    // to also be terminated.

    // This relies on prctl(2), so it only works on Linux.
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            // This prctl call effectively requests, "deliver SIGTERM when my
            // current parent dies."
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(io::Error::last_os_error());
            }

            // Though if there was a race condition and this pre_exec() block is
            // run _after_ the parent (i.e., the Codex process) has already
            // exited, then the parent is the _init_ process (which will never
            // die), so we should just terminate the child process now.
            if libc::getppid() == 1 {
                libc::raise(libc::SIGTERM);
            }
            Ok(())
        });
    }

    match stdio_policy {
        StdioPolicy::RedirectForShellTool => {
            // Do not create a file descriptor for stdin because otherwise some
            // commands may hang forever waiting for input. For example, ripgrep has
            // a heuristic where it may try to read from stdin as explained here:
            // https://github.com/BurntSushi/ripgrep/blob/e2362d4d5185d02fa857bf381e7bd52e66fafc73/crates/core/flags/hiargs.rs#L1101-L1103
            cmd.stdin(Stdio::null());

            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        StdioPolicy::Inherit => {
            // Inherit stdin, stdout, and stderr from the parent process.
            cmd.stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
    }

    cmd.kill_on_drop(true).spawn()
}

/// Consumes the output of a child process, truncating it so it is suitable for
/// use as the output of a `shell` tool call. Also enforces specified timeout.
pub(crate) async fn consume_truncated_output(
    mut child: Child,
    ctrl_c: Arc<Notify>,
    timeout_ms: Option<u64>,
) -> Result<RawExecToolCallOutput> {
    // Both stdout and stderr were configured with `Stdio::piped()`
    // above, therefore `take()` should normally return `Some`.  If it doesn't
    // we treat it as an exceptional I/O error

    let stdout_reader = child.stdout.take().ok_or_else(|| {
        CodexErr::Io(io::Error::other(
            "stdout pipe was unexpectedly not available",
        ))
    })?;
    let stderr_reader = child.stderr.take().ok_or_else(|| {
        CodexErr::Io(io::Error::other(
            "stderr pipe was unexpectedly not available",
        ))
    })?;

    let stdout_handle = tokio::spawn(read_capped(
        BufReader::new(stdout_reader),
        MAX_STREAM_OUTPUT,
        MAX_STREAM_OUTPUT_LINES,
    ));
    let stderr_handle = tokio::spawn(read_capped(
        BufReader::new(stderr_reader),
        MAX_STREAM_OUTPUT,
        MAX_STREAM_OUTPUT_LINES,
    ));

    let interrupted = ctrl_c.notified();
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let exit_status = tokio::select! {
        result = tokio::time::timeout(timeout, child.wait()) => {
            match result {
                Ok(Ok(exit_status)) => exit_status,
                Ok(e) => e?,
                Err(_) => {
                    // timeout
                    child.start_kill()?;
                    // Debatable whether `child.wait().await` should be called here.
                    synthetic_exit_status(128 + TIMEOUT_CODE)
                }
            }
        }
        _ = interrupted => {
            child.start_kill()?;
            synthetic_exit_status(128 + SIGKILL_CODE)
        }
    };

    let stdout = stdout_handle.await??;
    let stderr = stderr_handle.await??;

    Ok(RawExecToolCallOutput {
        exit_status,
        stdout,
        stderr,
    })
}

async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    max_output: usize,
    max_lines: usize,
) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(max_output.min(8 * 1024));
    let mut tmp = [0u8; 8192];

    let mut remaining_bytes = max_output;
    let mut remaining_lines = max_lines;

    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            break;
        }

        // Copy into the buffer only while we still have byte and line budget.
        if remaining_bytes > 0 && remaining_lines > 0 {
            let mut copy_len = 0;
            for &b in &tmp[..n] {
                if remaining_bytes == 0 || remaining_lines == 0 {
                    break;
                }
                copy_len += 1;
                remaining_bytes -= 1;
                if b == b'\n' {
                    remaining_lines -= 1;
                }
            }
            buf.extend_from_slice(&tmp[..copy_len]);
        }
        // Continue reading to EOF to avoid back-pressure, but discard once caps are hit.
    }

    Ok(buf)
}

#[cfg(unix)]
fn synthetic_exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code)
}

#[cfg(windows)]
fn synthetic_exit_status(code: i32) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    #[expect(clippy::unwrap_used)]
    std::process::ExitStatus::from_raw(code.try_into().unwrap())
}
