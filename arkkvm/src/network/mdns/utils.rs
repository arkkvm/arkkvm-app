use crate::network::mdns::{Mdns, MdnsListenOptions, MdnsOptions};

pub async fn create_mdns_service(
    hostname: String,
    fqdn: String,
    ipv4: bool,
    ipv6: bool,
) -> anyhow::Result<Mdns> {
    let mdns = Mdns::new(MdnsOptions {
        local_names: vec![hostname, fqdn],
        listen_options: MdnsListenOptions { ipv4, ipv6 },
    })?;

    Ok(mdns)
}

pub async fn start_mdns_service(mdns: &Mdns) -> anyhow::Result<()> {
    mdns.start().await?;
    Ok(())
}

pub async fn stop_mdns_service(mdns: &Mdns) -> anyhow::Result<()> {
    mdns.stop().await?;
    Ok(())
}

