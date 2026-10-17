//! Background jobs: long-running commands that outlive one tool round.
//!
//! A dev server, a watch task, a slow build — running them through the
//! foreground `run_command` blocks the agent loop for the whole duration (and
//! usually hits the timeout). Here they get a job id, a capped output tail and
//! an exit status, and the model can poll them with `job_output` between
//! rounds. The table lives in [`super::ToolCtx`], so a provider switch keeps
//! the jobs; dropping the last reference kills what is still running (no
//! orphaned servers after kazi exits).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Keep at most this much output per job (stdout + stderr interleaved).
const OUTPUT_CAP: usize = 256 * 1024;
/// How often the waiter checks `try_wait` so `kill` can act.
const POLL: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Exited(i32),
    /// `job_kill` (or the table shutting down) terminated it.
    Killed,
    Failed(String),
}

impl JobStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Running => "running".to_string(),
            Self::Exited(0) => "exited 0".to_string(),
            Self::Exited(code) => format!("exited {code} (failed)"),
            Self::Killed => "killed".to_string(),
            Self::Failed(e) => format!("failed: {e}"),
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// One running or finished command.
pub struct Job {
    pub id: u64,
    pub command: String,
    pub cwd: std::path::PathBuf,
    started: Instant,
    status: Mutex<JobStatus>,
    output: Mutex<Vec<u8>>,
    child: Mutex<Option<tokio::process::Child>>,
}

impl Job {
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn status(&self) -> JobStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The last `lines` lines of combined output, plus whether it was truncated.
    pub fn tail(&self, lines: usize) -> (String, bool) {
        let buf = self.output.lock().unwrap_or_else(|e| e.into_inner());
        let text = String::from_utf8_lossy(&buf).into_owned();
        let all: Vec<&str> = text.lines().collect();
        let keep = lines.max(1);
        let start = all.len().saturating_sub(keep);
        (all[start..].join("\n"), start > 0)
    }

    /// Total bytes still buffered (capped).
    pub fn output_len(&self) -> usize {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn push(&self, bytes: &[u8]) {
        let mut buf = self.output.lock().unwrap_or_else(|e| e.into_inner());
        buf.extend_from_slice(bytes);
        if buf.len() > OUTPUT_CAP {
            let excess = buf.len() - OUTPUT_CAP;
            buf.drain(0..excess);
        }
    }
}

/// All jobs of one session. Cheap to clone via `Arc`; the last drop kills the
/// still-running children.
#[derive(Default)]
pub struct JobTable {
    next: AtomicU64,
    jobs: Mutex<VecDeque<Arc<Job>>>,
}

impl JobTable {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn list(&self) -> Vec<Arc<Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn get(&self, id: u64) -> Option<Arc<Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    pub fn running(&self) -> usize {
        self.list()
            .iter()
            .filter(|j| j.status().is_running())
            .count()
    }

    /// Spawn `command` in `cwd` and return its job. The child's stdout and
    /// stderr are drained into a capped buffer by background tasks, so a chatty
    /// server cannot grow memory without bound.
    pub fn spawn(
        &self,
        command: &str,
        cwd: &std::path::Path,
        timeout: Duration,
    ) -> std::io::Result<Arc<Job>> {
        let mut cmd = super::shell::build_command(command, cwd);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());
        let mut child = cmd.spawn()?;
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let job = Arc::new(Job {
            id,
            command: command.to_string(),
            cwd: cwd.to_path_buf(),
            started: Instant::now(),
            status: Mutex::new(JobStatus::Running),
            output: Mutex::new(Vec::new()),
            child: Mutex::new(None),
        });

