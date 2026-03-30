use serde::{Deserialize, Serialize};

/// mDNS listening options
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MdnsListenOptions {
    pub ipv4: bool,
    pub ipv6: bool,
}

impl Default for MdnsListenOptions {
    fn default() -> Self {
        Self { ipv4: true, ipv6: true }
    }
}

/// mDNS configuration options
#[derive(Debug, Clone, Default)]
pub struct MdnsOptions {
    pub local_names: Vec<String>,
    pub listen_options: MdnsListenOptions,
}

impl MdnsOptions {
    pub fn new(local_names: Vec<String>, listen_options: MdnsListenOptions) -> Self {
        Self { local_names, listen_options }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mdns_listen_options_default() {
        let options = MdnsListenOptions::default();
        assert!(options.ipv4);
        assert!(options.ipv6);
    }

    #[test]
    fn test_mdns_listen_options_custom() {
        let options = MdnsListenOptions { ipv4: false, ipv6: true };
        assert!(!options.ipv4);
        assert!(options.ipv6);
    }

    #[test]
    fn test_mdns_options_default() {
        let options = MdnsOptions::default();
        assert!(options.local_names.is_empty());
        assert!(options.listen_options.ipv4);
        assert!(options.listen_options.ipv6);
    }

    #[test]
    fn test_mdns_options_new() {
        let local_names = vec!["test".to_string()];
        let listen_options = MdnsListenOptions { ipv4: true, ipv6: false };
        let options = MdnsOptions::new(local_names.clone(), listen_options);

        assert_eq!(options.local_names, local_names);
        assert_eq!(options.listen_options, listen_options);
    }

    #[test]
    fn test_mdns_listen_options_equality() {
        let options1 = MdnsListenOptions { ipv4: true, ipv6: false };
        let options2 = MdnsListenOptions { ipv4: true, ipv6: false };
        let options3 = MdnsListenOptions { ipv4: false, ipv6: true };

        assert_eq!(options1, options2);
        assert_ne!(options1, options3);
    }

    #[test]
    fn test_mdns_options_serialization() {
        let options = MdnsListenOptions { ipv4: true, ipv6: false };
        let serialized = serde_json::to_string(&options).unwrap();
        let deserialized: MdnsListenOptions = serde_json::from_str(&serialized).unwrap();

        assert_eq!(options, deserialized);
    }
}

