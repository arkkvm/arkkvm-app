use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use parking_lot::Mutex as ParkingMutex;
use rcgen::{
    Certificate, CertificateParams, DistinguishedName, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SanType,
};
use time::{Duration, OffsetDateTime};
use tracing::{info, warn};

const CA_DIR: &str = "/userdata/arkkvm/certs";
const CA_CERT_FILE: &str = "ca_cert.pem";
const CA_KEY_FILE: &str = "ca_key.pem";
const SERVER_CERT_FILE: &str = "server_cert.pem";
const SERVER_KEY_FILE: &str = "server_key.pem";
const SERVER_DOMAINS_FILE: &str = "server_domains.txt";

/// Optional override for the cert directory (tests).
static CERT_DIR_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Serialize get-or-create so concurrent mDNS NameChange reloads share one leaf.
static SERVER_CERT_LOCK: ParkingMutex<()> = ParkingMutex::new(());

fn cert_dir() -> PathBuf {
    CERT_DIR_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| PathBuf::from(CA_DIR))
}

#[cfg(test)]
fn set_cert_dir_for_test(path: PathBuf) {
    *CERT_DIR_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(path);
}

#[cfg(test)]
fn clear_cert_dir_override_for_test() {
    *CERT_DIR_OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

/// Normalize domain/IP list for SAN-set comparison and on-disk metadata.
pub fn normalize_domains(domains: &[String]) -> Vec<String> {
    let mut out: Vec<String> = domains
        .iter()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Generate root CA certificate, its key pair, and parameters for Issuer
fn generate_ca_cert() -> anyhow::Result<(Certificate, CertificateParams, KeyPair)> {
    let mut params = CertificateParams::default();

    // Mark this certificate as CA
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);

    // Set CA key usages
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];

    // Set certificate distinguished name (DN)
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(rcgen::DnType::CommonName, "ARKKVM Root CA");
    distinguished_name.push(rcgen::DnType::OrganizationName, "ARKKVM TEAM");
    params.distinguished_name = distinguished_name;

    // Set certificate validity period (e.g., 20 years)
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + Duration::days(365 * 20);

    // Generate key pair and self-sign to create CA certificate
    let ca_key_pair = KeyPair::generate()?;
    let ca_cert = params.self_signed(&ca_key_pair)?;

    Ok((ca_cert, params, ca_key_pair))
}

/// Load CA certificate and private key from file
fn load_ca_cert(
    ca_cert_path: &str,
    ca_key_path: &str,
) -> anyhow::Result<(Issuer<'static, KeyPair>, String)> {
    let ca_cert_pem = fs::read_to_string(ca_cert_path)?;
    let ca_key_pem = fs::read_to_string(ca_key_path)?;
    let ca_key = KeyPair::from_pem(ca_key_pem.as_str())?;
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem.as_str(), ca_key)?;
    Ok((issuer, ca_key_pem))
}

/// Save CA certificate and private key to file
fn save_ca_cert(ca_cert_pem: &str, ca_key_pem: &str) -> anyhow::Result<()> {
    let dir = cert_dir();
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(CA_CERT_FILE), ca_cert_pem)?;
    fs::write(dir.join(CA_KEY_FILE), ca_key_pem)?;
    info!("Generated and saved CA to {} and {}", CA_CERT_FILE, CA_KEY_FILE);
    Ok(())
}

/// Try to load CA from disk; if not exists, generate and save. Return Issuer and CA certificate PEM.
pub fn load_or_init_ca() -> anyhow::Result<(Issuer<'static, KeyPair>, String)> {
    info!("Loading or initializing CA");
    let dir = cert_dir();
    let ca_cert_path = dir.join(CA_CERT_FILE);
    let ca_key_path = dir.join(CA_KEY_FILE);
    if let Ok((issuer, ca_cert_pem)) = load_ca_cert(
        ca_cert_path.to_str().unwrap_or_default(),
        ca_key_path.to_str().unwrap_or_default(),
    ) {
        return Ok((issuer, ca_cert_pem));
    }

    info!("Generating and saving CA");
    let (ca_cert, params, ca_key) = generate_ca_cert()?;

    let ca_cert_pem = ca_cert.pem();
    let ca_key_pem = ca_key.serialize_pem();

    save_ca_cert(&ca_cert_pem, &ca_key_pem)?;

    // Construct Issuer (with parameters and key)
    info!("Constructing Issuer");
    let issuer = Issuer::new(params, ca_key);

    Ok((issuer, ca_cert_pem))
}

/// Issue and generate a server certificate signed by the given CA
pub fn generate_server_cert(
    issuer: &Issuer<'_, KeyPair>,
    domains: Vec<String>,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    // server certificate parameters
    let mut params = CertificateParams::new(domains.clone())?;

    // distinguished name
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(rcgen::DnType::CommonName, "ARKKVM");
    params.distinguished_name = distinguished_name;

    // subject alternative names (SAN)
    let mut san = Vec::new();
    for domain in domains {
        san.push(SanType::DnsName(domain.try_into()?));
    }
    san.push(SanType::IpAddress("127.0.0.1".parse()?));
    params.subject_alt_names = san;

    // validity period (e.g. 1 year)
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = OffsetDateTime::now_utc() + Duration::days(365);

    // new server key pair
    let server_key_pair = KeyPair::generate()?;

    // sign server cert with CA
    let server_cert = params.signed_by(&server_key_pair, issuer)?;

    let server_cert_pem = server_cert.pem();
    let server_key_pem = server_key_pair.serialize_pem();

    Ok((server_cert_pem, server_key_pem))
}

