//! Thruster's configuration (internal/config.go): every setting is read from `THRUSTER_<NAME>`,
//! falling back to `<NAME>`, and a value that doesn't parse falls back to the default.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

const KB: i64 = 1024;
const MB: i64 = 1024 * KB;

/// Let's Encrypt's production directory (`acme.LetsEncryptURL`).
pub const LETS_ENCRYPT_URL: &str = "https://acme-v02.api.letsencrypt.org/directory";

#[derive(Debug, Clone)]
pub struct FrontConfig {
    /// `TARGET_PORT`: where Thruster's upstream listened. Thruster exported it to the app as
    /// `PORT`, so the app itself still listens there.
    pub target_port: u16,
    /// `TARGET_BIND` (not Thruster's): the address the app's own listener on TARGET_PORT binds.
    /// Loopback by default, since nothing outside needs it and it lacks the front's protections.
    pub target_bind: IpAddr,
    /// `CACHE_SIZE`: the response cache's capacity in bytes.
    pub cache_size: i64,
    /// `MAX_CACHE_ITEM_SIZE`: the largest response the cache stores.
    pub max_cache_item_size: i64,
    pub gzip_compression_enabled: bool,
    pub gzip_compression_disable_on_auth: bool,
    pub gzip_compression_jitter: i64,
    /// `MAX_REQUEST_BODY`: 0 for no limit.
    pub max_request_body: i64,
    /// `TLS_DOMAIN`, comma-separated. Setting it turns on TLS.
    pub tls_domains: Vec<String>,
    /// `ACME_DIRECTORY`
    pub acme_directory_url: String,
    pub eab_kid: String,
    pub eab_hmac_key: String,
    /// `STORAGE_PATH`: autocert's certificate cache directory.
    pub storage_path: PathBuf,
    pub http_port: u16,
    pub https_port: u16,
    pub http_idle_timeout: Duration,
    pub http_read_timeout: Duration,
    pub http_write_timeout: Duration,
    /// `H2C_ENABLED`: HTTP/2 without TLS (prior knowledge).
    pub h2c_enabled: bool,
    /// `FORWARD_HEADERS`: trust the client's `X-Forwarded-*` (default: only without TLS).
    pub forward_headers: bool,
    /// `DEBUG`
    pub debug: bool,
    /// `LOG_REQUESTS`
    pub log_requests: bool,
    /// `RECOVER_UPGRADE_HEADERS` (not Thruster's): put back the `Upgrade` and `Connection` headers
    /// a reverse proxy dropped from a WebSocket handshake. See `front::upgrade_recovery`.
    pub recover_upgrade_headers: bool,
}

impl FrontConfig {
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// `NewConfig`, over any variable lookup (tests pass a map).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let find = |key: &str| get(&format!("THRUSTER_{key}")).or_else(|| get(key));
        let string = |key: &str, default: &str| find(key).unwrap_or_else(|| default.to_string());
        // strconv.Atoi: an optional sign and decimal digits, nothing else.
        let int = |key: &str, default: i64| find(key).and_then(|v| v.parse::<i64>().ok()).unwrap_or(default);
        let port = |key: &str, default: u16| u16::try_from(int(key, default.into())).unwrap_or(default);
        let seconds = |key: &str, default: u64| {
            find(key)
                .and_then(|v| v.parse::<i64>().ok())
                .map(|s| Duration::from_secs(s.max(0) as u64))
                .unwrap_or(Duration::from_secs(default))
        };
        let boolean = |key: &str, default: bool| find(key).and_then(|v| parse_bool(&v)).unwrap_or(default);

