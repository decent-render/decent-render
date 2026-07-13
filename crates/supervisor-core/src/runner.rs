use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use crate::protocol::{
    JobAssignMessage, JobCompleteMessage, JobFailedMessage, JobMetrics, JobProgressMessage,
    WorkerMessage,
};
use crate::purge::WorkDir;

const SILENCE_TIMEOUT: Duration = Duration::from_secs(120);
const CANCEL_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct InFlightJob {
    pub job_id: String,
    pub cancel: oneshot::Sender<()>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum RunnerEvent {
    Progress {
        progress: f64,
    },
    Done {
        #[serde(rename = "outputSizeInBytes")]
        output_size_in_bytes: u64,
        #[serde(rename = "wallTimeMs")]
        wall_time_ms: u64,
        metrics: Option<JobMetrics>,
    },
    Error {
        message: String,
    },
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub async fn ensure_payload(assign: &JobAssignMessage) -> anyhow::Result<PathBuf> {
    let dir = home_dir()?
        .join(".decent-worker/payloads")
        .join(&assign.payload_sha256);
    let runner = dir.join("decent-render-runner");
    if runner.exists() {
        tracing::info!(payload = %assign.payload_sha256, path = %dir.display(), "payload cached");
        return Ok(dir);
    }

    let parent = dir
        .parent()
        .ok_or_else(|| anyhow!("payload dir has no parent"))?
        .to_path_buf();
    tokio::fs::create_dir_all(&parent).await?;
    let tmp = parent.join(format!(".{}-download", assign.payload_sha256));
    if tmp.exists() {
        tokio::fs::remove_dir_all(&tmp).await.ok();
    }
    tokio::fs::create_dir_all(&tmp).await?;

    tracing::info!(payload = %assign.payload_sha256, "downloading payload");
    let bytes = reqwest::get(&assign.payload_get_url)
        .await
        .context("payload download request failed")?
        .error_for_status()
        .context("payload download returned non-2xx")?
        .bytes()
        .await
        .context("payload body read failed")?;
    let actual = sha256_hex(&bytes);
    if actual != assign.payload_sha256 {
        tokio::fs::remove_dir_all(&tmp).await.ok();
        return Err(anyhow!(
            "payload sha mismatch: expected {}, got {}",
            assign.payload_sha256,
            actual
        ));
    }
    let tar_path = tmp.join("payload.tar.gz");
    tokio::fs::write(&tar_path, &bytes).await?;

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tar_path)
        .arg("-C")
        .arg(&tmp)
        .status()
        .await
        .context("failed to spawn tar for payload extract")?;
    if !status.success() {
        tokio::fs::remove_dir_all(&tmp).await.ok();
        return Err(anyhow!("payload tar extract failed with {status}"));
    }
    tokio::fs::remove_file(&tar_path).await.ok();
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
    tokio::fs::rename(&tmp, &dir)
        .await
        .or_else(|_| std::fs::rename(&tmp, &dir).map_err(anyhow::Error::from))?;
    tracing::info!(payload = %assign.payload_sha256, path = %dir.display(), "payload extracted");
    Ok(dir)
}

pub async fn run_job(
    assign: JobAssignMessage,
    mut cancel_rx: oneshot::Receiver<()>,
    tx: tokio::sync::mpsc::UnboundedSender<WorkerMessage>,
) {
    let job_id = assign.job_id.clone();
    let tenant = assign.tenant.clone();
    let output_key = assign.output_key.clone();
    let attempt = assign.attempt;
    match run_job_inner(assign, &mut cancel_rx, tx.clone()).await {
        Ok(metrics) => {
            let _ = tx.send(WorkerMessage::JobComplete(JobCompleteMessage {
                tenant,
                job_id,
                attempt,
                output_key,
                metrics,
            }));
        }
        Err(err) => {
            let _ = tx.send(WorkerMessage::JobFailed(JobFailedMessage {
                tenant,
                job_id,
                attempt,
                reason: err.to_string(),
            }));
        }
    }
}

async fn run_job_inner(
    assign: JobAssignMessage,
    cancel_rx: &mut oneshot::Receiver<()>,
    tx: tokio::sync::mpsc::UnboundedSender<WorkerMessage>,
) -> anyhow::Result<JobMetrics> {
    let payload_dir = ensure_payload(&assign).await?;
    let runner = payload_dir.join("decent-render-runner");
    if !runner.exists() {
        return Err(anyhow!(
            "payload missing decent-render-runner at {}",
            runner.display()
        ));
    }
    let workdir = WorkDir::new(&format!("job-{}", assign.job_id)).context("create workdir")?;
    let purged_path = workdir.path().to_path_buf();
    let mut child = Command::new(&runner)
        .current_dir(workdir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn runner {}", runner.display()))?;

    let mut stdin = child.stdin.take().context("runner stdin missing")?;
    let mut input_frame = serde_json::to_value(&assign)?;
    if let serde_json::Value::Object(ref mut obj) = input_frame {
        obj.insert("type".into(), serde_json::Value::String("jobAssign".into()));
    }
    let input = serde_json::to_vec(&input_frame)?;
    stdin.write_all(&input).await?;
    stdin.shutdown().await?;
    drop(stdin);

    let stdout = child.stdout.take().context("runner stdout missing")?;
    let mut lines = BufReader::new(stdout).lines();
    let mut done_metrics: Option<JobMetrics> = None;

    loop {
        tokio::select! {
            _ = &mut *cancel_rx => {
                terminate_child(&mut child).await;
                drop(workdir);
                tracing::info!(job_id = %assign.job_id, purged = !purged_path.exists(), "runner canceled and workdir purged");
                return Err(anyhow!("Render canceled by dispatch"));
            }
            line = tokio::time::timeout(SILENCE_TIMEOUT, lines.next_line()) => {
                let line = match line {
                    Err(_) => {
                        terminate_child(&mut child).await;
                        drop(workdir);
                        tracing::warn!(job_id = %assign.job_id, purged = !purged_path.exists(), "runner silent and workdir purged");
                        return Err(anyhow!("runner silent"));
                    }
                    Ok(Err(e)) => return Err(anyhow!(e).context("runner stdout read failed")),
                    Ok(Ok(None)) => break,
                    Ok(Ok(Some(line))) => line,
                };
                let event: RunnerEvent = match serde_json::from_str(&line) {
                    Ok(event) => event,
                    Err(_) => {
                        tracing::warn!(job_id = %assign.job_id, line = %line, "ignoring non-NDJSON runner stdout line");
                        continue;
                    }
                };
                match event {
                    RunnerEvent::Progress { progress } => {
                        let _ = tx.send(WorkerMessage::JobProgress(JobProgressMessage {
                            tenant: assign.tenant.clone(),
                            job_id: assign.job_id.clone(),
                            attempt: assign.attempt,
                            progress,
                        }));
                    }
                    RunnerEvent::Done { output_size_in_bytes, wall_time_ms, metrics } => {
                        tracing::info!(job_id = %assign.job_id, output_size_in_bytes, wall_time_ms, "runner done");
                        let mut m = metrics.unwrap_or(JobMetrics {
                            wall_ms: wall_time_ms,
                            frames: assign.duration_frames,
                            output_size_in_bytes: None,
                        });
                        // The `done` envelope always carries the output size;
                        // stamp it onto the metrics so dispatch persists it.
                        m.output_size_in_bytes = Some(output_size_in_bytes);
                        done_metrics = Some(m);
                    }
                    RunnerEvent::Error { message } => return Err(anyhow!(message)),
                }
            }
        }
    }

    let status = child.wait().await.context("runner wait failed")?;
    drop(workdir);
    tracing::info!(job_id = %assign.job_id, purged = !purged_path.exists(), "workdir purged after runner exit");
    if !status.success() {
        return Err(anyhow!("runner exited with {status}"));
    }
    done_metrics.ok_or_else(|| anyhow!("runner exited without done event"))
}

async fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .await;
    }
    #[cfg(not(unix))]
    let _ = child.start_kill();

    match tokio::time::timeout(CANCEL_GRACE, child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

#[allow(dead_code)]
fn _assert_path(_: &Path) {}
