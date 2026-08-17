use anyhow::{bail, Context, Result};
use common::log::LogBuilder;
use std::path::Path;
use tokio::process::Command;
use tracing::{info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Registry, reload::Handle};

const LOG_FILE_NAME: &str = "arkkvm_app.log";

const COLLECT_EVIDENCE_SCRIPT: &str = "/oem/usr/bin/collect_evidence.sh";

pub fn init_log() -> Result<(Option<WorkerGuard>, Handle<EnvFilter, Registry>)> {
    LogBuilder::default()
        .set_output_file(true)
        .set_file_name(LOG_FILE_NAME)
        .build()
}

/// Print application logo with version
pub fn print_logo() {
    let version = super::get_app_version(true);
    info!(
        r#"
========================================================

     ╔════════════════════════════════════════════╗
     ║                                            ║
     ║     ___       _     _  ___     ____  __    ║
     ║    / _ \ _ __| | __| |/ \ \   / /  \/  |   ║
     ║   | |_| | '__| |/ /| ' / \ \ / /| |\/| |   ║
     ║   |  _  | |  |   < | . \  \ V / | |  | |   ║
     ║   |_| |_|_|  |_|\_\|_|\_\  \_/  |_|  |_|   ║
     ║                                            ║
     ║   Version: {:<31} ║
     ║   Power By Ensurebit                       ║
     ║                                            ║
     ╚════════════════════════════════════════════╝

======== The ArkKVM application is starting... =========
"#,
        version
    );
}

pub async fn collect_evidence() -> Result<String> {
    if !Path::new(COLLECT_EVIDENCE_SCRIPT).exists() {
        bail!("collect evidence script not found: {COLLECT_EVIDENCE_SCRIPT}");
    }

    info!("Running collect evidence script: {COLLECT_EVIDENCE_SCRIPT}");
    let output = Command::new(COLLECT_EVIDENCE_SCRIPT)
        .output()
        .await
        .with_context(|| format!("failed to execute {COLLECT_EVIDENCE_SCRIPT}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        let code = output.status.code().unwrap_or(-1);
        warn!(
            "collect_evidence failed (exit {code}): {detail}"
        );
        bail!("collect_evidence failed (exit {code}): {detail}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let filename = parse_archive_filename(&stdout)?;
    if !filename.ends_with(".zip") && !filename.ends_with(".tgz") {
        bail!("unexpected collect_evidence output filename: {filename}");
    }

    info!("collect_evidence completed: {filename}");
    Ok(filename)
}

fn parse_archive_filename(stdout: &str) -> Result<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .last()
        .map(str::to_string)
        .context("collect_evidence produced no output")
}