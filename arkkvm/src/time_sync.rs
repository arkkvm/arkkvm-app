use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, Context, anyhow};
use chrono::{DateTime, Utc};
use libc::{CLOCK_REALTIME, clock_settime, timespec};
use reqwest::header;
use tracing::{info, warn, error};

use crate::config::ConfigManager;

const DEFAULT_NTP_SERVERS: &[&str] = &[
    // Global
    "time.google.com:123",        // Google
    "time.cloudflare.com:123",    // Cloudflare
    "time.windows.com:123",       // Microsoft
    "pool.ntp.org:123",           // NTP Pool
    "time.nist.gov:123",          // NIST
    // China
    "ntp.ntsc.ac.cn:123",
    "ntp.aliyun.com:123",
    "time.tencentcloud.com:123",
    "time.hicloud.com:123",
];

const DEFAULT_HTTP_TIME_URLS: &[&str] = &[
    // Global
    "http://www.gstatic.com/generate_204",
    "http://www.google.com/",
    "http://cp.cloudflare.com/",
    "http://www.cloudflare.com/",
    "http://edge-http.microsoft.com/captiveportal/generate_204",
    "http://www.microsoft.com/",
    "http://www.apple.com/",
    "http://www.github.com/",
    // China
    "http://www.baidu.com/",
    "http://www.aliyun.com/",
    "http://cloud.tencent.com/",
    "http://www.huaweicloud.com/",
];

fn system_time_to_timespec(st: SystemTime) -> Result<timespec> {
    let dur = st.duration_since(UNIX_EPOCH).context("system time before UNIX_EPOCH")?;
    Ok(timespec {
        tv_sec: dur.as_secs() as libc::time_t,
        tv_nsec: dur.subsec_nanos() as libc::c_long,
    })
}

fn set_system_time(dt: DateTime<Utc>) -> Result<()> {
    let ts = system_time_to_timespec(SystemTime::from(dt))?;
    let rc = unsafe { clock_settime(CLOCK_REALTIME, &ts as *const timespec) };
    if rc != 0 {
        return Err(anyhow!("clock_settime failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

async fn http_date_with_rtt(
    url: &str,
    client: &reqwest::Client,
    method: &str,
) -> Result<DateTime<Utc>> {
    let t0 = SystemTime::now();
    let resp = match method {
        "HEAD" => client.head(url).send().await?,
        _ => client.get(url).send().await?,
    };
    let t1 = SystemTime::now();

    let date_hdr = resp
        .headers()
        .get(header::DATE)
        .ok_or_else(|| anyhow!("missing Date header"))?
        .to_str()
        .context("invalid Date header bytes")?;

    let srv_time = httpdate::parse_http_date(date_hdr)
        .with_context(|| format!("parse_http_date failed: {}", date_hdr))?;

    let rtt = t1.duration_since(t0).unwrap_or(Duration::ZERO);
    let corrected = srv_time + rtt / 2;

    Ok(DateTime::<Utc>::from(corrected))
}

async fn sync_time_once() -> Result<()> {
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .https_only(false)
        .timeout(Duration::from_secs(3))
        .tcp_keepalive(Duration::from_secs(30))
        .user_agent("arkkvm/timesync")
        .build()?;

    for url in DEFAULT_HTTP_TIME_URLS.iter() {
        // Try HEAD first
        match http_date_with_rtt(url, &client, "HEAD").await {
            Ok(dt) => {
                set_system_time(dt)?;
                info!("Time synced via HTTP Date(+1/2RTT): {} (url: {})", dt, url);
                return Ok(());
            }
            Err(e1) => {
                // Fallback to GET
                match http_date_with_rtt(url, &client, "GET").await {
                    Ok(dt) => {
                        set_system_time(dt)?;
                        info!("Time synced via HTTP Date(+1/2RTT): {} (url: {})", dt, url);
                        return Ok(());
                    }
                    Err(e2) => {
                        warn!("HTTP time source failed: {} (HEAD: {}; GET: {})", url, e1, e2);
                    }
                }
            }
        }
    }

    Err(anyhow!("all HTTP time sources failed"))
}

async fn sync_time_by_ntp() -> Result<()> {
    for server in DEFAULT_NTP_SERVERS.iter() {
        match ntp_request(server).await {
            Ok(dt) => {
                set_system_time(dt)?;
                info!("Time synced via NTP: {} (server: {})", dt, server);
                return Ok(());
            }
            Err(e) => {
                warn!("NTP server failed: {} (error: {})", server, e);
            }
        }
    }
    Err(anyhow!("all NTP servers failed"))
}

async fn ntp_request(server: &str) -> Result<DateTime<Utc>> {
    let server = server.to_string();
    let result = tokio::task::spawn_blocking(move || -> Result<DateTime<Utc>> {
        let response: ntp::packet::Packet = match ntp::request(&server) {
            Ok(response) => response,
            Err(e) => return Err(anyhow!("failed to request NTP time from {}: {:?}", server, e)),
        };

        let ntp_time = response.transmit_time;
        // Convert NTP timestamp to Unix time components manually
        let ntp_seconds = ntp_time.sec as i64;
        let ntp_fraction = ntp_time.frac as f64;
        // Calculate offset between NTP epoch (1900) and Unix epoch (1970)
        const NTP_TO_UNIX_OFFSET: i64 = 2_208_988_800; // 70 years including leap years
        let sec = ntp_seconds - NTP_TO_UNIX_OFFSET;
        let nsec = (ntp_fraction * 1_000_000_000.0 / (u32::MAX as f64 + 1.0)) as u32;

        if sec < 0 {
            return Err(anyhow!("invalid NTP time: negative timestamp from {}", server));
        }
        
        let unix_time = UNIX_EPOCH + Duration::new(sec as u64, nsec as u32);
        let time = DateTime::<Utc>::from(unix_time);
        info!("NTP response time: {} (server: {})", time, server);
        Ok(time)
    })
    .await??;
    Ok(result)
}

pub async fn sync_time(config: &ConfigManager) {
    let network_config = config.get_network_config().await;
    let (http_sync, ntp_sync) = match network_config.time_sync_mode.as_str() {
        "http_only" => (true, false),
        "ntp_only" => (false, true),
        "ntp_and_http" => (true, true),
        // "custom" => (true, true),
        _ => (false, false),
    };

    let mut succeed = false;
    if http_sync {
        if let Err(e) = sync_time_once().await {
            error!("Time sync failed (non-fatal if clock already correct): {}", e);
        }
        else {
            succeed = true;
        }
    }

    if ntp_sync {
        if succeed {
            tokio::spawn(async {
                if let Err(e) = sync_time_by_ntp().await {
                    error!("Time sync failed (non-fatal if clock already correct): {}", e);
                }
            });
        }
        else {
            if let Err(e) = sync_time_by_ntp().await {
                error!("Time sync failed (non-fatal if clock already correct): {}", e);
            }
        }
    }

    if !http_sync && !ntp_sync {
        warn!("Time sync mode is turn off, skipping time sync");
    }
}