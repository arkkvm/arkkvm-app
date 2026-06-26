//! Bindings for `shell/S40tailscale` — execution in the init script, parsing here.

use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::Instant;

/// Init script path on device.
pub const INIT_SCRIPT_PATH: &str = "/etc/init.d/S40tailscale";

/// `ERROR_CODE_APP_NOT_FOUND` in the shell script.
pub const EXIT_BINARY_NOT_FOUND: i32 = 1;

/// `ERROR_CODE_DISABLE` in the shell script.
pub const EXIT_DISABLED: i32 = 2;

/// Max wait for a browser login URL during `register` / `registerForce`.
const REGISTER_AUTH_URL_TIMEOUT: Duration = Duration::from_secs(30);

/// Max time to keep `tailscale up` running after returning a login URL.
const REGISTER_DETACH_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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
        let stdout = self.stdout.trim();
        let stderr = self.stderr.trim();
        match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => stdout.to_string(),
            (true, false) => stderr.to_string(),
            (false, false) => format!("{stdout}\n{stderr}"),
        }
    }

    pub fn stdout_lines(&self) -> Vec<String> {
        self.stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Map shell exit codes to `Result` (see `S40tailscale`).
    pub fn into_result(self, context: &str) -> Result<Self> {
        if self.success() {
            return Ok(self);
        }
        let log = self.combined_log();
        Err(match self.code {
            EXIT_BINARY_NOT_FOUND => anyhow!("{context}: tailscale binary not found"),
            EXIT_DISABLED => anyhow!("{context}: tailscale is disabled"),
            _ => anyhow!("{context} failed (exit {}): {log}", self.code),
        })
    }
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

/// `getLoginServer` — persisted control plane URL (`getLoginServer` subcommand).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoginServerResult {
    /// `None` = official Tailscale SaaS (no persisted file).
    pub url: Option<String>,
}

impl LoginServerResult {
    pub fn parse_stdout(stdout: &str) -> Self {
        let url = stdout.trim();
        if url.is_empty() {
            Self { url: None }
        } else {
            Self { url: Some(url.to_string()) }
        }
    }
}

/// `setLoginServer` — persist URL, clear state, restart tailscaled if enabled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SetLoginServerResult {
    Cleared { restarted: bool },
    Set { url: String, restarted: bool },
}

impl SetLoginServerResult {
    pub fn parse_stdout(stdout: &str) -> Result<Self> {
        let lines = stdout_lines(stdout);
        let restarted = lines.iter().any(|l| l == "Tailscale is enabled, restarting Tailscale");

        for line in &lines {
            if line == "Login server reset" {
                return Ok(Self::Cleared { restarted });
            }
            if let Some(url) = line.strip_prefix("Login server set to ") {
                return Ok(Self::Set { url: url.to_string(), restarted });
            }
        }
        Err(anyhow!("unexpected setLoginServer output: {stdout:?}"))
    }
}

/// Outcome of `register` / `registerForce` (`tailscale up` only).
///
/// `tailscale up` may print a login URL and exit non-zero before auth completes;
/// that is [`RegisterStatus::NeedsAuth`], not a hard failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RegisterStatus {
    /// Device joined / reconnected (script exit 0, no auth URL in output).
    Completed,
    /// Login URL was printed; user must finish auth in the browser (exit code may be non-zero).
    NeedsAuth,
    /// `S40tailscale` reported Tailscale disabled (exit 2).
    Disabled,
    /// Exited with error and no auth URL was found.
    Failed,
}

/// `register` / `registerForce` (`tailscale up` stdout/stderr).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterResult {
    pub status: RegisterStatus,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    /// Browser auth URL from `tailscale up` (when [`RegisterStatus::NeedsAuth`]).
    pub auth_url: Option<String>,
}