        // drain both pipes into the same buffer (order is arrival order)
        if let Some(stdout) = child.stdout.take() {
            let job2 = job.clone();
            tokio::spawn(async move {
                drain(stdout, job2).await;
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let job2 = job.clone();
            tokio::spawn(async move {
                drain(stderr, job2).await;
            });
        }

        *job.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(job.clone());

        // waiter: poll try_wait so `kill` can take the same lock
        let job2 = job.clone();
        tokio::spawn(async move {
            loop {
                {
                    let mut guard = job2.child.lock().unwrap_or_else(|e| e.into_inner());
                    let Some(child) = guard.as_mut() else { break };
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            let mut st = job2.status.lock().unwrap_or_else(|e| e.into_inner());
                            // `Killed` set by `kill` wins over the exit code
                            if st.is_running() {
                                *st = JobStatus::Exited(status.code().unwrap_or(-1));
                            }
                            *guard = None;
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let mut st = job2.status.lock().unwrap_or_else(|e| e.into_inner());
                            if st.is_running() {
                                *st = JobStatus::Failed(e.to_string());
                            }
                            *guard = None;
                            break;
                        }
                    }
                }
                if job2.elapsed() > timeout {
                    let mut st = job2.status.lock().unwrap_or_else(|e| e.into_inner());
                    if st.is_running() {
                        *st = JobStatus::Failed(format!(
                            "still running after {}s (tools.command_timeout_secs); job_kill to stop it",
                            timeout.as_secs()
                        ));
                    }
                }
                tokio::time::sleep(POLL).await;
            }
        });
        Ok(job)
    }

    /// Terminate a running job (and, on Windows, its whole process tree).
    ///
    /// Returns false when it was already finished. `cmd /C some-server` starts
    /// a CHILD process: killing the shell alone would leave the server holding
    /// the port, which is exactly the failure this tool exists to prevent, so
    /// Windows gets a `taskkill /T` on the shell's pid.
    pub fn kill(&self, id: u64) -> bool {
        let Some(job) = self.get(id) else {
            return false;
        };
        let mut guard = job.child.lock().unwrap_or_else(|e| e.into_inner());
        let Some(child) = guard.as_mut() else {
            return false;
        };
        let pid = child.id();
        let killed = child.start_kill().is_ok();
        if killed {
            kill_tree(pid);
            *job.status.lock().unwrap_or_else(|e| e.into_inner()) = JobStatus::Killed;
            *guard = None;
        }
        killed
    }
}

/// Windows: terminate the process tree rooted at `pid` (best effort — the
/// direct kill already happened). A no-op elsewhere: `sh -c` usually execs a
/// single command, so the direct kill reaches it.
fn kill_tree(pid: Option<u32>) {
    #[cfg(windows)]
    if let Some(pid) = pid {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut c = std::process::Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        let _ = c.status();
    }
    #[cfg(not(windows))]
    let _ = pid;
}

impl Drop for JobTable {
    fn drop(&mut self) {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs.iter() {
            if let Some(child) = job.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

async fn drain<R>(mut reader: R, job: Arc<Job>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => job.push(&buf[..n]),
        }
    }
}

/// `job_output`: status plus the tail of a background job.
pub struct JobOutput;

#[async_trait::async_trait]
impl super::Tool for JobOutput {
    fn name(&self) -> &str {
        "job_output"
    }

    fn description(&self) -> &str {
        "Read a background job's status and the tail of its output. Use it after run_command with background=true (or to check whether a started server/build is still alive). The buffer keeps the last 256 KiB."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "description": "Job id returned by run_command"},
                "lines": {"type": "integer", "description": "How many trailing lines to show (default 80, max 400)"}
            },
            "required": ["id"]
        })
    }

    async fn run(
        &self,
        args: &serde_json::Value,
        ctx: &super::ToolCtx,
    ) -> Result<String, super::ToolError> {
        let Some(table) = &ctx.jobs else {
            return Err(super::ToolError::new(
                "background jobs are not available in this configuration",
            ));
        };
        let id = args
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| super::ToolError::new("missing or non-integer argument `id`"))?;
        let Some(job) = table.get(id) else {
            let running = table
                .list()
                .iter()
                .filter(|j| j.status().is_running())
                .map(|j| format!("#{}", j.id))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(super::ToolError::new(format!(
                "no job #{id}; running: {}",
                if running.is_empty() { "none" } else { &running }
            )));
        };
        let lines = args
            .get("lines")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(80)
            .clamp(1, 400);
        let (tail, truncated) = job.tail(lines);
        Ok(format!(
            "#{} {}\ncommand: {}\ncwd: {}\noutput ({} bytes{}{}):\n{}",
            job.id,
            job.status().label(),
            job.command,
            job.cwd.display(),
            job.output_len(),
            if truncated {
                format!(", last {lines} lines")
            } else {
                String::new()
            },
            if job.status().is_running() {
                "\n(still running — poll again, or job_kill to stop it)"
            } else {
                ""
            },
            if tail.trim().is_empty() {
                "(no output yet)"
            } else {
                &tail
            }
        ))
    }
}

