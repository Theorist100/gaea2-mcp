//! CLI automation for Gaea2 project execution.
//!
//! Drives Gaea.Swarm.exe through its command line.
//!
//! Gaea.Swarm needs a real console. Capturing its output hands it a pipe instead, and it dies
//! with `System.IO.IOException: The handle is invalid` (exit code -532462766) after the project
//! has already been loaded - so the failure looks like a broken project rather than a broken
//! invocation. The child therefore gets a console of its own and its diagnostics are read back
//! from the crash log Gaea writes into the build directory.

use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio::process::Command;
use tokio::time::{timeout, Duration};

use crate::types::ExecutionResult;

/// Name of the crash log Gaea writes into the build directory when a node faults.
const CRASH_LOG: &str = "CRASH_LOG.txt";

/// Starting Gaea without a window, with a console, and with its exit code intact.
///
/// The three requirements fight each other through the standard library. Gaea.Swarm calls console
/// APIs, so it needs standard handles that belong to a real console - a pipe or NUL makes it die
/// with `The handle is invalid` once the project is already loaded. Rust always passes the
/// parent's handles through STARTUPINFO, and CREATE_NEW_CONSOLE does not replace handles that
/// were passed explicitly. Going through `cmd /c start` solved the console but opened a window
/// per build and threw the exit code away: `start` reports success no matter what the child
/// returned.
///
/// Creating the process directly settles all three: a console of its own, hidden through
/// STARTF_USESHOWWINDOW, and the real exit code from GetExitCodeProcess.
#[cfg(windows)]
mod hidden_launch {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
        CREATE_NEW_CONSOLE, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
    };

    /// SW_HIDE: the new console exists but is never shown.
    const SW_HIDE: u16 = 0;

    /// How the child ended.
    pub enum Outcome {
        /// Exited on its own with this code.
        Exited(i32),
        /// Outlived the timeout and was terminated.
        TimedOut,
    }

    /// Quote one argument the way `CommandLineToArgvW` reads it back.
    fn quote(arg: &str) -> String {
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            return arg.to_string();
        }

        let mut quoted = String::with_capacity(arg.len() + 2);
        quoted.push('"');
        let mut pending_backslashes = 0;
        for ch in arg.chars() {
            match ch {
                '\\' => pending_backslashes += 1,
                '"' => {
                    // Backslashes before a quote are doubled, then the quote is escaped.
                    for _ in 0..=pending_backslashes {
                        quoted.push('\\');
                    }
                    pending_backslashes = 0;
                    quoted.push('"');
                },
                _ => {
                    for _ in 0..pending_backslashes {
                        quoted.push('\\');
                    }
                    pending_backslashes = 0;
                    quoted.push(ch);
                },
            }
        }
        // Trailing backslashes would escape the closing quote, so they are doubled too.
        for _ in 0..pending_backslashes * 2 {
            quoted.push('\\');
        }
        quoted.push('"');
        quoted
    }

    fn to_wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Run the executable to completion, or terminate it once the timeout passes.
    pub fn run(exe: &Path, args: &[String], timeout: Duration) -> io::Result<Outcome> {
        let application = to_wide(exe.as_os_str());

        let mut line = quote(&exe.to_string_lossy());
        for arg in args {
            line.push(' ');
            line.push_str(&quote(arg));
        }
        let mut command_line = to_wide(OsStr::new(&line));

        // SAFETY: both buffers stay alive for the whole call, the startup info is zeroed and
        // sized as the API requires, and every handle the call hands back is closed below.
        unsafe {
            let mut startup: STARTUPINFOW = std::mem::zeroed();
            startup.cb = size_of::<STARTUPINFOW>() as u32;
            startup.dwFlags = STARTF_USESHOWWINDOW;
            startup.wShowWindow = SW_HIDE;

            let mut info: PROCESS_INFORMATION = std::mem::zeroed();

            let started = CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0, // handles are not inherited: this process may be speaking MCP over stdio
                CREATE_NEW_CONSOLE,
                ptr::null(),
                ptr::null(),
                &startup,
                &mut info,
            );
            if started == 0 {
                return Err(io::Error::last_os_error());
            }

            let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
            let waited = WaitForSingleObject(info.hProcess, millis);

            let outcome = if waited == WAIT_OBJECT_0 {
                let mut code: u32 = 0;
                if GetExitCodeProcess(info.hProcess, &mut code) == 0 {
                    CloseHandle(info.hThread);
                    CloseHandle(info.hProcess);
                    return Err(io::Error::last_os_error());
                }
                Outcome::Exited(code as i32)
            } else {
                TerminateProcess(info.hProcess, 1);
                Outcome::TimedOut
            };

            CloseHandle(info.hThread);
            CloseHandle(info.hProcess);
            Ok(outcome)
        }
    }
}

