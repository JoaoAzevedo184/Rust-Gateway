//! Layer de autenticação (spec §8).

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http::HeaderValue;
use http::header::AUTHORIZATION;
use tower::{Layer, Service};

use crate::auth::claims::{Claims, granted_scopes, validation};
use crate::auth::jwks::JwksCache;
use crate::auth::{AuthFailure, Identity};
use crate::error::GatewayError;
use crate::observability::metrics::{self, Outcome};
use crate::observability::tracing::record_sub;
use crate::routing::layer::route_of;
use crate::{BoxFuture, Request, Response};

#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub issuer: String,
    pub audience: String,
    pub leeway: Duration,
    pub scope_claim: String,
}

pub struct Authenticator {
    jwks: Arc<JwksCache>,
    settings: AuthSettings,
}

impl Authenticator {
    pub fn new(jwks: Arc<JwksCache>, settings: AuthSettings) -> Self {
        Self { jwks, settings }
    }

    pub fn jwks(&self) -> &Arc<JwksCache> {
        &self.jwks
    }

    /// Validação na ordem da spec §8.2: algoritmo, assinatura, tempo, emissor e
    /// audiência. Os scopes são conferidos pelo layer, que conhece a rota.
    pub async fn authenticate(&self, token: &str) -> Result<Identity, AuthFailure> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|_| AuthFailure::Malformed("header ilegível"))?;

        let kid = header.kid.ok_or(AuthFailure::Malformed("token sem kid"))?;

        let key = match self.jwks.key_for(&kid).await {
            Some(key) => key,
            None if !self.jwks.is_usable() => return Err(AuthFailure::JwksUnavailable),
            None => return Err(AuthFailure::UnknownKid),
        };

        // O algoritmo vem da chave, não do token.
        let validation = validation(
            key.alg,
            &self.settings.issuer,
            &self.settings.audience,
            self.settings.leeway,
        );

        let data = jsonwebtoken::decode::<Claims>(token, &key.key, &validation)
            .map_err(|err| AuthFailure::Invalid(err.to_string()))?;

        let sub = data.claims.sub.clone().ok_or(AuthFailure::MissingSubject)?;
        if sub.is_empty() {
            return Err(AuthFailure::MissingSubject);
        }

        Ok(Identity {
            sub: Arc::from(sub.as_str()),
            scopes: granted_scopes(&data.claims, &self.settings.scope_claim),
        })
    }
}

impl std::fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticator")
            .field("settings", &self.settings)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct AuthLayer {
    authenticator: Option<Arc<Authenticator>>,
}

impl AuthLayer {
    /// `None` quando não há seção `auth` configurada. A validação de startup já
    /// garantiu que, nesse caso, nenhuma rota exige autenticação.
    pub fn new(authenticator: Option<Arc<Authenticator>>) -> Self {
        Self { authenticator }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            authenticator: self.authenticator.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuthService<S> {
    inner: S,
    authenticator: Option<Arc<Authenticator>>,
}

impl<S> Service<Request> for AuthService<S>
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
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        // Rota não resolvida, ou rota sem política de auth e sem token: nada a fazer.
        let Some(route) = route_of(req.extensions()).cloned() else {
            return Box::pin(async move { inner.call(req).await });
        };

        let token = bearer_token(req.headers());
        let authenticator = self.authenticator.clone();

        Box::pin(async move {
            let outcome = match (token, &authenticator) {
                (None, _) if !route.auth.required => None,
                (None, _) => Some(Err(AuthFailure::Missing)),
                (Some(_), None) => Some(Err(AuthFailure::JwksUnavailable)),
                (Some(token), Some(authenticator)) => {
                    Some(authenticator.authenticate(&token).await)
                }
            };

            let identity = match outcome {
                // Rota anônima sem token: segue sem identidade.
                None => None,

                Some(Ok(identity)) => Some(identity),

                // Um token presente e inválido é sempre rejeitado, mesmo em rota
                // anônima: a rota dispensa credencial, não perdoa credencial ruim.
                Some(Err(failure)) => {
                    let error = match &failure {
                        AuthFailure::JwksUnavailable => {
                            metrics::mark(req.extensions(), Outcome::RejectedAuth);
                            GatewayError::unavailable(
                                "jwks_unavailable",
                                "Authentication keys are unavailable",
                            )
                        }
                        other => {
                            metrics::mark(req.extensions(), Outcome::RejectedAuth);
                            tracing::debug!(reason = other.reason(), "autenticação recusada");
                            GatewayError::unauthorized("Missing or invalid access token")
                        }
                    };
                    return Ok(error.into_response_for(req.extensions()));
                }
            };

            if let Some(identity) = identity {
                if !route.auth.scopes_satisfied_by(&identity.scopes) {
                    let missing = route.auth.missing_scopes(&identity.scopes);
                    tracing::debug!(sub = %identity.sub, ?missing, "escopo insuficiente");
                    metrics::mark(req.extensions(), Outcome::RejectedAuth);
                    return Ok(GatewayError::forbidden(
                        "Token lacks the scopes required by this route",
                    )
                    .into_response_for(req.extensions()));
                }

                record_sub(&identity.sub);
                inject_identity_headers(&mut req, &identity);
                req.extensions_mut().insert(identity);
            }

            inner.call(req).await
        })
    }
}

fn bearer_token(headers: &http::HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;

    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// Headers de identidade escritos pelo gateway.
///
/// Como o scrub global já rodou antes da resolução de rota, estes headers são
/// inequivocamente do gateway. O `Authorization` original segue intacto, para o
/// backend que quiser reexaminar o token.
fn inject_identity_headers(req: &mut Request, identity: &Identity) {
    let headers = req.headers_mut();

    if let Ok(value) = HeaderValue::from_str(&identity.sub) {
        headers.insert("x-user-id", value);
    }
    if let Ok(value) = HeaderValue::from_str(&identity.scopes.join(" ")) {
        headers.insert("x-user-scopes", value);
    }
    headers.insert("x-auth-method", HeaderValue::from_static("jwt"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    fn headers_with_auth(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, value.parse().unwrap());
        headers
    }

    #[test]
    fn le_o_token_bearer_ignorando_caixa_do_esquema() {
        assert_eq!(
            bearer_token(&headers_with_auth("Bearer abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(
            bearer_token(&headers_with_auth("bearer abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(
            bearer_token(&headers_with_auth("BEARER abc")).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn ignora_esquemas_que_nao_sao_bearer_e_valores_vazios() {
        assert!(bearer_token(&headers_with_auth("Basic dXNlcjpwdw==")).is_none());
        assert!(bearer_token(&headers_with_auth("Bearer ")).is_none());
        assert!(bearer_token(&headers_with_auth("abc")).is_none());
        assert!(bearer_token(&HeaderMap::new()).is_none());
    }
}