fn load_cached_server_cert(dir: &Path) -> Option<(String, String, Vec<String>)> {
    let cert_pem = fs::read_to_string(dir.join(SERVER_CERT_FILE)).ok()?;
    let key_pem = fs::read_to_string(dir.join(SERVER_KEY_FILE)).ok()?;
    let domains_raw = fs::read_to_string(dir.join(SERVER_DOMAINS_FILE)).ok()?;
    if cert_pem.trim().is_empty() || key_pem.trim().is_empty() {
        return None;
    }
    let domains: Vec<String> = domains_raw
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let domains = normalize_domains(&domains);
    Some((cert_pem, key_pem, domains))
}

fn save_server_cert(dir: &Path, cert_pem: &str, key_pem: &str, domains: &[String]) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join(SERVER_CERT_FILE), cert_pem)?;
    fs::write(dir.join(SERVER_KEY_FILE), key_pem)?;
    fs::write(dir.join(SERVER_DOMAINS_FILE), domains.join("\n") + "\n")?;
    info!(
        domain_count = domains.len(),
        "Saved self-signed server leaf certificate for SAN set"
    );
    Ok(())
}

/// Return a server leaf cert for `domains`, reusing the on-disk leaf when the
/// normalized SAN input set matches the cached metadata.
pub fn get_or_create_server_cert(
    issuer: &Issuer<'_, KeyPair>,
    domains: Vec<String>,
) -> anyhow::Result<(String, String)> {
    let _guard = SERVER_CERT_LOCK.lock();
    let wanted = normalize_domains(&domains);
    let dir = cert_dir();

    if let Some((cert_pem, key_pem, cached_domains)) = load_cached_server_cert(&dir) {
        if cached_domains == wanted {
            info!(
                domain_count = wanted.len(),
                "Reusing cached self-signed server leaf (SAN set unchanged)"
            );
            return Ok((cert_pem, key_pem));
        }
        info!(
            cached = cached_domains.len(),
            wanted = wanted.len(),
            "SAN set changed; issuing new self-signed server leaf"
        );
    } else {
        info!("No cached server leaf; issuing new self-signed certificate");
    }

    let (cert_pem, key_pem) = generate_server_cert(issuer, wanted.clone())
        .map_err(|e| anyhow::anyhow!("generate server certificate failed: {e}"))?;
    if let Err(e) = save_server_cert(&dir, &cert_pem, &key_pem, &wanted) {
        warn!(error = %e, "Failed to persist server leaf certificate; continuing with in-memory cert");
    }
    Ok((cert_pem, key_pem))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Serialize tests that mutate CERT_DIR_OVERRIDE.
    static TEST_DIR_LOCK: StdMutex<()> = StdMutex::new(());

    fn with_temp_cert_dir<F: FnOnce(PathBuf)>(f: F) {
        let _lock = TEST_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("arkkvm-cert-test-{nanos}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp cert dir");
        set_cert_dir_for_test(dir.clone());
        f(dir.clone());
        clear_cert_dir_override_for_test();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_normalize_domains_sorts_dedups() {
        let domains = vec![
            " b.example ".to_string(),
            "a.example".to_string(),
            "a.example".to_string(),
            "".to_string(),
            "  ".to_string(),
        ];
        assert_eq!(
            normalize_domains(&domains),
            vec!["a.example".to_string(), "b.example".to_string()]
        );
    }

    #[test]
    fn test_get_or_create_reuses_same_san_set() {
        with_temp_cert_dir(|_dir| {
            let (issuer, _) = load_or_init_ca().expect("init CA");
            let domains = vec![
                "localhost".to_string(),
                "arkkvm.lan.local".to_string(),
                "arkkvm-2.lan.local".to_string(),
            ];
            let (c1, k1) =
                get_or_create_server_cert(&issuer, domains.clone()).expect("first issue");
            let (c2, k2) =
                get_or_create_server_cert(&issuer, domains.clone()).expect("second issue");
            assert_eq!(c1, c2);
            assert_eq!(k1, k2);

            // Order / whitespace must not force reissue.
            let shuffled = vec![
                " arkkvm-2.lan.local".to_string(),
                "localhost".to_string(),
                "arkkvm.lan.local".to_string(),
            ];
            let (c3, k3) = get_or_create_server_cert(&issuer, shuffled).expect("shuffled");
            assert_eq!(c1, c3);
            assert_eq!(k1, k3);
        });
    }

    #[test]
    fn test_get_or_create_reissues_when_san_changes() {
        with_temp_cert_dir(|dir| {
            let (issuer, _) = load_or_init_ca().expect("init CA");
            let d1 = vec!["localhost".to_string(), "arkkvm.lan.local".to_string()];
            let (c1, _) = get_or_create_server_cert(&issuer, d1).expect("first");
            let d2 = vec![
                "localhost".to_string(),
                "arkkvm.lan.local".to_string(),
                "arkkvm-2.lan.local".to_string(),
            ];
            let (c2, _) = get_or_create_server_cert(&issuer, d2.clone()).expect("changed");
            assert_ne!(c1, c2);

            let meta = fs::read_to_string(dir.join(SERVER_DOMAINS_FILE)).expect("domains file");
            let stored = normalize_domains(
                &meta
                    .lines()
                    .map(|l| l.to_string())
                    .collect::<Vec<_>>(),
            );
            assert_eq!(stored, normalize_domains(&d2));
        });
    }
}
