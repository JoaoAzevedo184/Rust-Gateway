//! Proxy reverso: cliente, higiene de headers e encaminhamento (spec §7).

pub mod limit;
pub mod scrub;

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use http::header::{HOST, HeaderName};
use http::uri::PathAndQuery;
use http::{HeaderMap, HeaderValue, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tower::Service;

use crate::error::GatewayError;
use crate::observability::metrics::{self, Metrics, Outcome};
use crate::peer::{PeerAddr, TrustedProxies};
use crate::resilience::AttemptOutcome;
use crate::routing::layer::route_of;
use crate::{BoxFuture, Request, Response};

/// Headers hop-by-hop: pertencem a uma conexão, não à mensagem, e repassá-los
/// corrompe a semântica da conexão seguinte.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Remove os hop-by-hop fixos **e** todo header nomeado dentro de `Connection`.
/// Ignorar a segunda parte deixa passar exatamente os headers que o peer anterior
/// declarou como sendo só daquela conexão.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| name.trim().parse::<HeaderName>().ok())
        .collect();

    for name in listed {
        while headers.remove(&name).is_some() {}
    }

    for name in HOP_BY_HOP {
        let name = HeaderName::from_static(name);
        while headers.remove(&name).is_some() {}
    }
}

type HttpClient = Client<HttpConnector, Body>;

pub struct ProxyClient {
    /// Um cliente por `connect_timeout` distinto. O timeout de conexão é
    /// propriedade do conector, fixada quando o cliente é construído, então
    /// honrar a herança por upstream exige um pool por valor configurado — e não
    /// um por upstream, porque o caso comum é todos compartilharem o default.
    clients: HashMap<Duration, HttpClient>,
    fallback: HttpClient,
    trusted: TrustedProxies,
}

impl std::fmt::Debug for ProxyClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyClient").finish_non_exhaustive()
    }
}

impl ProxyClient {
    /// Cliente hyper com pool de conexões, em vez de `reqwest`, por controle
    /// explícito sobre pooling e timeouts.
    pub fn new(
        trusted: TrustedProxies,
        connect_timeouts: impl IntoIterator<Item = Duration>,
    ) -> Self {
        let clients = connect_timeouts
            .into_iter()
            .map(|timeout| (timeout, build_client(Some(timeout))))
            .collect();

        Self {
            clients,
            fallback: build_client(None),
            trusted,
        }
    }

    fn client_for(&self, connect_timeout: Duration) -> &HttpClient {
        self.clients.get(&connect_timeout).unwrap_or(&self.fallback)
    }
}

fn build_client(connect_timeout: Option<Duration>) -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(connect_timeout);
    connector.set_nodelay(true);

    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .build(connector)
}

#[derive(Debug, Clone)]
pub struct ProxyService {
    client: Arc<ProxyClient>,
    metrics: Arc<Metrics>,
}

impl ProxyService {
    pub fn new(client: Arc<ProxyClient>, metrics: Arc<Metrics>) -> Self {
        Self { client, metrics }
    }
}

impl Service<Request> for ProxyService {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<Result<Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let client = self.client.clone();
        let metrics = self.metrics.clone();

        let Some(route) = route_of(req.extensions()).cloned() else {
            // Inalcançável: `route_resolve` responde 404 antes de chegar aqui.
            let response = GatewayError::not_found().into_response_for(req.extensions());
            return Box::pin(async move { Ok(response) });
        };

        let target = match build_target_uri(&route, req.uri()) {
            Ok(uri) => uri,
            Err(err) => {
                metrics::mark(req.extensions(), Outcome::UpstreamError);
                let response = err.into_response_for(req.extensions());
                return Box::pin(async move { Ok(response) });
            }
        };

        let original_host = req
            .headers()
            .get(HOST)
            .cloned()
            .or_else(|| HeaderValue::from_str(req.uri().host().unwrap_or_default()).ok());

        let peer = req.extensions().get::<PeerAddr>().map(|p| p.0.ip());
        let extensions = req.extensions().clone();

        let headers = req.headers_mut();
        strip_hop_by_hop(headers);
        apply_forwarded(headers, &client.trusted, peer, original_host.as_ref());

        if let Ok(host) = HeaderValue::from_str(route.upstream.authority.as_str()) {
            headers.insert(HOST, host);
        }

        *req.uri_mut() = target;

        let connect_timeout = route.upstream.resilience.connect_timeout;

        Box::pin(async move {
            let upstream_id = route.upstream.id.clone();

            // O gauge cobre a chamada inteira, inclusive o caminho de erro; o
            // histograma mede só a chamada, não o resto do processamento do
            // gateway — é o que responde "a lentidão é do gateway ou do upstream?"
            let _inflight = metrics.track_upstream_inflight(&upstream_id);
            let started = std::time::Instant::now();

            let result = client.client_for(connect_timeout).request(req).await;
            metrics.observe_upstream_duration(&upstream_id, started.elapsed().as_secs_f64());

            match result {
                Ok(upstream) => {
                    let (mut parts, body) = upstream.into_parts();
                    strip_hop_by_hop(&mut parts.headers);

                    let (outcome, attempt) = if parts.status.is_server_error() {
                        (Outcome::UpstreamError, AttemptOutcome::ServerError)
                    } else {
                        (Outcome::Ok, AttemptOutcome::Success)
                    };
                    metrics::mark(&extensions, outcome);

                    // O corpo do upstream é repassado em streaming, sem bufferizar:
                    // um download de 100 MB não pode virar 100 MB de RAM.
                    let mut response = http::Response::from_parts(parts, Body::new(body));
                    attempt.mark(&mut response);
                    Ok(response)
                }
                Err(err) => {
                    tracing::warn!(upstream = %upstream_id, error = %err, "falha ao encaminhar para o upstream");
                    metrics::mark(&extensions, Outcome::UpstreamError);

                    // Um erro devolvido pelo cliente significa que nenhuma resposta
                    // chegou: a falha é comprovadamente pré-resposta, e portanto a
                    // única condição em que repetir é seguro.
                    let mut response = GatewayError::bad_gateway("Upstream is unreachable")
                        .into_response_for(&extensions);
                    AttemptOutcome::PreResponseFailure.mark(&mut response);
                    Ok(response)
                }
            }
        })
    }
}

