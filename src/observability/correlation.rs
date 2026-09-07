//! Correlation ID (spec §7.2).

use std::sync::Arc;
use std::task::{Context, Poll};

use http::{HeaderMap, HeaderValue};
use tower::{Layer, Service};

use crate::{BoxFuture, Request, Response};

pub const HEADER: &str = "x-request-id";
const MAX_LEN: usize = 64;

#[derive(Debug, Clone)]
pub struct RequestId(Arc<str>);

impl RequestId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn apply_to(&self, headers: &mut HeaderMap) {
        if let Ok(value) = HeaderValue::from_str(&self.0) {
            headers.insert(HEADER, value);
        }
    }
}

/// Aceita o id do cliente apenas se for bem-formado.
///
/// A validação não é preciosismo: o id entra em log, e log que aceita string
/// arbitrária aceita injeção de linha.
fn is_well_formed(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn resolve(headers: &HeaderMap) -> RequestId {
    let from_client = headers
        .get(HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| is_well_formed(value));

    match from_client {
        Some(value) => RequestId(Arc::from(value)),
        // UUIDv7 em vez de v4 porque é ordenável por tempo, o que mantém os
        // logs agrupáveis.
        None => RequestId(Arc::from(uuid::Uuid::now_v7().to_string().as_str())),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CorrelationIdLayer;

impl<S> Layer<S> for CorrelationIdLayer {
    type Service = CorrelationId<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CorrelationId { inner }
    }
}

#[derive(Debug, Clone)]
pub struct CorrelationId<S> {
    inner: S,
}

impl<S> Service<Request> for CorrelationId<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let id = resolve(req.headers());

        // O id vai junto para o upstream e volta ao cliente: um só identificador
        // atravessa gateway, backend e log.
        id.apply_to(req.headers_mut());
        req.extensions_mut().insert(id.clone());

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            let mut response = inner.call(req).await?;
            id.apply_to(response.headers_mut());
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aceita_id_bem_formado_do_cliente() {
        assert!(is_well_formed("01924f8e-3c7a-7000-8000-abcdef123456"));
        assert!(is_well_formed("abc123"));
    }

    #[test]
    fn rejeita_id_que_permitiria_injecao_de_linha_no_log() {
        assert!(!is_well_formed("abc\ndef"));
        assert!(!is_well_formed("abc def"));
        assert!(!is_well_formed("abc\"def"));
        assert!(!is_well_formed(""));
        assert!(!is_well_formed(&"a".repeat(MAX_LEN + 1)));
    }

    #[test]
    fn id_malformado_e_substituido_por_um_gerado() {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER, HeaderValue::from_static("nao valido"));

        let id = resolve(&headers);
        assert_ne!(id.as_str(), "nao valido");
        assert!(is_well_formed(id.as_str()));
    }
}
