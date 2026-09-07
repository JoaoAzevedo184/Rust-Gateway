//! Timeouts (spec §10.3).
//!
//! Três níveis, e confundi-los é como se produz um gateway que trava:
//!
//! | Nível | Escopo |
//! |---|---|
//! | `connect_timeout` | estabelecer a conexão TCP, aplicado no conector |
//! | `upstream_timeout` | uma tentativa |
//! | `request_timeout` | a requisição inteira, incluindo todos os retries |

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::time::{Instant, Sleep};
use tower::{Layer, Service};

use crate::error::GatewayError;
use crate::observability::metrics::{self, Outcome};
use crate::resilience::AttemptOutcome;
use crate::routing::layer::route_of;
use crate::{BoxFuture, Request, Response};

/// Timeout de **uma tentativa**, o mais interno da pilha.
///
/// Cobre a espera pelos headers da resposta e, depois deles, a inatividade do
/// corpo. Não é um teto sobre a duração total do download: o gateway transmite
/// corpos em streaming, e um teto absoluto mataria toda transferência longa —
/// que é justamente o caso que o streaming existe para atender.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpstreamTimeoutLayer;

impl<S> Layer<S> for UpstreamTimeoutLayer {
    type Service = UpstreamTimeout<S>;

    fn layer(&self, inner: S) -> Self::Service {
        UpstreamTimeout { inner }
    }
}

#[derive(Debug, Clone)]
pub struct UpstreamTimeout<S> {
    inner: S,
}

impl<S> Service<Request> for UpstreamTimeout<S>
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
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        let Some(route) = route_of(req.extensions()) else {
            return Box::pin(async move { inner.call(req).await });
        };

        let budget = route.resilience.upstream_timeout;
        let extensions = req.extensions().clone();

        Box::pin(async move {
            match tokio::time::timeout(budget, inner.call(req)).await {
                Ok(result) => {
                    let (parts, body) = result?.into_parts();
                    Ok(http::Response::from_parts(
                        parts,
                        Body::new(IdleTimeoutBody::new(body, budget)),
                    ))
                }
                Err(_) => {
                    tracing::warn!(?budget, "tentativa estourou o upstream_timeout");
                    metrics::mark(&extensions, Outcome::Timeout);

                    let mut response =
                        GatewayError::gateway_timeout().into_response_for(&extensions);
                    AttemptOutcome::Timeout.mark(&mut response);
                    Ok(response)
                }
            }
        })
    }
}

/// Teto absoluto da requisição, incluindo todos os retries.
///
/// A validação de startup recusa `request_timeout` menor que `upstream_timeout`,
/// porque o teto global cortaria antes da tentativa individual — o que é sempre
/// engano.
#[derive(Debug, Clone, Copy)]
pub struct RequestTimeoutLayer {
    budget: Duration,
}

impl RequestTimeoutLayer {
    pub fn new(budget: Duration) -> Self {
        Self { budget }
    }
}

impl<S> Layer<S> for RequestTimeoutLayer {
    type Service = RequestTimeout<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestTimeout {
            inner,
            budget: self.budget,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RequestTimeout<S> {
    inner: S,
    budget: Duration,
}

impl<S> Service<Request> for RequestTimeout<S>
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
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        let budget = self.budget;
        let extensions = req.extensions().clone();

        Box::pin(async move {
            match tokio::time::timeout(budget, inner.call(req)).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(?budget, "requisição estourou o request_timeout");
                    metrics::mark(&extensions, Outcome::Timeout);
                    Ok(GatewayError::gateway_timeout().into_response_for(&extensions))
                }
            }
        })
    }
}

/// Corpo que falha quando o upstream para de enviar.
///
/// O prazo é reiniciado a cada quadro recebido, então uma transferência longa mas
/// progredindo nunca é cortada; uma que estagnou, sim. Depois dos headers já não
/// há como responder 504 — a resposta começou —, então o sintoma é a conexão
/// terminando com erro, que é o que o cliente precisa distinguir de um fim normal.
struct IdleTimeoutBody {
    inner: Body,
    idle: Duration,
    sleep: Pin<Box<Sleep>>,
}

impl IdleTimeoutBody {
    fn new(inner: Body, idle: Duration) -> Self {
        Self {
            inner,
            idle,
            sleep: Box::pin(tokio::time::sleep(idle)),
        }
    }
}

impl HttpBody for IdleTimeoutBody {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();

        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(frame) => {
                this.sleep.as_mut().reset(Instant::now() + this.idle);
                Poll::Ready(frame.map(|result| result.map_err(Into::into)))
            }
            Poll::Pending => match this.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Some(Err(format!(
                    "upstream parou de enviar o corpo por mais de {:?}",
                    this.idle
                )
                .into()))),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
