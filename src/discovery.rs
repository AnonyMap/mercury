//! Find OpenAI-compatible servers on this machine and the local network.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use futures_util::stream::{self, StreamExt};

use crate::config::{Config, ProviderConfig};
use crate::provider::Client;

/// Default ports of the common local inference servers.
pub const KNOWN_PORTS: &[(u16, &str)] = &[
    (11434, "Ollama"),
    (1234, "LM Studio"),
    (8080, "llama.cpp"),
    (8000, "vLLM"),
    (5001, "KoboldCpp"),
    (1337, "Jan"),
    (4891, "GPT4All"),
    (8888, "OpenAI-compatible"),
];

#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    pub server: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub is_this_machine: bool,
}

impl Discovered {
    pub fn display_name(&self) -> String {
        let host = url::Url::parse(&self.base_url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        if self.is_this_machine { format!("{} (this machine)", self.server) } else { format!("{} @ {host}", self.server) }
    }

    /// A provider entry for this server, with an id unique within `cfg`.
    pub fn to_provider(&self, cfg: &Config) -> ProviderConfig {
        let id = crate::config::unique_provider_id(cfg, &self.display_name().replace("(this machine)", ""));
        let mut p = ProviderConfig::openai(&id, &self.display_name(), &self.base_url);
        p.default_model = self.models.first().cloned().unwrap_or_default();
        p
    }
}

/// Probe one base URL: is there an OpenAI-compatible `/models` behind it?
pub async fn probe(base_url: &str, server: &str, timeout: Duration) -> Option<Discovered> {
    let client = Client::new(base_url, None);
    let models = tokio::time::timeout(timeout, client.list_models()).await.ok()?.ok()?;
    let is_this_machine = url::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host().map(|h| matches!(h, url::Host::Ipv4(ip) if ip.is_loopback()) || h.to_string() == "localhost"))
        .unwrap_or(false);
    Some(Discovered { server: server.into(), base_url: base_url.trim_end_matches('/').into(), models, is_this_machine })
}

async fn port_open(addr: SocketAddr, timeout: Duration) -> bool {
    matches!(tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await, Ok(Ok(_)))
}

/// Probe every (host, port) pair. A cheap TCP connect filters closed ports before any HTTP.
pub async fn discover(hosts: &[IpAddr], ports: &[(u16, &str)], connect_timeout: Duration, http_timeout: Duration) -> Vec<Discovered> {
    let targets: Vec<(IpAddr, u16, String)> = hosts.iter().flat_map(|h| ports.iter().map(move |(p, n)| (*h, *p, n.to_string()))).collect();
    let open: Vec<(IpAddr, u16, String)> = stream::iter(targets)
        .map(|(h, p, n)| async move { port_open(SocketAddr::new(h, p), connect_timeout).await.then_some((h, p, n)) })
        .buffer_unordered(256)
        .filter_map(|x| async move { x })
        .collect()
        .await;
    let mut found: Vec<Discovered> = stream::iter(open)
        .map(|(h, p, n)| async move {
            let host = match h {
                IpAddr::V4(v) => v.to_string(),
                IpAddr::V6(v) => format!("[{v}]"),
            };
            probe(&format!("http://{host}:{p}/v1"), &n, http_timeout).await
        })
        .buffer_unordered(32)
        .filter_map(|x| async move { x })
        .collect()
        .await;
    found.sort_by(|a, b| (!a.is_this_machine, &a.base_url).cmp(&(!b.is_this_machine, &b.base_url)));
    found
}

pub async fn discover_local() -> Vec<Discovered> {
    discover(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], KNOWN_PORTS, Duration::from_millis(300), Duration::from_secs(3)).await
}

/// This machine's primary LAN address (no packets are sent; UDP `connect` only picks a route).
pub fn primary_ipv4() -> Option<Ipv4Addr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Every other host in `ip`'s /24.
pub fn hosts_in_24(ip: Ipv4Addr) -> Vec<IpAddr> {
    let [a, b, c, own] = ip.octets();
    (1..=254u8).filter(|&d| d != own).map(|d| IpAddr::V4(Ipv4Addr::new(a, b, c, d))).collect()
}

/// Sweep the local /24 for inference servers on the known ports.
pub async fn scan_lan() -> Vec<Discovered> {
    let Some(ip) = primary_ipv4() else { return vec![] };
    if !ip.is_private() && !ip.is_link_local() {
        // Don't sweep a public subnet.
        return vec![];
    }
    discover(&hosts_in_24(ip), KNOWN_PORTS, Duration::from_millis(250), Duration::from_secs(3)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_hosts() {
        let hosts = hosts_in_24(Ipv4Addr::new(192, 168, 1, 7));
        assert_eq!(hosts.len(), 253);
        assert!(!hosts.contains(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 7))));
        assert!(hosts.contains(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 254))));
    }

    #[test]
    fn provider_from_discovery() {
        let d = Discovered {
            server: "Ollama".into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            models: vec!["qwen3:8b".into()],
            is_this_machine: true,
        };
        let cfg = Config::default();
        let p = d.to_provider(&cfg);
        assert_eq!(p.id, "ollama");
        assert_eq!(p.name, "Ollama (this machine)");
        assert_eq!(p.default_model, "qwen3:8b");
        let lan =
            Discovered { server: "vLLM".into(), base_url: "http://192.168.1.50:8000/v1".into(), models: vec![], is_this_machine: false };
        assert_eq!(lan.display_name(), "vLLM @ 192.168.1.50");
        assert_eq!(lan.to_provider(&cfg).id, "vllm-192-168-1-50");
    }
}
