//! Bridge to network-config-unified (project C) shell tools.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tracing::{info, warn};

pub const NWU_ROOT: &str = "/oem/usr/bin/arkkvm-network";
const CTL: &str = "/oem/usr/bin/arkkvm-network/ctl";
const REFRESH_SCRIPT: &str = "/oem/usr/bin/arkkvm-network/scripts/refresh-network.sh";

fn ctl_path() -> &'static str {
    if Path::new(CTL).exists() {
        CTL
    } else {
        // Dev / test tree relative to workspace
        "/root/arkkvm-devel/arkkvm-network/ctl"
    }
}

fn refresh_script_path() -> &'static str {
    if Path::new(REFRESH_SCRIPT).exists() {
        REFRESH_SCRIPT
    } else {
        "/root/arkkvm-devel/arkkvm-network/scripts/refresh-network.sh"
    }
}

async fn run_command(program: &str, args: &[&str]) -> Result<()> {
    info!("Running: {} {}", program, args.join(" "));
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = crate::process_spawn::output(cmd)
        .await
        .with_context(|| format!("Failed to execute {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "{program} {} failed (exit {}): {stderr}{}",
            args.join(" "),
            output.status,
            if stdout.is_empty() {
                String::new()
            } else {
                format!(" stdout: {stdout}")
            }
        );
    }
    Ok(())
}

/// Apply committed `network.conf` (default path).
pub async fn refresh_committed(conf_path: Option<&str>) -> Result<()> {
    let script = refresh_script_path();
    let path = conf_path.unwrap_or(crate::network::network_conf::NETWORK_CONF);
    let start = Instant::now();
    let result = run_command(script, &[path]).await;
    info!(
        path,
        elapsed_ms = start.elapsed().as_millis() as u64,
        ok = result.is_ok(),
        "nwu refresh_committed"
    );
    result
}

/// Apply hostname/domain the same way as NWU `nwu_hostname_set`, without a full
/// network refresh (so VLAN/DHCP state is left untouched).
pub async fn apply_hostname(name: &str, domain: &str) -> Result<()> {
    let name = if name.trim().is_empty() {
        "arkkvm"
    } else {
        name.trim()
    };
    let domain = if domain.trim().is_empty() {
        "lan"
    } else {
        domain.trim()
    };

    tokio::fs::write("/etc/hostname", format!("{name}\n"))
        .await
        .with_context(|| "Failed to write /etc/hostname")?;

    match crate::process_spawn::output({
        let mut cmd = Command::new("hostname");
        cmd.arg(name);
        cmd
    })
    .await
    {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            warn!(
                "hostname(1) exit {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Err(e) => warn!("Failed to run hostname(1): {e}"),
    }

    // Mirror lib/hostname.sh: refresh the 127.0.1.1 hosts line.
    let hosts_path = "/etc/hosts";
    let existing = tokio::fs::read_to_string(hosts_path).await.unwrap_or_default();
    let mut kept = String::new();
    for line in existing.lines() {
        if line.split_whitespace().next() == Some("127.0.1.1") {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept.push_str(&format!("127.0.1.1\t{name}.{domain}\t{name}\n"));
    tokio::fs::write(hosts_path, kept)
        .await
        .with_context(|| "Failed to update /etc/hosts")?;

    info!(name, domain, "nwu apply_hostname (no full refresh)");
    Ok(())
}

/// Apply VLAN staging from pending file without overwriting committed config.
pub async fn refresh_staging(pending_path: &str) -> Result<()> {
    let script = refresh_script_path();
    run_command(script, &["--staging", pending_path]).await
}

/// Renew DHCP lease on interface (e.g. eth0 or eth0.10).
pub async fn dhcp_renew(iface: Option<&str>) -> Result<()> {
    let ctl = ctl_path();
    match iface {
        Some(iface) => run_command(ctl, &["dhcp", "renew", iface]).await,
        None => run_command(ctl, &["dhcp", "renew"]).await,
    }
}

/// Remove pending VLAN config files via C tooling.
pub async fn vlan_cleanup_pending() -> Result<()> {
    let ctl = ctl_path();
    if Path::new(ctl).exists() {
        if let Err(e) = run_command(ctl, &["vlan", "cleanup-pending"]).await {
            warn!("vlan cleanup-pending: {e}");
        }
    }
    Ok(())
}
