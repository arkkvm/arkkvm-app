use anyhow::{Context, Result, bail};
use regex::Regex;
use rustix::fs::{Mode, OFlags, open};
use rustix::io::{read};

const CPUINFO_FILE: &str = "/proc/cpuinfo";

pub fn extract_serial_number() -> Result<String> {
    let file = open(CPUINFO_FILE, OFlags::RDONLY, Mode::empty())
        .context("Failed to open /proc/cpuinfo")?;

    let mut content = Vec::new();
    let mut buffer = [0u8; 4096];

    loop {
        match read(&file, &mut buffer) {
            Ok(0) => break,
            Ok(n) => content.extend_from_slice(&buffer[..n]),
            Err(e) => return Err(e).context(format!("Failed to read {}", CPUINFO_FILE)),
        }
    }

    let content = String::from_utf8(content).context(format!("Failed to parse {} as UTF-8", CPUINFO_FILE))?;

    let r = Regex::new(r"Serial\s*:\s*(\S+)").context("Failed to compile regex")?;

    if let Some(matches) = r.captures(&content)
        && matches.len() >= 2
    {
        return Ok(matches[1].to_string());
    }

    bail!("No serial number found in {}", CPUINFO_FILE)
}