/// Read the crash log Gaea left in the build directory, if any.
///
/// Only the first fault matters: everything downstream of a failed node reports that its input
/// returned no data, which buries the actual cause.
pub(crate) async fn read_crash_log(output_dir: &Path) -> Option<(String, Option<String>)> {
    let path = output_dir.join(CRASH_LOG);
    let text = tokio::fs::read_to_string(&path).await.ok()?;

    let first_fault = text
        .lines()
        .find(|line| line.contains("failed:"))
        .map(|line| line.trim().to_string());

    Some((text, first_fault))
}

/// CLI automation for running Gaea2 projects.
pub struct Gaea2CLI {
    gaea_path: PathBuf,
}

impl Gaea2CLI {
    /// Create a new CLI automation instance.
    pub fn new(gaea_path: PathBuf) -> Self {
        Self { gaea_path }
    }

    /// Run Gaea to completion and report how it ended.
    ///
    /// `Ok(Some(code))` is a finished build, `Ok(None)` a timeout.
    #[cfg(windows)]
    async fn run_to_completion(
        &self,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> std::io::Result<Option<i32>> {
        let exe = self.gaea_path.clone();
        let timeout = Duration::from_secs(timeout_secs);

        // The wait blocks, so it belongs off the async runtime's threads.
        tokio::task::spawn_blocking(move || match hidden_launch::run(&exe, &args, timeout)? {
            hidden_launch::Outcome::Exited(code) => Ok(Some(code)),
            hidden_launch::Outcome::TimedOut => Ok(None),
        })
        .await
        .map_err(std::io::Error::other)?
    }

    /// Run Gaea to completion and report how it ended.
    #[cfg(not(windows))]
    async fn run_to_completion(
        &self,
        args: Vec<String>,
        timeout_secs: u64,
    ) -> std::io::Result<Option<i32>> {
        let mut cmd = Command::new(&self.gaea_path);
        cmd.args(&args);
        match timeout(Duration::from_secs(timeout_secs), cmd.status()).await {
            Ok(status) => Ok(status?.code()),
            Err(_) => Ok(None),
        }
    }

    /// Run a Gaea2 project and generate terrain outputs.
    ///
    /// # Arguments
    /// * `project_path` - Path to the .terrain file
    /// * `resolution` - Build resolution (512, 1024, 2048, 4096, 8192)
    /// * `build_path` - Output directory (optional)
    /// * `profile` - Build profile name (optional)
    /// * `region` - Specific region to build (optional)
    /// * `seed` - Mutation seed for variations (optional)
    /// * `target_node` - Specific node index to target (optional)
    /// * `variables` - Variable name:value pairs (optional)
    /// * `ignore_cache` - Force rebuild ignoring cache
    /// * `verbose` - Enable verbose logging
    /// * `timeout_secs` - Maximum execution time in seconds
    pub async fn run_project(
        &self,
        project_path: &str,
        resolution: &str,
        build_path: Option<&str>,
        profile: Option<&str>,
        region: Option<&str>,
        seed: Option<i64>,
        target_node: Option<&str>,
        variables: Option<std::collections::HashMap<String, String>>,
        ignore_cache: bool,
        verbose: bool,
        timeout_secs: u64,
    ) -> ExecutionResult {
        let project_path = Path::new(project_path);

        // Verify project exists
        if !project_path.exists() {
            return ExecutionResult {
                success: false,
                error: Some(format!("Project file not found: {project_path:?}")),
                output_dir: None,
                output_files: vec![],
                file_count: 0,
                execution_time: None,
                note: None,
                stdout: None,
                stderr: None,
            };
        }

        // Determine output directory
        let output_dir = if let Some(bp) = build_path {
            PathBuf::from(bp)
        } else {
            let stem = project_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy();
            project_path
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!("output_{stem}"))
        };

        // Create output directory
        if let Err(e) = tokio::fs::create_dir_all(&output_dir).await {
            return ExecutionResult {
                success: false,
                error: Some(format!("Failed to create output directory: {e}")),
                output_dir: None,
                output_files: vec![],
                file_count: 0,
                execution_time: None,
                note: None,
                stdout: None,
                stderr: None,
            };
        }

        // Build the argument list
        let mut args: Vec<String> = vec![
            "--Filename".to_string(),
            project_path.to_string_lossy().to_string(),
            "--resolution".to_string(),
            resolution.to_string(),
            "--buildpath".to_string(),
            output_dir.to_string_lossy().to_string(),
            "--silent".to_string(), // Required for automation
        ];

        if let Some(p) = profile {
            args.push("--profile".to_string());
            args.push(p.to_string());
        }

        if let Some(r) = region {
            args.push("--region".to_string());
            args.push(r.to_string());
        }

        if let Some(s) = seed {
            args.push("--seed".to_string());
            args.push(s.to_string());
        }

        if let Some(n) = target_node {
            args.push("--node".to_string());
            args.push(n.to_string());
        }

        if let Some(vars) = variables {
            for (key, value) in vars {
                args.push("-v".to_string());
                args.push(format!("{key}:{value}"));
            }
        }

        if ignore_cache {
            args.push("--ignorecache".to_string());
        }

        if verbose {
            args.push("--verbose".to_string());
        }

        // Log the command as it will actually run. Printing a fixed subset of the flags made a
        // build look as if --node, --ignorecache and --verbose had been dropped on the way.
        tracing::info!("Running Gaea2: {:?} {}", self.gaea_path, args.join(" "));

        // What was already there and when it was written, so a rebuild into the same directory
        // is recognised as having produced something even though the names do not change.
        let before = output_stamps(&output_dir).await;

        let start_time = Instant::now();

        let result = self.run_to_completion(args, timeout_secs).await;

        let execution_time = start_time.elapsed().as_secs_f64();

        match result {
            Ok(Some(code)) => {
                let crash = read_crash_log(&output_dir).await;
                let crash_text = crash.as_ref().map(|(text, _)| text.clone());
                let first_fault = crash.as_ref().and_then(|(_, fault)| fault.clone());

                let after = output_stamps(&output_dir).await;
                let wrote_something = after
                    .iter()
                    .any(|(path, written)| before.get(path) != Some(written));
                let mut output_files: Vec<String> = after.into_keys().collect();
                output_files.sort();

                // Gaea can exit zero and still leave a crash log for a node that faulted, and it
                // can exit zero having computed nothing at all, so the exit code alone never
                // decides: a build succeeded when it produced a file and left no fault behind.
                if code == 0 && first_fault.is_none() && wrote_something {
                    let file_count = output_files.len();

                    ExecutionResult {
                        success: true,
                        error: None,
                        output_dir: Some(output_dir.to_string_lossy().to_string()),
                        output_files,
                        file_count,
                        execution_time: Some(execution_time),
                        note: None,
                        stdout: None,
                        stderr: crash_text,
                    }
                } else {
                    let file_count = output_files.len();
                    let error = match (&first_fault, code, wrote_something) {
                        (Some(fault), _, _) => format!("Gaea2 build failed: {fault}"),
                        (None, code, _) if code != 0 => format!(
                            "Gaea2 exited with code {}{}",
                            code,
                            if code == -532462766 {
                                " (unhandled .NET exception; see CRASH_LOG.txt in the build \
                                 directory)"
                            } else {
                                ""
                            }
                        ),
                        (None, _, false) => "Gaea2 finished without writing a single file and \
                                             without a crash log. The graph computed nothing: \
                                             check that the chain reaches the saved nodes and \
                                             that the nodes feeding them produce data."
                            .to_string(),
                        (None, _, true) => "Gaea2 reported a failure".to_string(),
                    };

                    ExecutionResult {
                        success: false,
                        error: Some(error),
                        output_dir: Some(output_dir.to_string_lossy().to_string()),
                        output_files,
                        file_count,
                        execution_time: Some(execution_time),
                        note: None,
                        stdout: None,
                        stderr: crash_text,
                    }
                }
            },
            Ok(None) => ExecutionResult {
                success: false,
                error: Some(format!("Process timed out after {timeout_secs} seconds")),
                output_dir: Some(output_dir.to_string_lossy().to_string()),
                output_files: find_output_files(&output_dir).await,
                file_count: 0,
                execution_time: Some(execution_time),
                note: None,
                stdout: None,
                stderr: None,
            },
            Err(e) => ExecutionResult {
                success: false,
                error: Some(format!("Failed to execute Gaea2: {e}")),
                output_dir: None,
                output_files: vec![],
                file_count: 0,
                execution_time: Some(execution_time),
                note: None,
                stdout: None,
                stderr: None,
            },
        }
    }

