//! Bindings for `shell/S41netbird` — execution in the init script, parsing here.
//!
//! NetBird is a WireGuard-based mesh VPN whose control plane (management, signal,
//! relay) is open source and self-hostable. The init script mirrors the
//! `S40tailscale` contract (subcommands + exit codes) so both plugins are driven
//! the same way.

use std::net::IpAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::Instant;

/// Init script path on device.
pub const INIT_SCRIPT_PATH: &str = "/etc/init.d/S41netbird";

/// `ERROR_CODE_APP_NOT_FOUND` in the shell script.
pub const EXIT_BINARY_NOT_FOUND: i32 = 1;

/// `ERROR_CODE_DISABLE` in the shell script.
pub const EXIT_DISABLED: i32 = 2;

/// Printed by the script before exiting with [`EXIT_BINARY_NOT_FOUND`].
///
/// `netbird` itself also exits with 1 on errors, so the code alone is ambiguous.
const BINARY_NOT_FOUND_MSG: &str = "NetBird binary not found";

/// Environment variable read by `netbird up` (keeps the key out of argv).
const SETUP_KEY_ENV: &str = "NB_SETUP_KEY";

/// Max wait for an SSO URL or a completed setup-key join during `register`.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(45);

/// Max time to keep `netbird up` running after returning an SSO URL.
const REGISTER_DETACH_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// NetBird hostnames end up as DNS labels on the management side.
const MAX_HOSTNAME_LEN: usize = 63;

// ---------------------------------------------------------------------------
// Raw output
// ---------------------------------------------------------------------------

/// Result of invoking the init script.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.code == 0
    }

    pub fn combined_log(&self) -> String {
        join_log(self.stdout.trim(), self.stderr.trim())
    }

    fn binary_missing(&self) -> bool {
        self.code == EXIT_BINARY_NOT_FOUND
            && (self.stdout.contains(BINARY_NOT_FOUND_MSG) || self.stderr.contains(BINARY_NOT_FOUND_MSG))
    }

    /// Map shell exit codes to `Result` (see `S41netbird`).
    pub fn into_result(self, context: &str) -> Result<Self> {
        if self.success() {
            return Ok(self);
        }
        if self.binary_missing() {
            return Err(anyhow!("{context}: netbird binary not found"));
        }
        if self.code == EXIT_DISABLED {
            return Err(anyhow!("{context}: netbird is disabled"));
        }
        Err(anyhow!("{context} failed (exit {}): {}", self.code, self.combined_log()))
    }
}