        let tls_domains = find("TLS_DOMAIN")
            .map(|v| v.split(',').map(str::trim).filter(|d| !d.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        let mut config = Self {
            target_port: port("TARGET_PORT", 3000),
            target_bind: find("TARGET_BIND").and_then(|v| v.parse().ok()).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            cache_size: int("CACHE_SIZE", 64 * MB),
            max_cache_item_size: int("MAX_CACHE_ITEM_SIZE", MB),
            gzip_compression_enabled: boolean("GZIP_COMPRESSION_ENABLED", true),
            gzip_compression_disable_on_auth: boolean("GZIP_COMPRESSION_DISABLE_ON_AUTH", false),
            gzip_compression_jitter: int("GZIP_COMPRESSION_JITTER", 32),
            max_request_body: int("MAX_REQUEST_BODY", 0),
            tls_domains,
            acme_directory_url: string("ACME_DIRECTORY", LETS_ENCRYPT_URL),
            eab_kid: string("EAB_KID", ""),
            eab_hmac_key: string("EAB_HMAC_KEY", ""),
            storage_path: PathBuf::from(string("STORAGE_PATH", "./storage/thruster")),
            http_port: port("HTTP_PORT", 80),
            https_port: port("HTTPS_PORT", 443),
            http_idle_timeout: seconds("HTTP_IDLE_TIMEOUT", 60),
            http_read_timeout: seconds("HTTP_READ_TIMEOUT", 30),
            http_write_timeout: seconds("HTTP_WRITE_TIMEOUT", 30),
            h2c_enabled: boolean("H2C_ENABLED", false),
            forward_headers: false,
            debug: boolean("DEBUG", false),
            log_requests: boolean("LOG_REQUESTS", true),
            recover_upgrade_headers: boolean("RECOVER_UPGRADE_HEADERS", false),
        };
        config.forward_headers = boolean("FORWARD_HEADERS", !config.has_tls());
        config
    }

    pub fn has_tls(&self) -> bool {
        !self.tls_domains.is_empty()
    }
}

/// Go's `strconv.ParseBool`.
fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> FrontConfig {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        FrontConfig::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn defaults() {
        let c = config(&[]);
        assert_eq!((c.http_port, c.https_port, c.target_port), (80, 443, 3000));
        assert_eq!(c.target_bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(c.cache_size, 64 * 1024 * 1024);
        assert_eq!(c.max_cache_item_size, 1024 * 1024);
        assert_eq!(c.http_idle_timeout, Duration::from_secs(60));
        assert_eq!(c.http_read_timeout, Duration::from_secs(30));
        assert_eq!(c.http_write_timeout, Duration::from_secs(30));
        assert_eq!(c.storage_path, PathBuf::from("./storage/thruster"));
        assert_eq!(c.acme_directory_url, LETS_ENCRYPT_URL);
        assert_eq!(c.gzip_compression_jitter, 32);
        assert!(c.gzip_compression_enabled && !c.gzip_compression_disable_on_auth && !c.h2c_enabled);
        assert!(!c.has_tls() && c.forward_headers && c.log_requests);
    }

    #[test]
    fn prefixed_variables_win() {
        let c = config(&[("HTTP_PORT", "8080"), ("THRUSTER_HTTP_PORT", "9090"), ("HTTPS_PORT", "8443")]);
        assert_eq!((c.http_port, c.https_port), (9090, 8443));
    }

    #[test]
    fn target_bind_can_open_the_app_listener() {
        assert_eq!(config(&[("TARGET_BIND", "0.0.0.0")]).target_bind, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(config(&[("TARGET_BIND", "::")]).target_bind.to_string(), "::");
        assert_eq!(config(&[("TARGET_BIND", "everywhere")]).target_bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn unparseable_values_fall_back_to_the_default() {
        let c = config(&[("HTTP_PORT", "eighty"), ("HTTP_READ_TIMEOUT", "5s"), ("LOG_REQUESTS", "yes"), ("H2C_ENABLED", "1")]);
        assert_eq!(c.http_port, 80);
        assert_eq!(c.http_read_timeout, Duration::from_secs(30));
        assert!(c.log_requests && c.h2c_enabled);
    }

    #[test]
    fn tls_domains_turn_off_forwarded_headers() {
        let c = config(&[("TLS_DOMAIN", " chat.example.com, ,other.example.com ")]);
        assert_eq!(c.tls_domains, vec!["chat.example.com", "other.example.com"]);
        assert!(c.has_tls() && !c.forward_headers);
        assert!(config(&[("TLS_DOMAIN", "a.example.com"), ("FORWARD_HEADERS", "true")]).forward_headers);
        // Only TLS_DOMAIN counts (Thruster 0.1.23 doesn't read SSL_DOMAIN).
        assert!(!config(&[("SSL_DOMAIN", "a.example.com")]).has_tls());
        assert!(!config(&[("TLS_DOMAIN", " , ")]).has_tls());
    }
}
