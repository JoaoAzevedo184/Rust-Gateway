//! Limite de corpo de requisição, global (`server.max_body_bytes`).
//!
//! Duas checagens, porque uma só não cobre os dois casos: um `Content-Length`
//! declarado acima do limite é recusado antes de ler um byte, e um corpo em
//! chunks — que não declara tamanho — é cortado durante o streaming.

use std::task::{Context, Poll};

use axum::body::Body;
use http_body_util::Limited;
use tower::{Layer, Service};

use crate::error::GatewayError;
use crate::{BoxFuture, Request, Response};

#[derive(Debug, Clone, Copy)]
pub struct BodyLimitLayer {
    max_bytes: u64,
}

impl BodyLimitLayer {
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

impl<S> Layer<S> for BodyLimitLayer {
    type Service = BodyLimit<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BodyLimit {
            inner,
            max_bytes: self.max_bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BodyLimit<S> {
    inner: S,
    max_bytes: u64,
}

impl<S> Service<Request> for BodyLimit<S>
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

    fn call(&mut self, req: Request) -> Self::Future {
        let max_bytes = self.max_bytes;

        if declared_length(&req).is_some_and(|len| len > max_bytes) {
            let response =
                GatewayError::payload_too_large(max_bytes).into_response_for(req.extensions());
            return Box::pin(async move { Ok(response) });
        }

        let (parts, body) = req.into_parts();
        let limited = Body::new(Limited::new(body, max_bytes as usize));
        let req = Request::from_parts(parts, limited);

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}

fn declared_length(req: &Request) -> Option<u64> {
    req.headers()
        .get(http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use tower::ServiceExt;

    fn stub() -> crate::testing::EchoService {
        crate::testing::EchoService
    }

    #[tokio::test]
    async fn content_length_acima_do_limite_e_recusado_antes_de_ler_o_corpo() {
        let service = BodyLimitLayer::new(1024).layer(stub());

        let req = http::Request::builder()
            .header(http::header::CONTENT_LENGTH, "2048")
            .body(Body::from("x"))
            .unwrap();

        let response = service.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn corpo_dentro_do_limite_passa() {
        let service = BodyLimitLayer::new(1024).layer(stub());

        let req = http::Request::builder()
            .header(http::header::CONTENT_LENGTH, "1")
            .body(Body::from("x"))
            .unwrap();

        let response = service.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
