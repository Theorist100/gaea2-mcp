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

/// Quote one argument for cmd.exe.
#[cfg(windows)]
fn quote_for_cmd(arg: &str) -> String {
    format!("\"{}\"", arg.replace('"', "\"\""))
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

    /// Build the command that launches Gaea with a console of its own.
    ///
    /// On Windows the launch goes through `cmd /c start`, and that indirection is the whole
    /// point. Gaea.Swarm uses console APIs, so it needs standard handles that belong to a real
    /// console. Rust always fills STARTUPINFO with the parent's handles - pipes, when this
    /// process is speaking MCP over stdio - and CREATE_NEW_CONSOLE does not override handles
    /// that were passed explicitly; Gaea then dies with
    /// `System.IO.IOException: The handle is invalid` (exit code -532462766) once the project is
    /// already loaded, which reads like a broken project rather than a broken invocation.
    /// `start` opens the process in a window of its own, with handles to match, and `/wait`
    /// passes its exit code back.
    #[cfg(windows)]
    fn spawn_command(&self, args: &[String]) -> Command {
        use std::os::windows::process::CommandExt;

        let quoted: Vec<String> = args.iter().map(|a| quote_for_cmd(a)).collect();
        let line = format!(
            "/c start \"Gaea\" /wait /min {} {}",
            quote_for_cmd(&self.gaea_path.to_string_lossy()),
            quoted.join(" ")
        );

        let mut cmd = Command::new("cmd.exe");
        // raw_arg: cmd.exe parses its own command line, so the usual argument escaping would
        // reach it doubled and `start` would take the executable path for a window title.
        cmd.raw_arg(line);
        cmd
    }

    /// Build the command that launches Gaea.
    #[cfg(not(windows))]
    fn spawn_command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.gaea_path);
        cmd.args(args);
        cmd
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

        // What was already there, so "did this build write anything" can be answered by
        // difference rather than by an exit code that does not survive the launch.
        let files_before = find_output_files(&output_dir).await;

        let mut cmd = self.spawn_command(&args);

        let start_time = Instant::now();

        // Run with timeout
        let result = timeout(Duration::from_secs(timeout_secs), cmd.status()).await;

        let execution_time = start_time.elapsed().as_secs_f64();

        match result {
            Ok(Ok(status)) => {
                let crash = read_crash_log(&output_dir).await;
                let crash_text = crash.as_ref().map(|(text, _)| text.clone());
                let first_fault = crash.as_ref().and_then(|(_, fault)| fault.clone());

                let output_files = find_output_files(&output_dir).await;
                let wrote_something = output_files.iter().any(|f| !files_before.contains(f));

                // The exit code cannot be trusted here. Gaea needs its own console, which it
                // gets through `cmd /c start /wait`, and `start` reports success regardless of
                // what the child returned - a build that exits 1 arrives as 0. So a build counts
                // as successful only when it actually produced a file and left no crash log.
                if status.success() && first_fault.is_none() && wrote_something {
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
                    let error = match (&first_fault, status.code(), wrote_something) {
                        (Some(fault), _, _) => format!("Gaea2 build failed: {fault}"),
                        (None, Some(code), _) if code != 0 => format!(
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
                        (None, None, _) => "Gaea2 was terminated by a signal".to_string(),
                        (None, Some(_), true) => "Gaea2 reported a failure".to_string(),
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
            Ok(Err(e)) => ExecutionResult {
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
            Err(_) => ExecutionResult {
                success: false,
                error: Some(format!("Process timed out after {timeout_secs} seconds")),
                output_dir: Some(output_dir.to_string_lossy().to_string()),
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
    let mut files = Vec::new();

    let extensions = ["exr", "png", "tiff", "tif", "raw", "r16", "r32"];

    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                let ext_lower = ext.to_string_lossy().to_lowercase();
                if extensions.contains(&ext_lower.as_str()) {
                    files.push(path.to_string_lossy().to_string());
                }
            }
        }
    }

    files.sort();
    files
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