fn build_target_uri(
    route: &crate::routing::runtime::RouteRuntime,
    original: &Uri,
) -> Result<Uri, GatewayError> {
    let path = route.upstream_path(original.path());

    // A query string é sempre preservada intacta, inclusive quando o prefixo é removido.
    let path_and_query = match original.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };

    let path_and_query: PathAndQuery = path_and_query
        .parse()
        .map_err(|_| GatewayError::bad_gateway("Could not build the upstream URI"))?;

    Uri::builder()
        .scheme(route.upstream.scheme.clone())
        .authority(route.upstream.authority.clone())
        .path_and_query(path_and_query)
        .build()
        .map_err(|_| GatewayError::bad_gateway("Could not build the upstream URI"))
}

/// `X-Forwarded-*` na ida.
///
/// Com o peer fora da lista de confiança, a cadeia recebida é descartada e
/// substituída pelo endereço observado: manter o que o cliente escreveu seria
/// repassar adiante um dado que o gateway acabou de decidir não acreditar.
fn apply_forwarded(
    headers: &mut HeaderMap,
    trusted: &TrustedProxies,
    peer: Option<std::net::IpAddr>,
    original_host: Option<&HeaderValue>,
) {
    const XFF: &str = "x-forwarded-for";
    const XFP: &str = "x-forwarded-proto";
    const XFH: &str = "x-forwarded-host";

    if let Some(peer) = peer {
        let peer_trusted = trusted.is_trusted(peer);

        let chain = if peer_trusted {
            let existing: Vec<String> = headers
                .get_all(XFF)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .map(str::to_string)
                .collect();

            if existing.is_empty() {
                peer.to_string()
            } else {
                format!("{}, {peer}", existing.join(", "))
            }
        } else {
            peer.to_string()
        };

        if let Ok(value) = HeaderValue::from_str(&chain) {
            headers.insert(XFF, value);
        }

        if !peer_trusted || !headers.contains_key(XFP) {
            // O gateway não termina TLS nesta topologia.
            headers.insert(XFP, HeaderValue::from_static("http"));
        }
    }

    if let Some(host) = original_host
        && !headers.contains_key(XFH)
    {
        headers.insert(XFH, host.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResiliencePolicy;
    use crate::routing::runtime::{AuthPolicy, RouteRuntime, UpstreamRuntime};

    fn route(prefix: &str, strip: bool) -> RouteRuntime {
        RouteRuntime {
            id: "r".into(),
            prefix: prefix.to_string(),
            strip_prefix: strip,
            upstream: Arc::new(UpstreamRuntime {
                id: "svc".into(),
                scheme: http::uri::Scheme::HTTP,
                authority: "svc:8080".parse().unwrap(),
                base_path: String::new(),
                resilience: ResiliencePolicy::default(),
            }),
            auth: AuthPolicy::default(),
            limits: Vec::new(),
            resilience: ResiliencePolicy::default(),
        }
    }

    #[test]
    fn a_query_string_e_preservada_intacta_mesmo_com_strip_prefix() {
        let uri: Uri = "/orders/42?fields=a,b&sort=-date".parse().unwrap();
        let target = build_target_uri(&route("/orders", true), &uri).unwrap();

        assert_eq!(
            target.to_string(),
            "http://svc:8080/42?fields=a,b&sort=-date"
        );
    }

    #[test]
    fn remove_hop_by_hop_fixos_e_os_nomeados_no_connection() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", "keep-alive, x-custom-hop".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("x-custom-hop", "segredo".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        strip_hop_by_hop(&mut headers);

        assert!(headers.get("connection").is_none());
        assert!(headers.get("keep-alive").is_none());
        assert!(headers.get("transfer-encoding").is_none());
        assert!(
            headers.get("x-custom-hop").is_none(),
            "header listado no Connection sobreviveu"
        );
        assert_eq!(headers["content-type"], "application/json");
    }

    #[test]
    fn peer_nao_confiavel_tem_sua_cadeia_forjada_descartada() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let peer = Some("10.0.0.9".parse().unwrap());

        apply_forwarded(&mut headers, &TrustedProxies::default(), peer, None);

        assert_eq!(headers["x-forwarded-for"], "10.0.0.9");
        assert_eq!(headers["x-forwarded-proto"], "http");
    }

    #[test]
    fn peer_confiavel_tem_o_proprio_endereco_acrescentado_a_cadeia() {
        let trusted = TrustedProxies::new(vec!["10.0.0.0/8".parse().unwrap()]);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        let peer = Some("10.0.0.6".parse().unwrap());

        apply_forwarded(&mut headers, &trusted, peer, None);

        assert_eq!(headers["x-forwarded-for"], "203.0.113.7, 10.0.0.6");
    }
}
