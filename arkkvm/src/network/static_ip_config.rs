use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const CONFIG_DIR: &str = "/userdata/arkkvm/"; // config file storage directory
const CONFIG_FILENAME: &str = "static_ip.conf"; // config filename

#[derive(Debug, Clone)]
pub struct StaticIpConfigInfo {
    pub ip_address: Option<String>,
    pub netmask: Option<String>,
    pub gateway: Option<String>,
    pub dns_servers: Option<Vec<String>>,
    pub custom_dir: Option<String>,
}

impl StaticIpConfigInfo {
    pub fn new() -> Self {
        Self { ip_address: None, netmask: None, gateway: None, dns_servers: None, custom_dir: None }
    }

    pub fn with_ip(&mut self, ip: &str) -> &mut Self {
        self.ip_address = Some(ip.to_string());
        self
    }

    pub fn with_netmask(&mut self, netmask: &str) -> &mut Self {
        self.netmask = Some(netmask.to_string());
        self
    }

    pub fn with_gateway(&mut self, gateway: &str) -> &mut Self {
        self.gateway = Some(gateway.to_string());
        self
    }

    pub fn with_dns(&mut self, dns_servers: Vec<&str>) -> &mut Self {
        self.dns_servers = Some(dns_servers.iter().map(|&s| s.to_string()).collect());
        self
    }

    pub fn with_custom_dir(&mut self, dir: &str) -> &mut Self {
        self.custom_dir = Some(dir.to_string());
        self
    }

    // preserve chained builder at construction time
    pub fn with_all(ip: &str, netmask: &str, gateway: &str) -> Self {
        Self {
            ip_address: Some(ip.to_string()),
            netmask: Some(netmask.to_string()),
            gateway: Some(gateway.to_string()),
            dns_servers: None,
            custom_dir: None,
        }
    }

    // resolve the actual config directory
    pub fn get_actual_config_dir(&self) -> &str {
        match &self.custom_dir {
            Some(dir) => dir,
            None => CONFIG_DIR,
        }
    }

    // full path to the config file
    pub fn get_config_path(&self, filename: &str) -> PathBuf {
        PathBuf::from(self.get_actual_config_dir()).join(filename)
    }

    // generate config file content
    pub fn generate_config_content(&self) -> String {
        let mut lines = Vec::new();
        lines.push("# Static IPv4 configuration".to_string());

        if let Some(ip) = &self.ip_address {
            lines.push(format!("ip = {}", ip));
        }

        if let Some(netmask) = &self.netmask {
            lines.push(format!("netmask = {}", netmask));
        }

        if let Some(gateway) = &self.gateway {
            lines.push(format!("gateway = {}", gateway));
        }

        if let Some(dns_servers) = &self.dns_servers {
            if !dns_servers.is_empty() {
                lines.push(format!("dns = {}", dns_servers.join(" ")));
            }
        }

        lines.join("\n")
    }
}

// ensure the directory exists
fn ensure_directory_exists(dir_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !dir_path.exists() {
        fs::create_dir_all(dir_path)?;
        println!("[INFO] created directory: {:?}", dir_path);
    }
    Ok(())
}

// default config file path
pub fn get_default_config_path(filename: &str) -> PathBuf {
    PathBuf::from(CONFIG_DIR).join(filename)
}

// update static IPv4 config
pub fn update_static_ipv4_config(
    config: &StaticIpConfigInfo,
) -> Result<(), Box<dyn std::error::Error>> {
    // resolve file path
    let config_dir = config.get_actual_config_dir();
    let config_file = config.get_config_path(CONFIG_FILENAME);

    // ensure directory exists
    ensure_directory_exists(Path::new(config_dir))?;

    // check whether the file already exists
    let config_exists = config_file.exists();

    if config_exists {
        //println!("[INFO] config file {:?} exists, overwriting...", config_file);
    } else {
        // println!("[INFO] creating config file: {:?}", config_file);
    }

    // generate config content
    let config_content = config.generate_config_content();

    // write config file (overwrite if present)
    match fs::write(&config_file, config_content) {
        Ok(_) => {
            //  println!("[SUCCESS] config file created/updated: {:?}", config_file);
            Ok(())
        }
        Err(e) => {
            // eprintln!("[ERROR] failed to write config file: {}", e);
            Err(e.into())
        }
    }
}

pub fn remove_static_ipv4_config() {
    let _ = cleanup_config_file(None);
}

// read existing config
pub fn read_existing_config(
    config_dir: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let config_path = match config_dir {
        Some(dir) => PathBuf::from(dir).join(CONFIG_FILENAME),
        None => get_default_config_path(CONFIG_FILENAME),
    };

    if config_path.exists() {
        let content = fs::read_to_string(config_path)?;
        Ok(content)
    } else {
        Err(format!("config file not found: {:?}", config_path).into())
    }
}

// remove config file
pub fn cleanup_config_file(config_dir: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = match config_dir {
        Some(dir) => PathBuf::from(dir).join(CONFIG_FILENAME),
        None => get_default_config_path(CONFIG_FILENAME),
    };

    if config_path.exists() {
        fs::remove_file(&config_path)?;
    }
    Ok(())
}

pub fn restart_network() {
    let reuslt = run_command("/etc/init.d/S16network", &["restart"]);
}

// run a command and return stdout
fn run_command(cmd: &str, args: &[&str]) -> Result<String, String> {
    let output =
        Command::new(cmd).args(args).output().map_err(|e| format!("failed to run command: {}", e))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(format!("command failed: {}", stderr))
    }
}

fn test() {
    let mut config = StaticIpConfigInfo::new();
    config.with_ip("10.1.1.111");
    config.with_netmask("255.255.255.0");
    config.with_gateway("10.1.1.1");
    config.with_dns(vec!["8.8.8.8", "8.8.8.9"]);
    let _ = update_static_ipv4_config(&config);
    restart_network();
}
