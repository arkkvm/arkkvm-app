use std::fs;

use rcgen::{
    Certificate, CertificateParams, DistinguishedName, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SanType,
};
use time::{Duration, OffsetDateTime};
use tracing::info;

const CA_DIR: &str = "/userdata/arkkvm/certs";
const CA_CERT_FILE: &str = "ca_cert.pem";
const CA_KEY_FILE: &str = "ca_key.pem";

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
    fs::create_dir_all(CA_DIR)?;
    fs::write(format!("{}/{}", CA_DIR, CA_CERT_FILE), ca_cert_pem)?;
    fs::write(format!("{}/{}", CA_DIR, CA_KEY_FILE), ca_key_pem)?;
    info!("Generated and saved CA to {} and {}", CA_CERT_FILE, CA_KEY_FILE);
    Ok(())
}

/// Try to load CA from disk; if not exists, generate and save. Return Issuer and CA certificate PEM.
pub fn load_or_init_ca() -> anyhow::Result<(Issuer<'static, KeyPair>, String)> {
    info!("Loading or initializing CA");
    if let Ok((issuer, ca_cert_pem)) = load_ca_cert(format!("{}/{}", CA_DIR, CA_CERT_FILE).as_str(), format!("{}/{}", CA_DIR, CA_KEY_FILE).as_str()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_ca_and_server_cert() {
        // load or initialize CA
        let (issuer, ca_cert_pem) = load_or_init_ca().expect("failed to init/load CA");

        info!("CA certificate PEM:\n{}", ca_cert_pem);

        let domains = vec!["arkkvm.dev".to_string(), "localhost".to_string()];
        let (server_cert_pem, server_key_pem) =
            generate_server_cert(&issuer, domains).expect("failed to generate server certificate");

        info!("\nServer certificate generated and signed successfully.");
        info!("Server certificate PEM:\n{}", server_cert_pem);
        info!("Server private key PEM:\n{}", server_key_pem);

        // basic PEM format assertions
        assert!(ca_cert_pem.contains("-----BEGIN CERTIFICATE-----"));
        assert!(server_cert_pem.contains("-----BEGIN CERTIFICATE-----"));
        assert!(server_key_pem.contains("-----BEGIN PRIVATE KEY-----"));
    }
}