impl RegisterResult {
    pub fn from_output(out: CommandOutput) -> Result<Self> {
        if out.code == EXIT_BINARY_NOT_FOUND {
            return Err(anyhow!("register: tailscale binary not found"));
        }

        let auth_url =
            extract_auth_url(&out.stdout).or_else(|| extract_auth_url(&out.stderr));
        let stdout = out.stdout.trim().to_string();
        let stderr = out.stderr.trim().to_string();

        let status = if out.code == EXIT_DISABLED {
            RegisterStatus::Disabled
        } else if auth_url.is_some() {
            // URL can appear before `tailscale up` succeeds; do not require exit 0.
            RegisterStatus::NeedsAuth
        } else if out.success() {
            RegisterStatus::Completed
        } else {
            RegisterStatus::Failed
        };

        Ok(Self { status, code: out.code, stdout, stderr, auth_url })
    }

    pub fn combined_log(&self) -> String {
        match (self.stdout.is_empty(), self.stderr.is_empty()) {
            (true, true) => String::new(),
            (false, true) => self.stdout.clone(),
            (true, false) => self.stderr.clone(),
            (false, false) => format!("{}\n{}", self.stdout, self.stderr),
        }
    }

    pub fn needs_auth(&self) -> bool {
        self.status == RegisterStatus::NeedsAuth
    }

    pub fn is_completed(&self) -> bool {
        self.status == RegisterStatus::Completed
    }
}

/// `getIp` (`tailscale ip`) — Tailscale assigns both v4 and v6 when available.
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
                match token.parse::<IpAddr>() {
                    Ok(IpAddr::V4(addr)) => ipv4.push(addr.to_string()),
                    Ok(IpAddr::V6(addr)) => ipv6.push(addr.to_string()),
                    Err(_) => {}
                }
            }
        }

        if ipv4.is_empty() && ipv6.is_empty() {
            return Err(anyhow!("unexpected getIp output: {stdout:?}"));
        }

        Ok(Self { ipv4, ipv6 })
    }

    /// All addresses (v4 first, then v6).
    pub fn all(&self) -> Vec<String> {
        self.ipv4.iter().chain(self.ipv6.iter()).cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.ipv4.is_empty() && self.ipv6.is_empty()
    }
}

/// Local tailnet connection state derived from `tailscale status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionState {
    Connected,
    LoggedOut,
    Offline,
    #[default]
    Unknown,
}

/// `getStatus` (`tailscale status --json`) — 1:1 mirror of Tailscale CLI JSON (PascalCase).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StatusResult {
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "TUN")]
    pub tun: bool,
    #[serde(rename = "BackendState")]
    pub backend_state: String,
    #[serde(rename = "HaveNodeKey")]
    pub have_node_key: bool,
    #[serde(rename = "AuthURL")]
    pub auth_url: String,
    #[serde(rename = "TailscaleIPs")]
    pub tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "Self")]
    pub self_node: Option<StatusPeer>,
    #[serde(rename = "Health")]
    pub health: Vec<String>,
    #[serde(rename = "MagicDNSSuffix")]
    pub magic_dns_suffix: String,
    #[serde(rename = "CurrentTailnet")]
    pub current_tailnet: Option<StatusTailnet>,
    #[serde(rename = "CertDomains")]
    pub cert_domains: Option<Vec<String>>,
    #[serde(rename = "Peer")]
    pub peer: Option<std::collections::HashMap<String, StatusPeer>>,
    #[serde(rename = "User")]
    pub user: Option<std::collections::HashMap<String, StatusUser>>,
    #[serde(rename = "ClientVersion")]
    pub client_version: Option<serde_json::Value>,
}