/// `job_kill`: stop a running background job.
pub struct JobKill;

#[async_trait::async_trait]
impl super::Tool for JobKill {
    fn name(&self) -> &str {
        "job_kill"
    }

    fn description(&self) -> &str {
        "Terminate a running background job started by run_command with background=true. Use it before starting a second server on the same port."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "Job id"}},
            "required": ["id"]
        })
    }

    async fn run(
        &self,
        args: &serde_json::Value,
        ctx: &super::ToolCtx,
    ) -> Result<String, super::ToolError> {
        let Some(table) = &ctx.jobs else {
            return Err(super::ToolError::new(
                "background jobs are not available in this configuration",
            ));
        };
        let id = args
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| super::ToolError::new("missing or non-integer argument `id`"))?;
        match table.get(id) {
            None => Err(super::ToolError::new(format!("no job #{id}"))),
            Some(job) if !job.status().is_running() => {
                Ok(format!("#{id} already finished ({})", job.status().label()))
            }
            Some(_) => {
                if table.kill(id) {
                    Ok(format!("#{id} killed"))
                } else {
                    Err(super::ToolError::new(format!(
                        "#{id} could not be killed (already exited?)"
                    )))
                }
            }
        }
    }
}

/// One-line description for `/jobs` and tool text.
pub fn describe(job: &Job) -> String {
    let mut line = job.command.lines().next().unwrap_or("").to_string();
    if line.chars().count() > 70 {
        line = line.chars().take(67).collect::<String>() + "...";
    }
    format!(
        "#{:>2}  {:<12}  {:>6.1}s  {line}",
        job.id,
        job.status().label(),
        job.elapsed().as_secs_f64()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Arc<JobTable> {
        JobTable::new()
    }

    #[tokio::test]
    async fn captures_output_and_reports_the_exit_code() {
        let t = table();
        let job = t
            .spawn(
                "echo hello-from-a-job",
                std::path::Path::new("."),
                Duration::from_secs(20),
            )
            .unwrap();
        // poll until the waiter notices the exit
        for _ in 0..100 {
            if !job.status().is_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(job.status(), JobStatus::Exited(0), "{:?}", job.status());
        let (tail, _) = job.tail(20);
        assert!(tail.contains("hello-from-a-job"), "{tail:?}");
        assert_eq!(t.running(), 0);
    }

    #[tokio::test]
    async fn kill_stops_a_long_running_job() {
        let t = table();
        // a SINGLE long-lived process on both platforms: killing a shell whose
        // child survives would leave the pipe open and delay the test
        let cmd = if cfg!(windows) {
            "powershell -NoProfile -Command \"Start-Sleep -Seconds 30\""
        } else {
            "sleep 30"
        };
        let job = t
            .spawn(cmd, std::path::Path::new("."), Duration::from_secs(60))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(job.status().is_running(), "job should still be running");
        assert!(t.kill(job.id), "kill returns true while running");
        for _ in 0..60 {
            if !job.status().is_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(job.status(), JobStatus::Killed);
        assert!(!t.kill(job.id), "a finished job cannot be killed twice");
    }

    #[tokio::test]
    async fn output_is_capped() {
        let job = Job {
            id: 1,
            command: "x".into(),
            cwd: std::path::PathBuf::from("."),
            started: Instant::now(),
            status: Mutex::new(JobStatus::Running),
            output: Mutex::new(Vec::new()),
            child: Mutex::new(None),
        };
        // ~1 KiB per line, a few more lines than the cap holds
        for i in 0..(OUTPUT_CAP / 1024 + 8) {
            job.push(format!("line {i} {}\n", "x".repeat(1000)).as_bytes());
        }
        job.push(b"THE-END\n");
        assert!(job.output_len() <= OUTPUT_CAP, "the buffer is capped");
        let (tail, truncated) = job.tail(10);
        assert!(truncated, "older lines must be reported as dropped");
        assert!(tail.ends_with("THE-END"), "newest output survives");
        assert!(!tail.contains("line 0 "), "oldest output is gone");
    }
}