    /// Validate the Gaea2 installation.
    pub async fn validate_installation(&self) -> Result<String, String> {
        if !self.gaea_path.exists() {
            return Err(format!(
                "Gaea2 executable not found at {:?}",
                self.gaea_path
            ));
        }

        let output = Command::new(&self.gaea_path)
            .arg("--version")
            .output()
            .await
            .map_err(|e| format!("Failed to run Gaea2: {e}"))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Ok("Unknown version".to_string())
        }
    }
}

/// Find output files in a directory.
async fn find_output_files(dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = output_stamps(dir).await.into_keys().collect();
    files.sort();
    files
}

/// Output files with the moment each was last written.
///
/// Names alone cannot answer "did this build write anything": a rebuild into the same directory
/// overwrites the same names, so comparing name lists reports a successful build as having
/// produced nothing.
async fn output_stamps(dir: &Path) -> std::collections::HashMap<String, std::time::SystemTime> {
    const EXTENSIONS: [&str; 7] = ["exr", "png", "tiff", "tif", "raw", "r16", "r32"];
    let mut stamps = std::collections::HashMap::new();

    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Some(ext) = path.extension() else {
                continue;
            };
            if !EXTENSIONS.contains(&ext.to_string_lossy().to_lowercase().as_str()) {
                continue;
            }
            let written = entry
                .metadata()
                .await
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            stamps.insert(path.to_string_lossy().to_string(), written);
        }
    }

    stamps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_find_output_files_empty_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let files = find_output_files(temp_dir.path()).await;
        assert!(files.is_empty());
    }
}
