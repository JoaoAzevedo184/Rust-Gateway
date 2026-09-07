//! Endereço do cliente e confiança em `X-Forwarded-For` (spec §5.2).
//!
//! Rate limit por IP que confia em `X-Forwarded-For` sem validar a origem é
//! trivialmente burlável: o cliente forja o header e ganha um bucket novo por
//! requisição. Por padrão o gateway ignora o header e usa o endereço do peer.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use http::HeaderMap;
use ipnet::IpNet;

/// Endereço do socket do cliente, injetado pelo make-service do listener público.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

#[derive(Debug, Default, Clone)]
pub struct TrustedProxies {
    nets: Arc<Vec<IpNet>>,
}

impl TrustedProxies {
    pub fn new(nets: Vec<IpNet>) -> Self {
        Self {
            nets: Arc::new(nets),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    pub fn is_trusted(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|net| net.contains(&ip))
    }

    /// Endereço a ser usado como identidade de rede do cliente.
    ///
    /// Com o peer fora da lista de confiança, o header é ignorado por completo.
    /// Com o peer confiável, percorre a cadeia da direita para a esquerda e toma
    /// o último endereço **não** confiável — o primeiro salto que o gateway não
    /// controla, e portanto o mais próximo do cliente real que ainda é verificável.
    pub fn client_ip(&self, headers: &HeaderMap, peer: IpAddr) -> IpAddr {
        if !self.is_trusted(peer) {
            return peer;
        }

        let chain: Vec<IpAddr> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
            .collect();

        chain
            .iter()
            .rev()
            .find(|ip| !self.is_trusted(**ip))
            .copied()
            .or_else(|| chain.first().copied())
            .unwrap_or(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_xff(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", value.parse().unwrap());
        headers
    }

    #[test]
    fn peer_nao_confiavel_ignora_x_forwarded_for() {
        let trusted = TrustedProxies::default();
        let headers = headers_with_xff("1.2.3.4");
        let peer: IpAddr = "10.0.0.9".parse().unwrap();

        assert_eq!(trusted.client_ip(&headers, peer), peer);
    }

    #[test]
    fn peer_confiavel_toma_o_ultimo_endereco_nao_confiavel() {
        let trusted = TrustedProxies::new(vec!["10.0.0.0/8".parse().unwrap()]);
        let headers = headers_with_xff("203.0.113.7, 10.0.0.5, 10.0.0.6");
        let peer: IpAddr = "10.0.0.6".parse().unwrap();

        assert_eq!(
            trusted.client_ip(&headers, peer),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn cadeia_toda_confiavel_cai_no_primeiro_endereco() {
        let trusted = TrustedProxies::new(vec!["10.0.0.0/8".parse().unwrap()]);
        let headers = headers_with_xff("10.0.0.1, 10.0.0.2");
        let peer: IpAddr = "10.0.0.2".parse().unwrap();

        assert_eq!(
            trusted.client_ip(&headers, peer),
            "10.0.0.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn sem_header_usa_o_peer() {
        let trusted = TrustedProxies::new(vec!["10.0.0.0/8".parse().unwrap()]);
        let peer: IpAddr = "10.0.0.2".parse().unwrap();

        assert_eq!(trusted.client_ip(&HeaderMap::new(), peer), peer);
    }
}