impl Default for StatusResult {
    fn default() -> Self {
        Self {
            version: String::new(),
            tun: false,
            backend_state: String::new(),
            have_node_key: false,
            auth_url: String::new(),
            tailscale_ips: None,
            self_node: None,
            health: Vec::new(),
            magic_dns_suffix: String::new(),
            current_tailnet: None,
            cert_domains: None,
            peer: None,
            user: None,
            client_version: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StatusTailnet {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "MagicDNSSuffix")]
    pub magic_dns_suffix: String,
    #[serde(rename = "MagicDNSEnabled")]
    pub magic_dns_enabled: bool,
}

impl Default for StatusTailnet {
    fn default() -> Self {
        Self {
            name: String::new(),
            magic_dns_suffix: String::new(),
            magic_dns_enabled: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StatusUser {
    #[serde(rename = "ID")]
    pub id: u64,
    #[serde(rename = "LoginName")]
    pub login_name: String,
    #[serde(rename = "DisplayName")]
    pub display_name: String,
}

impl Default for StatusUser {
    fn default() -> Self {
        Self {
            id: 0,
            login_name: String::new(),
            display_name: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StatusPeer {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "PublicKey")]
    pub public_key: String,
    #[serde(rename = "HostName")]
    pub hostname: String,
    #[serde(rename = "DNSName")]
    pub dns_name: String,
    #[serde(rename = "OS")]
    pub os: String,
    #[serde(rename = "UserID")]
    pub user_id: u64,
    #[serde(rename = "TailscaleIPs")]
    pub tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "AllowedIPs")]
    pub allowed_ips: Option<Vec<String>>,
    #[serde(rename = "Addrs")]
    pub addrs: Option<Vec<String>>,
    #[serde(rename = "CurAddr")]
    pub cur_addr: String,
    #[serde(rename = "Relay")]
    pub relay: String,
    #[serde(rename = "PeerRelay")]
    pub peer_relay: String,
    #[serde(rename = "RxBytes")]
    pub rx_bytes: u64,
    #[serde(rename = "TxBytes")]
    pub tx_bytes: u64,
    #[serde(rename = "Created")]
    pub created: String,
    #[serde(rename = "LastWrite")]
    pub last_write: String,
    #[serde(rename = "LastSeen")]
    pub last_seen: String,
    #[serde(rename = "LastHandshake")]
    pub last_handshake: String,
    #[serde(rename = "Online")]
    pub online: bool,
    #[serde(rename = "ExitNode")]
    pub exit_node: bool,
    #[serde(rename = "ExitNodeOption")]
    pub exit_node_option: bool,
    #[serde(rename = "Active")]
    pub active: bool,
    #[serde(rename = "PeerAPIURL")]
    pub peer_api_url: Option<Vec<String>>,
    #[serde(rename = "TaildropTarget")]
    pub taildrop_target: i64,
    #[serde(rename = "NoFileSharingReason")]
    pub no_file_sharing_reason: String,
    #[serde(rename = "Capabilities", default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(rename = "CapMap", default)]
    pub cap_map: Option<std::collections::HashMap<String, serde_json::Value>>,
    #[serde(rename = "InNetworkMap")]
    pub in_network_map: bool,
    #[serde(rename = "InMagicSock")]
    pub in_magic_sock: bool,
    #[serde(rename = "InEngine")]
    pub in_engine: bool,
    #[serde(rename = "KeyExpiry")]
    pub key_expiry: String,
}

impl Default for StatusPeer {
    fn default() -> Self {
        Self {
            id: String::new(),
            public_key: String::new(),
            hostname: String::new(),
            dns_name: String::new(),
            os: String::new(),
            user_id: 0,
            tailscale_ips: None,
            allowed_ips: None,
            addrs: None,
            cur_addr: String::new(),
            relay: String::new(),
            peer_relay: String::new(),
            rx_bytes: 0,
            tx_bytes: 0,
            created: String::new(),
            last_write: String::new(),
            last_seen: String::new(),
            last_handshake: String::new(),
            online: false,
            exit_node: false,
            exit_node_option: false,
            active: false,
            peer_api_url: None,
            taildrop_target: 0,
            no_file_sharing_reason: String::new(),
            capabilities: None,
            cap_map: None,
            in_network_map: false,
            in_magic_sock: false,
            in_engine: false,
            key_expiry: String::new(),
        }
    }
}

impl StatusResult {
    /// Parse `tailscale status --json` stdout.
    pub fn parse_stdout(stdout: &str) -> Result<Self> {
        Self::parse_json(stdout)
    }

    pub fn parse_json(json: &str) -> Result<Self> {
        serde_json::from_str(json.trim()).context("parse tailscale status --json")
    }

    /// Assigned Tailscale IPs (top-level or on `Self`).
    fn has_tailscale_ips(&self) -> bool {
        self.tailscale_ips
            .as_ref()
            .is_some_and(|ips| !ips.is_empty())
            || self
                .self_node
                .as_ref()
                .and_then(|node| node.tailscale_ips.as_ref())
                .is_some_and(|ips| !ips.is_empty())
    }

    /// `Self` node is on the tailnet (complements top-level fields).
    ///
    /// Per ipnstate, `InNetworkMap` / `InMagicSock` / `InEngine` should all be true
    /// when wired; `DNSName` is assigned from the netmap. `Online` is control-plane
    /// reachability only and is intentionally excluded here.
    fn self_on_tailnet(node: &StatusPeer) -> bool {
        if node.tailscale_ips.as_ref().is_some_and(|ips| !ips.is_empty()) {
            return true;
        }
        if !node.in_network_map {
            return false;
        }
        node.in_engine || node.in_magic_sock || !node.dns_name.is_empty()
    }

    /// Joined a tailnet per `tailscale status --json` / ipnstate semantics.
    fn is_on_tailnet(&self) -> bool {
        self.has_tailscale_ips()
            || self.current_tailnet.is_some()
            || self
                .self_node
                .as_ref()
                .is_some_and(Self::self_on_tailnet)
    }

    fn health_lower(&self) -> String {
        self.health.join("\n").to_ascii_lowercase()
    }

    /// `Health` indicates auth/session is invalid — user must re-register.
    ///
    /// Surfaces e.g. admin-console device removal (`node not found`), expired keys,
    /// and explicit logout (`not logged in`, `Tailscale is stopped.`).
    fn health_needs_reauth(&self) -> bool {
        const NEEDLES: &[&str] = &[
            "not logged in",
            "logged out",
            "tailscale is stopped",
            "state=needslogin",
            "needslogin",
            "needs login",
            "needs machine auth",
            "not yet approved",
            "node not found",
            "node key not found",
            "invalid key",
            "api key does not exist",
            "api key not valid",
        ];
        let health = self.health_lower();
        NEEDLES.iter().any(|needle| health.contains(needle))
    }

    pub fn compute_connection_state(&self) -> ConnectionState {
        if self.health_needs_reauth() {
            return ConnectionState::LoggedOut;
        }

        match self.backend_state.as_str() {
            "NeedsLogin" | "NeedsMachineAuth" | "Stopped" => ConnectionState::LoggedOut,
            "Running" => {
                if self.is_on_tailnet() {
                    ConnectionState::Connected
                } else {
                    // Not on tailnet yet (including control-plane-only / netmap pending).
                    ConnectionState::Offline
                }
            }
            "Starting" | "NoState" => ConnectionState::Unknown,
            _ => {
                let health = self.health_lower();
                if health.contains("logged out") || health.contains("stopped") {
                    ConnectionState::LoggedOut
                } else {
                    ConnectionState::Unknown
                }
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        self.compute_connection_state() == ConnectionState::Connected
    }

    pub fn is_logged_out(&self) -> bool {
        self.compute_connection_state() == ConnectionState::LoggedOut
    }
}

/// `help`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpResult {
    pub text: String,
}

impl HelpResult {
    pub fn parse_stdout(stdout: &str) -> Self {
        Self { text: stdout.to_string() }
    }
}

/// `setup_first_time` (= enable → setLoginServer → register)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupFirstTimeResult {
    pub set_login_server: SetLoginServerResult,
    pub register: RegisterResult,
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Invoke `$INIT_SCRIPT_PATH <subcommand> [args...]`.
pub async fn run(subcommand: &str, args: &[&str]) -> Result<CommandOutput> {
    if !Path::new(INIT_SCRIPT_PATH).exists() {
        return Err(anyhow!("Tailscale init script not found: {INIT_SCRIPT_PATH}"));
    }

    let output = Command::new(INIT_SCRIPT_PATH)
        .arg(subcommand)
        .args(args)
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

fn stdout_lines(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Extract login URL from `tailscale up` output (`https://` or `http://`).
fn extract_auth_url(text: &str) -> Option<String> {
    let start = text.find("https://").or_else(|| text.find("http://"))?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ')' || c == '"' || c == '\'')
        .unwrap_or(rest.len());
    Some(rest[..end].trim_end_matches(&['.', ','][..]).to_string())
}

fn merged_auth_url(stdout: &str, stderr: &str) -> Option<String> {
    extract_auth_url(stdout).or_else(|| extract_auth_url(stderr))
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

/// Keep `tailscale up` running after the login URL is returned; drain pipes and reap the child.
///
/// Kills the process if it is still running after [`REGISTER_DETACH_TIMEOUT`] (5 minutes).
fn detach_register_child(
    child: tokio::process::Child,
    mut stdout_lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    mut stderr_lines: tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
) {
    tokio::spawn(async move {
        let mut child = Some(child);
        let drain = async {
            tokio::join!(
                async {
                    while stdout_lines.next_line().await.ok().flatten().is_some() {}
                },
                async {
                    while stderr_lines.next_line().await.ok().flatten().is_some() {}
                },
            );
        };

        tokio::select! {
            status = async {
                match child.as_mut() {
                    Some(c) => c.wait().await,
                    None => Ok(std::process::ExitStatus::default()),
                }
            } => {
                let _ = status;
            }
            _ = drain => {
                if let Some(mut c) = child.take() {
                    let _ = c.wait().await;
                }
            }
            _ = tokio::time::sleep(REGISTER_DETACH_TIMEOUT) => {
                if let Some(mut c) = child.take() {
                    let _ = stop_register_child(&mut c).await;
                }
            }
        }
    });
}

/// Run `register` / `registerForce` and wait up to [`REGISTER_AUTH_URL_TIMEOUT`] for a login URL.
///
/// Returns as soon as:
/// - a login URL is parsed from `tailscale up` stdout/stderr, or
/// - the child exits with a definitive outcome (`Completed` / `Disabled` without needing a URL).
///
/// When a login URL is found, the child is **not** killed so the user can finish browser auth;
/// stdout/stderr are drained in a background task until `tailscale up` exits.
///
/// If no URL appears before the deadline, the child is killed and [`RegisterStatus::Failed`] is returned.
async fn run_register(subcommand: &str) -> Result<RegisterResult> {
    if !Path::new(INIT_SCRIPT_PATH).exists() {
        return Err(anyhow!("Tailscale init script not found: {INIT_SCRIPT_PATH}"));
    }

    let mut child = Command::new(INIT_SCRIPT_PATH)
        .arg(subcommand)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {INIT_SCRIPT_PATH} {subcommand}"))?;

    let stdout = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("register: missing stdout pipe"))?,
    );
    let stderr = BufReader::new(
        child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("register: missing stderr pipe"))?,
    );
    let mut stdout_lines = stdout.lines();
    let mut stderr_lines = stderr.lines();

    let mut stdout_acc = String::new();
    let mut stderr_acc = String::new();
    let deadline = Instant::now() + REGISTER_AUTH_URL_TIMEOUT;
    let mut exit_code: Option<i32> = None;
    let mut child_done = false;
    let mut auth_url = None;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());

        tokio::select! {
            line = stdout_lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        stdout_acc.push_str(&line);
                        stdout_acc.push('\n');
                    }
                    Ok(None) => {}
                    Err(e) => stderr_acc.push_str(&format!("stdout read error: {e}\n")),
                }
            }
            line = stderr_lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        stderr_acc.push_str(&line);
                        stderr_acc.push('\n');
                    }
                    Ok(None) => {}
                    Err(e) => stderr_acc.push_str(&format!("stderr read error: {e}\n")),
                }
            }
            status = child.wait(), if !child_done => {
                exit_code = Some(status?.code().unwrap_or(-1));
                child_done = true;
            }
            _ = tokio::time::sleep(remaining) => {}
        }

        auth_url = merged_auth_url(&stdout_acc, &stderr_acc);
        if auth_url.is_some() {
            break;
        }

        if let Some(code) = exit_code {
            if code == EXIT_BINARY_NOT_FOUND {
                return Err(anyhow!("register: tailscale binary not found"));
            }
            if code == EXIT_DISABLED || code == 0 {
                break;
            }
        }
    }

    auth_url = auth_url.or_else(|| merged_auth_url(&stdout_acc, &stderr_acc));

    if let Some(url) = auth_url {
        let code = if child_done {
            exit_code.unwrap_or(-1)
        } else {
            detach_register_child(child, stdout_lines, stderr_lines);
            -1
        };
        return Ok(RegisterResult {
            status: RegisterStatus::NeedsAuth,
            code,
            stdout: stdout_acc.trim().to_string(),
            stderr: stderr_acc.trim().to_string(),
            auth_url: Some(url),
        });
    }

    if !child_done {
        exit_code = Some(stop_register_child(&mut child).await);
        child_done = true;
    }

    if Instant::now() >= deadline
        && exit_code != Some(0)
        && exit_code != Some(EXIT_DISABLED)
    {
        let timeout_msg = format!(
            "register: timed out after {}s waiting for login URL",
            REGISTER_AUTH_URL_TIMEOUT.as_secs()
        );
        if stderr_acc.trim().is_empty() {
            stderr_acc = timeout_msg;
        } else if !stderr_acc.contains("timed out after") {
            stderr_acc.push('\n');
            stderr_acc.push_str(&timeout_msg);
        }
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

/// `start` — success if exit 0 (`EXIT_BINARY_NOT_FOUND` / other non-zero → `Err`).
pub async fn start() -> Result<()> {
    run_ok("start", &[], "start").await?;
    Ok(())
}

pub async fn stop() -> Result<()> {
    run_ok("stop", &[], "stop").await?;
    Ok(())
}

pub async fn restart() -> Result<()> {
    run_ok("restart", &[], "restart").await?;
    Ok(())
}

pub async fn enable() -> Result<()> {
    run_ok("enable", &[], "enable").await?;
    Ok(())
}

pub async fn disable() -> Result<()> {
    run_ok("disable", &[], "disable").await?;
    Ok(())
}

pub async fn get_enable() -> Result<EnableState> {
    let out = run_ok("getEnable", &[], "getEnable").await?;
    EnableState::parse_stdout(&out.stdout)
}

/// `getLoginServer` — read persisted control plane URL (`None` = Tailscale SaaS default).
pub async fn get_login_server() -> Result<LoginServerResult> {
    let out = run_ok("getLoginServer", &[], "getLoginServer").await?;
    Ok(LoginServerResult::parse_stdout(&out.stdout))
}

/// `setLoginServer` — persist control plane URL and restart `tailscaled` if enabled.
///
/// Control plane is applied via `tailscaled --login-server=...` on start, not via `tailscale up`.
/// `None` clears the file (official SaaS on next start).
pub async fn set_login_server(url: Option<String>) -> Result<SetLoginServerResult> {
    let out = match url {
        Some(url) => run_ok("setLoginServer", &[url.as_str()], "setLoginServer").await?,
        None => run_ok("setLoginServer", &[], "setLoginServer").await?,
    };
    SetLoginServerResult::parse_stdout(&out.stdout)
}

/// `register` — `tailscale up` only (daemon must already use the desired login server).
///
/// Call [`set_login_server`] first when using Headscale / a custom control plane.
pub async fn register() -> Result<RegisterResult> {
    run_register("register").await
}

/// `registerForce` — `tailscale up --force-reauth`.
pub async fn register_force() -> Result<RegisterResult> {
    run_register("registerForce").await
}

pub async fn reset_config() -> Result<()> {
    run_ok("resetConfig", &[], "resetConfig").await?;
    Ok(())
}

pub async fn reset() -> Result<()> {
    run_ok("reset", &[], "reset").await?;
    Ok(())
}

pub async fn get_status() -> Result<(String, StatusResult)> {
    let out = run_ok("getStatus", &[], "getStatus").await?;
    let status = StatusResult::parse_json(&out.stdout)?;
    Ok((out.stdout, status))
}