//! Corpo de erro único do gateway (spec §12).
//!
//! Toda resposta de erro **gerada pelo gateway** usa este corpo, com o correlation
//! id dentro. Respostas produzidas pelo upstream são repassadas sem alteração.

use std::borrow::Cow;
use std::time::Duration;

use axum::body::Body;
use http::{HeaderValue, StatusCode, header};

use crate::Response;
use crate::observability::correlation::RequestId;

#[derive(Debug, Clone)]
pub struct GatewayError {
    pub status: StatusCode,
    /// Código estável, legível por máquina. Vai no campo `error` do corpo.
    pub code: &'static str,
    pub message: Cow<'static, str>,
    pub retry_after: Option<Duration>,
}

impl GatewayError {
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn with_retry_after(mut self, after: Duration) -> Self {
        self.retry_after = Some(after);
        self
    }

    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "No route matches this path",
        )
    }

    pub fn unauthorized(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn forbidden(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn payload_too_large(limit: u64) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("Request body exceeds the {limit} byte limit"),
        )
    }

    pub fn rate_limited(retry_after: Option<Duration>) -> Self {
        let mut err = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Request rate exceeded for this route",
        );
        err.retry_after = retry_after;
        err
    }

    pub fn bad_gateway(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "bad_gateway", message)
    }

    pub fn unavailable(code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code, message)
    }

    pub fn gateway_timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "Upstream did not respond in time",
        )
    }

    pub fn into_response(self, request_id: &str) -> Response {
        let body = serde_json::json!({
            "error": self.code,
            "message": self.message,
            "request_id": request_id,
        });

        let mut response = http::Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("corpo de erro é sempre construível");

        if let Some(after) = self.retry_after {
            // Retry-After em segundos, arredondado para cima: 0 convidaria o
            // cliente a repetir imediatamente.
            let secs = after.as_secs_f64().ceil().max(1.0) as u64;
            if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }

        response
    }

    /// Converte usando o correlation id já presente nas extensions da requisição.
    pub fn into_response_for(self, extensions: &http::Extensions) -> Response {
        let id = extensions
            .get::<RequestId>()
            .map(RequestId::as_str)
            .unwrap_or("-");
        let mut response = self.into_response(id);
        if let Some(request_id) = extensions.get::<RequestId>() {
            request_id.apply_to(response.headers_mut());
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn corpo_de_erro_carrega_codigo_mensagem_e_request_id() {
        let response = GatewayError::not_found().into_response("abc-123");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "not_found");
        assert_eq!(json["request_id"], "abc-123");
        assert!(json["message"].is_string());
    }

    #[tokio::test]
    async fn retry_after_arredonda_para_cima_e_nunca_e_zero() {
        let response =
            GatewayError::rate_limited(Some(Duration::from_millis(1))).into_response("id");
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");

        let response =
            GatewayError::rate_limited(Some(Duration::from_millis(2400))).into_response("id");
        assert_eq!(response.headers()[header::RETRY_AFTER], "3");
    }
}
