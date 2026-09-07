//! Scrub de headers de identidade (spec §7.1, D9).
//!
//! Global e **incondicional**, antes da resolução de rota. Se o scrub morasse no
//! layer de auth, uma rota anônima — que não roda auth — repassaria um
//! `X-User-Id` forjado pelo cliente direto ao backend.
//!
//! A regra resultante é auditável em uma frase: nenhum header de identidade
//! sobrevive à borda; os únicos que chegam ao upstream foram escritos pelo gateway.

use std::task::{Context, Poll};

use http::{HeaderMap, HeaderName};
use tower::{Layer, Service};

use crate::{BoxFuture, Request, Response};

const EXACT: [&str; 2] = ["x-user-id", "x-user-scopes"];
const PREFIX: &str = "x-auth-";

pub fn scrub(headers: &mut HeaderMap) {
    let doomed: Vec<HeaderName> = headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            EXACT.contains(&name) || name.starts_with(PREFIX)
        })
        .cloned()
        .collect();

    for name in doomed {
        // `remove` tira um valor por vez; um header repetido precisa do laço.
        while headers.remove(&name).is_some() {}
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityScrubLayer;

impl<S> Layer<S> for IdentityScrubLayer {
    type Service = IdentityScrub<S>;

    fn layer(&self, inner: S) -> Self::Service {
        IdentityScrub { inner }
    }
}

#[derive(Debug, Clone)]
pub struct IdentityScrub<S> {
    inner: S,
}

impl<S> Service<Request> for IdentityScrub<S>
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
        scrub(req.headers_mut());

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_headers_de_identidade_e_preserva_o_resto() {
        let mut headers = HeaderMap::new();
        headers.insert("x-user-id", "forjado".parse().unwrap());
        headers.insert("x-user-scopes", "admin".parse().unwrap());
        headers.insert("x-auth-method", "jwt".parse().unwrap());
        headers.insert("x-auth-qualquer-coisa", "1".parse().unwrap());
        headers.insert("authorization", "Bearer token".parse().unwrap());
        headers.insert("x-request-id", "abc".parse().unwrap());

        scrub(&mut headers);

        assert!(headers.get("x-user-id").is_none());
        assert!(headers.get("x-user-scopes").is_none());
        assert!(headers.get("x-auth-method").is_none());
        assert!(headers.get("x-auth-qualquer-coisa").is_none());
        assert_eq!(headers["authorization"], "Bearer token");
        assert_eq!(headers["x-request-id"], "abc");
    }

    #[test]
    fn remove_todas_as_ocorrencias_de_um_header_repetido() {
        let mut headers = HeaderMap::new();
        headers.append("x-user-id", "um".parse().unwrap());
        headers.append("x-user-id", "dois".parse().unwrap());

        scrub(&mut headers);

        assert_eq!(headers.get_all("x-user-id").iter().count(), 0);
    }
}