fn join_log(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn stdout_lines(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Single-value getters print one line, or nothing when unset.
fn optional_value(stdout: &str) -> Option<String> {
    let value = stdout.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// NetBird emits `null` for empty lists (e.g. `"details": null` without peers);
/// `#[serde(default)]` only covers *missing* fields, so map `null` to the default too.
fn null_as_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    use serde::Deserialize;
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

/// Management URL for a self-hosted control plane (`None` = NetBird Cloud).
pub fn validate_management_url(url: &str) -> Result<()> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(anyhow!("management URL must start with https:// or http://"));
    }
    let host = url.split_once("://").map(|(_, rest)| rest).unwrap_or_default();
    if host.is_empty() || host.starts_with('/') {
        return Err(anyhow!("management URL is missing a host"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(anyhow!("management URL must not contain whitespace"));
    }
    Ok(())
}

/// Peer hostname: letters, digits, `-` and `.`, not starting with `-` or `.`.
pub fn validate_hostname(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_HOSTNAME_LEN {
        return Err(anyhow!("hostname must be 1-{MAX_HOSTNAME_LEN} characters"));
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err(anyhow!("hostname must not start with '-' or '.'"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        return Err(anyhow!("hostname may only contain letters, digits, '-' and '.'"));
    }
    Ok(())
}

/// Setup keys are UUID-like tokens; only reject obviously broken input.
pub fn validate_setup_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(anyhow!("setup key must not be empty"));
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(anyhow!("setup key must not contain whitespace"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Parsed results (from script / CLI stdout)
// ---------------------------------------------------------------------------

/// `getEnable` → `enable` | `disable`
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EnableState {
    Enabled,
    Disabled,
}

impl EnableState {
    pub fn parse_stdout(stdout: &str) -> Result<Self> {
        match stdout.trim() {
            "enable" => Ok(Self::Enabled),
            "disable" => Ok(Self::Disabled),
            other => Err(anyhow!("unexpected getEnable output: {other:?}")),
        }
    }
}

/// Outcome of `register` / `registerForce` (`netbird up`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RegisterStatus {
    /// Peer joined / reconnected (exit 0, no SSO URL in output).
    Completed,
    /// SSO URL was printed; user must finish login in the browser.
    NeedsAuth,
    /// `S41netbird` reported NetBird disabled (exit 2).
    Disabled,
    /// Exited with an error (e.g. invalid setup key) or timed out.
    Failed,
}

/// `register` / `registerForce` (`netbird up --no-browser` stdout/stderr).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterResult {
    pub status: RegisterStatus,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    /// SSO login URL (when [`RegisterStatus::NeedsAuth`]).
    pub auth_url: Option<String>,
    /// Device-flow user code, when the IdP does not embed it in the URL.
    pub user_code: Option<String>,
}

impl RegisterResult {
    pub fn from_output(out: CommandOutput) -> Result<Self> {
        if out.binary_missing() {
            return Err(anyhow!("register: netbird binary not found"));
        }

        let auth = merged_auth(&out.stdout, &out.stderr);
        let stdout = out.stdout.trim().to_string();
        let stderr = out.stderr.trim().to_string();

        let status = if out.code == EXIT_DISABLED {
            RegisterStatus::Disabled
        } else if auth.is_some() {
            RegisterStatus::NeedsAuth
        } else if out.success() {
            RegisterStatus::Completed
        } else {
            RegisterStatus::Failed
        };

        let (auth_url, user_code) = match auth {
            Some(SsoPrompt { url, user_code }) => (Some(url), user_code),
            None => (None, None),
        };

        Ok(Self { status, code: out.code, stdout, stderr, auth_url, user_code })
    }

    pub fn combined_log(&self) -> String {
        join_log(&self.stdout, &self.stderr)
    }

    pub fn needs_auth(&self) -> bool {
        self.status == RegisterStatus::NeedsAuth
    }

    pub fn is_completed(&self) -> bool {
        self.status == RegisterStatus::Completed
    }
}

/// `getIp` (`netbird status --ipv4` / `--ipv6`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IpListResult {
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
}

impl IpListResult {
    pub fn parse_stdout(stdout: &str) -> Result<Self> {
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();

        for line in stdout_lines(stdout) {
            for token in line.split_whitespace() {
                // Tolerate CIDR notation (`100.92.1.2/16`).
                let addr = token.split('/').next().unwrap_or(token);
                match addr.parse::<IpAddr>() {
                    Ok(IpAddr::V4(a)) => ipv4.push(a.to_string()),
                    Ok(IpAddr::V6(a)) => ipv6.push(a.to_string()),
                    Err(_) => {}
                }
            }
        }

        if ipv4.is_empty() && ipv6.is_empty() {
            return Err(anyhow!("unexpected getIp output: {stdout:?}"));
        }
        Ok(Self { ipv4, ipv6 })
    }
}

/// Local connection state derived from `netbird status --json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionState {
    Connected,
    Connecting,
    LoggedOut,
    Offline,
    #[default]
    Unknown,
}

/// `getStatus` (`netbird status --json`) — subset of NetBird's `OutputOverview` (camelCase).
///
/// Unknown fields are ignored and missing ones default, so older/newer clients still parse.
/// The untouched JSON is returned alongside as `raw`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StatusResult {
    #[serde(deserialize_with = "null_as_default")]
    pub peers: StatusPeers,
    pub cli_version: String,
    pub daemon_version: String,
    /// `Idle` | `Connecting` | `Connected` | `NeedsLogin` | `LoginFailed` | `SessionExpired`.
    /// Empty on clients that predate the field.
    pub daemon_status: String,
    #[serde(deserialize_with = "null_as_default")]
    pub management: StatusEndpoint,
    #[serde(deserialize_with = "null_as_default")]
    pub signal: StatusEndpoint,
    #[serde(deserialize_with = "null_as_default")]
    pub relays: StatusRelays,
    /// Peer address in CIDR notation, e.g. `100.92.1.2/16`.
    pub netbird_ip: String,
    pub netbird_ipv6: String,
    pub public_key: String,
    pub uses_kernel_interface: bool,
    pub fqdn: String,
    pub networks: Option<Vec<String>>,
    pub profile_name: String,
    /// When the SSO login of this peer expires (RFC 3339). `None` for setup-key
    /// peers or when login expiration is disabled in the management server.
    pub session_expires_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StatusEndpoint {
    pub url: String,
    pub connected: bool,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StatusRelays {
    pub total: u32,
    pub available: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StatusPeers {
    pub total: u32,
    pub connected: u32,
    #[serde(deserialize_with = "null_as_default")]
    pub details: Vec<StatusPeer>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StatusPeer {
    pub fqdn: String,
    pub netbird_ip: String,
    pub public_key: String,
    /// `Idle` | `Connecting` | `Connected`.
    pub status: String,
    /// `P2P` | `Relayed` | empty.
    pub connection_type: String,
    pub last_status_update: String,
    pub last_wireguard_handshake: String,
    pub transfer_received: i64,
    pub transfer_sent: i64,
    /// Nanoseconds (Go `time.Duration`).
    pub latency: i64,
}

impl StatusResult {
    pub fn parse_json(json: &str) -> Result<Self> {
        serde_json::from_str(json.trim()).context("parse netbird status --json")
    }

    /// Assigned NetBird address without prefix length.
    pub fn ip(&self) -> Option<String> {
        let ip = self.netbird_ip.split('/').next().unwrap_or_default();
        (!ip.is_empty()).then(|| ip.to_string())
    }

    /// Peer FQDN without trailing dot.
    pub fn hostname(&self) -> Option<String> {
        let fqdn = self.fqdn.trim_end_matches('.');
        (!fqdn.is_empty()).then(|| fqdn.to_string())
    }

    pub fn compute_connection_state(&self) -> ConnectionState {
        match self.daemon_status.as_str() {
            "Connected" => ConnectionState::Connected,
            "Connecting" => ConnectionState::Connecting,
            "NeedsLogin" | "LoginFailed" | "SessionExpired" => ConnectionState::LoggedOut,
            // Daemon is up but the engine was stopped (`netbird down`) or not started yet.
            "Idle" => ConnectionState::Offline,
            // Older clients without `daemonStatus`: infer from management + IP.
            "" if self.management.connected && self.ip().is_some() => ConnectionState::Connected,
            "" if self.ip().is_some() => ConnectionState::Connecting,
            _ => ConnectionState::Unknown,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.compute_connection_state() == ConnectionState::Connected
    }
}

// ---------------------------------------------------------------------------
// SSO prompt parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct SsoPrompt {
    url: String,
    user_code: Option<String>,
}

/// Parse the device-flow prompt printed by `netbird up --no-browser`:
///
/// ```text
/// Use this URL to log in:
///
/// https://idp.example.com/device?user_code=ABCD-EFGH  [and enter the code ABCD-EFGH to authenticate.]
/// ```
fn extract_sso_prompt(text: &str) -> Option<SsoPrompt> {
    // Both CLI variants print "... use this URL to log in:" right before the URL.
    // Requiring the marker avoids picking up unrelated links (e.g. setup-key docs in errors).
    let marker = text.find("Use this URL to log in").or_else(|| text.find("use this URL to log in"))?;
    let tail = &text[marker..];

    let start = tail.find("https://").or_else(|| tail.find("http://"))?;
    let rest = &tail[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ')']).to_string();

    let user_code = rest
        .find("enter the code ")
        .map(|i| &rest[i + "enter the code ".len()..])
        .and_then(|s| s.split_whitespace().next())
        .map(str::to_string);

    Some(SsoPrompt { url, user_code })
}

fn merged_auth(stdout: &str, stderr: &str) -> Option<SsoPrompt> {
    extract_sso_prompt(stdout).or_else(|| extract_sso_prompt(stderr))
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

fn ensure_script() -> Result<()> {
    if Path::new(INIT_SCRIPT_PATH).exists() {
        Ok(())
    } else {
        Err(anyhow!("NetBird init script not found: {INIT_SCRIPT_PATH}"))
    }
}

/// Whether this firmware ships NetBird support (init script present).
pub fn is_available() -> bool {
    Path::new(INIT_SCRIPT_PATH).exists()
}

/// Invoke `$INIT_SCRIPT_PATH <subcommand> [args...]`.
pub async fn run(subcommand: &str, args: &[&str]) -> Result<CommandOutput> {
    ensure_script()?;

    let output = Command::new(INIT_SCRIPT_PATH)
        .arg(subcommand)
        .args(args)
        .env_remove(SETUP_KEY_ENV)
        .output()
        .await
        .with_context(|| format!("failed to execute {INIT_SCRIPT_PATH} {subcommand}"))?;

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    })
}

async fn run_ok(subcommand: &str, args: &[&str], context: &str) -> Result<CommandOutput> {
    run(subcommand, args).await?.into_result(context)
}

async fn stop_register_child(child: &mut tokio::process::Child) -> i32 {
    if child.id().is_some() {
        let _ = child.start_kill();
    }
    match child.wait().await {
        Ok(status) => status.code().unwrap_or(-1),
        Err(_) => -1,
    }
}

/// Keep `netbird up` running after the SSO URL is returned so the device flow can
/// complete; drain pipes and reap the child. Killed after [`REGISTER_DETACH_TIMEOUT`].
fn detach_register_child(
    mut child: tokio::process::Child,
    mut stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    mut stderr_lines: tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
) {
    tokio::spawn(async move {
        let drain = async {
            tokio::join!(
                async { while stdout_lines.next_line().await.ok().flatten().is_some() {} },
                async { while stderr_lines.next_line().await.ok().flatten().is_some() {} },
            );
        };

        tokio::select! {
            _ = async { tokio::join!(drain, child.wait()) } => {}
            _ = tokio::time::sleep(REGISTER_DETACH_TIMEOUT) => {
                let _ = stop_register_child(&mut child).await;
            }
        }
    });
}

/// Run `register` / `registerForce` and wait up to [`REGISTER_TIMEOUT`].
///
/// - With a setup key, `netbird up` joins non-interactively and exits 0 → `Completed`.
/// - Without one, it prints an SSO URL and blocks until login → `NeedsAuth`; the child
///   is kept running in the background so the login can finish.
async fn run_register(subcommand: &str, setup_key: Option<&str>) -> Result<RegisterResult> {
    ensure_script()?;

    let mut cmd = Command::new(INIT_SCRIPT_PATH);
    cmd.arg(subcommand)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove(SETUP_KEY_ENV);
    if let Some(key) = setup_key {
        cmd.env(SETUP_KEY_ENV, key);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {INIT_SCRIPT_PATH} {subcommand}"))?;

    let mut stdout_lines = BufReader::new(
        child.stdout.take().ok_or_else(|| anyhow!("register: missing stdout pipe"))?,
    )
    .lines();
    let mut stderr_lines = BufReader::new(
        child.stderr.take().ok_or_else(|| anyhow!("register: missing stderr pipe"))?,
    )
    .lines();

    let mut stdout_acc = String::new();
    let mut stderr_acc = String::new();
    let deadline = Instant::now() + REGISTER_TIMEOUT;
    let mut exit_code: Option<i32> = None;
    let mut stdout_open = true;
    let mut stderr_open = true;

    loop {
        // Child exited and both pipes are drained → definitive result.
        if exit_code.is_some() && !stdout_open && !stderr_open {
            break;
        }
        if merged_auth(&stdout_acc, &stderr_acc).is_some() {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        tokio::select! {
            line = stdout_lines.next_line(), if stdout_open => match line {
                Ok(Some(line)) => { stdout_acc.push_str(&line); stdout_acc.push('\n'); }
                Ok(None) => stdout_open = false,
                Err(e) => { stdout_open = false; stderr_acc.push_str(&format!("stdout read error: {e}\n")); }
            },
            line = stderr_lines.next_line(), if stderr_open => match line {
                Ok(Some(line)) => { stderr_acc.push_str(&line); stderr_acc.push('\n'); }
                Ok(None) => stderr_open = false,
                Err(e) => { stderr_open = false; stderr_acc.push_str(&format!("stderr read error: {e}\n")); }
            },
            status = child.wait(), if exit_code.is_none() => {
                exit_code = Some(status?.code().unwrap_or(-1));
            }
            _ = tokio::time::sleep(remaining) => {}
        }
    }

    if let Some(SsoPrompt { url, user_code }) = merged_auth(&stdout_acc, &stderr_acc) {
        let code = match exit_code {
            Some(code) => code,
            None => {
                detach_register_child(child, stdout_lines, stderr_lines);
                -1
            }
        };
        return Ok(RegisterResult {
            status: RegisterStatus::NeedsAuth,
            code,
            stdout: stdout_acc.trim().to_string(),
            stderr: stderr_acc.trim().to_string(),
            auth_url: Some(url),
            user_code,
        });
    }

    if exit_code.is_none() {
        exit_code = Some(stop_register_child(&mut child).await);
        let msg = format!("register: timed out after {}s", REGISTER_TIMEOUT.as_secs());
        if !stderr_acc.trim().is_empty() {
            stderr_acc.push('\n');
        }
        stderr_acc.push_str(&msg);
        return Ok(RegisterResult {
            status: RegisterStatus::Failed,
            code: exit_code.unwrap_or(-1),
            stdout: stdout_acc.trim().to_string(),
            stderr: stderr_acc.trim().to_string(),
            auth_url: None,
            user_code: None,
        });
    }

    RegisterResult::from_output(CommandOutput {
        stdout: stdout_acc,
        stderr: stderr_acc,
        code: exit_code.unwrap_or(-1),
    })
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

pub async fn start() -> Result<()> {
    run_ok("start", &[], "start").await.map(|_| ())
}

pub async fn stop() -> Result<()> {
    run_ok("stop", &[], "stop").await.map(|_| ())
}

pub async fn restart() -> Result<()> {
    run_ok("restart", &[], "restart").await.map(|_| ())
}

pub async fn enable() -> Result<()> {
    run_ok("enable", &[], "enable").await.map(|_| ())
}

pub async fn disable() -> Result<()> {
    run_ok("disable", &[], "disable").await.map(|_| ())
}

pub async fn get_enable() -> Result<EnableState> {
    let out = run_ok("getEnable", &[], "getEnable").await?;
    EnableState::parse_stdout(&out.stdout)
}

/// Persisted management URL (`None` = NetBird Cloud).
pub async fn get_management_url() -> Result<Option<String>> {
    let out = run_ok("getManagementUrl", &[], "getManagementUrl").await?;
    Ok(optional_value(&out.stdout))
}

/// Persist the management URL. Changing it clears the peer state (new identity)
/// and restarts the daemon when enabled. `None` reverts to NetBird Cloud.
pub async fn set_management_url(url: Option<&str>) -> Result<()> {
    match url {
        Some(url) => {
            validate_management_url(url)?;
            run_ok("setManagementUrl", &[url.trim()], "setManagementUrl").await?
        }
        None => run_ok("setManagementUrl", &[], "setManagementUrl").await?,
    };
    Ok(())
}

/// Persisted peer hostname (`None` = NetBird default, the device hostname).
pub async fn get_hostname() -> Result<Option<String>> {
    let out = run_ok("getHostname", &[], "getHostname").await?;
    Ok(optional_value(&out.stdout))
}

/// Persist the peer hostname used on the next `register`.
pub async fn set_hostname(name: Option<&str>) -> Result<()> {
    match name {
        Some(name) => {
            validate_hostname(name)?;
            run_ok("setHostname", &[name], "setHostname").await?
        }
        None => run_ok("setHostname", &[], "setHostname").await?,
    };
    Ok(())
}

/// `register` — `netbird up`. With a setup key the join is non-interactive,
/// otherwise an SSO URL is returned.
pub async fn register(setup_key: Option<&str>) -> Result<RegisterResult> {
    if let Some(key) = setup_key {
        validate_setup_key(key)?;
    }
    run_register("register", setup_key).await
}

/// `registerForce` — `netbird down` + `netbird logout`, then `netbird up`.
pub async fn register_force(setup_key: Option<&str>) -> Result<RegisterResult> {
    if let Some(key) = setup_key {
        validate_setup_key(key)?;
    }
    run_register("registerForce", setup_key).await
}

pub async fn reset_config() -> Result<()> {
    run_ok("resetConfig", &[], "resetConfig").await.map(|_| ())
}

pub async fn reset() -> Result<()> {
    run_ok("reset", &[], "reset").await.map(|_| ())
}

pub async fn get_ip() -> Result<IpListResult> {
    let out = run_ok("getIp", &[], "getIp").await?;
    IpListResult::parse_stdout(&out.stdout)
}

pub async fn get_status() -> Result<(String, StatusResult)> {
    let out = run_ok("getStatus", &[], "getStatus").await?;
    let status = StatusResult::parse_json(&out.stdout)?;
    Ok((out.stdout, status))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS_CONNECTED: &str = r#"{
      "peers": {"total": 3, "connected": 2, "details": [
        {"fqdn": "alexandria.netbird.selfhosted", "netbirdIp": "100.92.0.12", "publicKey": "abc=",
         "status": "Connected", "connectionType": "P2P", "lastStatusUpdate": "2026-10-17T10:00:00Z",
         "lastWireguardHandshake": "2026-10-17T10:00:05Z", "transferReceived": 1024, "transferSent": 2048,
         "latency": 12000000, "quantumResistance": false, "networks": [],
         "iceCandidateType": {"local": "host", "remote": "srflx"}}
      ]},
      "cliVersion": "0.81.2", "daemonVersion": "0.81.2", "daemonStatus": "Connected",
      "management": {"url": "https://netbird.example.org:443", "connected": true, "error": ""},
      "signal": {"url": "https://netbird.example.org:443", "connected": true, "error": ""},
      "relays": {"total": 2, "available": 2, "details": []},
      "netbirdIp": "100.92.1.2/16", "publicKey": "xyz=", "usesKernelInterface": false,
      "wireguardPort": 51820, "fqdn": "arkkvm.netbird.selfhosted", "quantumResistance": false,
      "quantumResistancePermissive": false, "networks": null, "dnsServers": [], "events": [],
      "lazyConnectionEnabled": false, "profileName": "default",
      "sshServer": {"enabled": false, "sessions": []}
    }"#;

    #[test]
    fn parses_connected_status() {
        let s = StatusResult::parse_json(STATUS_CONNECTED).unwrap();
        assert_eq!(s.compute_connection_state(), ConnectionState::Connected);
        assert_eq!(s.ip().as_deref(), Some("100.92.1.2"));
        assert_eq!(s.hostname().as_deref(), Some("arkkvm.netbird.selfhosted"));
        assert_eq!(s.peers.connected, 2);
        assert_eq!(s.peers.details[0].connection_type, "P2P");
        assert!(s.management.connected);
        assert!(s.is_connected());
    }

    /// Shape captured from a real ArkKVM (NetBird 0.81.2, NetBird Cloud, no other peers yet).
    /// Keys / IDs anonymized.
    const STATUS_DEVICE_NO_PEERS: &str = r#"{"peers":{"total":0,"connected":0,"details":null},"cliVersion":"0.81.2","daemonVersion":"0.81.2","daemonStatus":"Connected","management":{"url":"https://api.netbird.io:443","connected":true,"error":""},"signal":{"url":"https://signal.netbird.io:443","connected":true,"error":""},"relays":{"total":4,"available":4,"details":[{"uri":"stun:stun.netbird.io:443","available":true,"error":""},{"uri":"rels://relay.example.netbird.io:443","available":true,"error":"","transport":"ws"}]},"netbirdIp":"100.84.0.10/16","netbirdIpv6":"fdd8::10/64","publicKey":"AAAA=","usesKernelInterface":false,"wireguardPort":51820,"fqdn":"arkkvm.netbird.cloud","quantumResistance":false,"quantumResistancePermissive":false,"networks":null,"dnsServers":[],"events":[{"id":"x","severity":"INFO","category":"SYSTEM","message":"Network map updated","userMessage":"","timestamp":"2026-10-17T14:59:49.493429405Z","metadata":null}],"lazyConnectionEnabled":true,"profileName":"default","sshServer":{"enabled":false,"sessions":[]},"sessionExpiresAt":"2026-10-18T14:59:47.27214Z"}"#;

    #[test]
    fn parses_device_status_with_null_lists() {
        let s = StatusResult::parse_json(STATUS_DEVICE_NO_PEERS).unwrap();
        assert_eq!(s.compute_connection_state(), ConnectionState::Connected);
        assert!(s.peers.details.is_empty());
        assert_eq!(s.peers.total, 0);
        assert_eq!(s.ip().as_deref(), Some("100.84.0.10"));
        assert_eq!(s.hostname().as_deref(), Some("arkkvm.netbird.cloud"));
        assert_eq!(s.relays.available, 4);
        assert_eq!(s.session_expires_at.as_deref(), Some("2026-10-18T14:59:47.27214Z"));

        let s = StatusResult::parse_json(r#"{"daemonStatus":"Idle","peers":null,"management":null}"#).unwrap();
        assert_eq!(s.compute_connection_state(), ConnectionState::Offline);
    }

    #[test]
    fn maps_daemon_states() {
        for (daemon, expected) in [
            ("NeedsLogin", ConnectionState::LoggedOut),
            ("LoginFailed", ConnectionState::LoggedOut),
            ("SessionExpired", ConnectionState::LoggedOut),
            ("Connecting", ConnectionState::Connecting),
            ("Idle", ConnectionState::Offline),
            ("SomethingNew", ConnectionState::Unknown),
        ] {
            let s = StatusResult { daemon_status: daemon.into(), ..Default::default() };
            assert_eq!(s.compute_connection_state(), expected, "{daemon}");
        }
    }

    #[test]
    fn infers_state_without_daemon_status() {
        let mut s = StatusResult { netbird_ip: "100.92.1.2/16".into(), ..Default::default() };
        assert_eq!(s.compute_connection_state(), ConnectionState::Connecting);
        s.management.connected = true;
        assert_eq!(s.compute_connection_state(), ConnectionState::Connected);
        assert_eq!(StatusResult::default().compute_connection_state(), ConnectionState::Unknown);
    }

    #[test]
    fn parses_minimal_status_json() {
        let s = StatusResult::parse_json(r#"{"daemonStatus":"NeedsLogin"}"#).unwrap();
        assert_eq!(s.compute_connection_state(), ConnectionState::LoggedOut);
        assert_eq!(s.ip(), None);
    }

    #[test]
    fn extracts_sso_url_and_code() {
        let out = "Use this URL to log in:\n\nhttps://idp.example.com/activate and enter the code WXYZ-1234 to authenticate.\n";
        let r = RegisterResult::from_output(CommandOutput { stdout: out.into(), stderr: String::new(), code: -1 }).unwrap();
        assert_eq!(r.status, RegisterStatus::NeedsAuth);
        assert_eq!(r.auth_url.as_deref(), Some("https://idp.example.com/activate"));
        assert_eq!(r.user_code.as_deref(), Some("WXYZ-1234"));
    }

    #[test]
    fn extracts_sso_url_with_embedded_code() {
        let out = "Use this URL to log in:\n\nhttps://login.netbird.io/activate?user_code=ABCD-EFGH \n";
        let p = extract_sso_prompt(out).unwrap();
        assert_eq!(p.url, "https://login.netbird.io/activate?user_code=ABCD-EFGH");
        assert_eq!(p.user_code, None);
    }

    #[test]
    fn ignores_unrelated_urls() {
        let err = "Error: login failed: invalid setup key, see https://docs.netbird.io/how-to/register-machines-using-setup-keys";
        assert!(extract_sso_prompt(err).is_none());
        let r = RegisterResult::from_output(CommandOutput { stdout: String::new(), stderr: err.into(), code: 1 }).unwrap();
        assert_eq!(r.status, RegisterStatus::Failed);
    }

    #[test]
    fn register_exit_codes() {
        let ok = RegisterResult::from_output(CommandOutput { stdout: "Connected\n".into(), stderr: String::new(), code: 0 }).unwrap();
        assert!(ok.is_completed());

        let disabled = RegisterResult::from_output(CommandOutput {
            stdout: "NetBird is disabled, skipping registration\n".into(), stderr: String::new(), code: 2,
        }).unwrap();
        assert_eq!(disabled.status, RegisterStatus::Disabled);

        let missing = RegisterResult::from_output(CommandOutput {
            stdout: "NetBird binary not found\n".into(), stderr: String::new(), code: 1,
        });
        assert!(missing.is_err());
    }

    #[test]
    fn command_output_errors() {
        let e = CommandOutput { stdout: String::new(), stderr: "boom".into(), code: 1 }.into_result("getStatus").unwrap_err();
        assert!(e.to_string().contains("exit 1"), "{e}");
        let e = CommandOutput { stdout: "NetBird binary not found".into(), stderr: String::new(), code: 1 }.into_result("x").unwrap_err();
        assert!(e.to_string().contains("binary not found"), "{e}");
    }

    #[test]
    fn parses_ip_list() {
        let ips = IpListResult::parse_stdout("100.92.1.2\nfd00:4e42::2\n").unwrap();
        assert_eq!(ips.ipv4, vec!["100.92.1.2"]);
        assert_eq!(ips.ipv6, vec!["fd00:4e42::2"]);
        assert!(IpListResult::parse_stdout("\n").is_err());
    }

    #[test]
    fn validates_inputs() {
        assert!(validate_management_url("https://netbird.example.org").is_ok());
        assert!(validate_management_url("https://netbird.example.org:33073").is_ok());
        assert!(validate_management_url("netbird.example.org").is_err());
        assert!(validate_management_url("https://").is_err());
        assert!(validate_management_url("https://a b").is_err());

        assert!(validate_hostname("arkkvm-rack1").is_ok());
        assert!(validate_hostname("-evil").is_err());
        assert!(validate_hostname("a;b").is_err());
        assert!(validate_hostname(&"a".repeat(64)).is_err());

        assert!(validate_setup_key("A1B2C3D4-0000-1111-2222-333344445555").is_ok());
        assert!(validate_setup_key("").is_err());
        assert!(validate_setup_key("a b").is_err());
    }
}
