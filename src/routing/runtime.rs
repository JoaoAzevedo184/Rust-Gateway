//! Política de rota já resolvida (spec §6).
//!
//! `RouteRuntime` é o resultado da resolução: política de auth, lista de limites,
//! política de resiliência já herdada, e o `route_id` usado como label de métrica.
//! Os layers de política leem daqui em vez de reinterpretar a configuração crua.

use std::sync::Arc;

use http::uri::{Authority, Scheme};

use crate::config::{KeyKind, ResiliencePolicy};

#[derive(Debug)]
pub struct RouteRuntime {
    pub id: Arc<str>,
    /// Prefixo normalizado, sem barra final. A raiz é a string vazia.
    pub prefix: String,
    pub strip_prefix: bool,
    pub upstream: Arc<UpstreamRuntime>,
    pub auth: AuthPolicy,
    pub limits: Vec<LimitSpec>,
    pub resilience: ResiliencePolicy,
}

#[derive(Debug)]
pub struct UpstreamRuntime {
    pub id: Arc<str>,
    pub scheme: Scheme,
    pub authority: Authority,
    /// Prefixo de caminho da URL do upstream, sem barra final.
    pub base_path: String,
    pub resilience: ResiliencePolicy,
}

#[derive(Debug, Clone, Default)]
pub struct AuthPolicy {
    pub required: bool,
    pub scopes: Vec<String>,
}

impl AuthPolicy {
    /// A rota exige **todos** os scopes listados — conjunção. Menos surpreendente
    /// do que disjunção quando alguém adiciona um scope achando que restringe.
    pub fn scopes_satisfied_by(&self, granted: &[String]) -> bool {
        self.scopes
            .iter()
            .all(|needed| granted.iter().any(|g| g == needed))
    }

    pub fn missing_scopes(&self, granted: &[String]) -> Vec<String> {
        self.scopes
            .iter()
            .filter(|needed| !granted.iter().any(|g| &g == needed))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LimitSpec {
    pub key: KeyKind,
    pub capacity: u32,
    pub refill_per_sec: f64,
}

impl RouteRuntime {
    /// Caminho a enviar ao upstream, já com `strip_prefix` e o base path aplicados.
    /// A query string é tratada à parte, e sempre preservada intacta.
    pub fn upstream_path(&self, path: &str) -> String {
        let remainder = if self.strip_prefix && !self.prefix.is_empty() {
            path.strip_prefix(self.prefix.as_str()).unwrap_or(path)
        } else {
            path
        };

        let remainder = if remainder.is_empty() { "/" } else { remainder };
        let base = self.upstream.base_path.as_str();

        if base.is_empty() {
            remainder.to_string()
        } else {
            format!("{base}{remainder}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(base_path: &str) -> Arc<UpstreamRuntime> {
        Arc::new(UpstreamRuntime {
            id: "svc".into(),
            scheme: Scheme::HTTP,
            authority: "svc:8080".parse().unwrap(),
            base_path: base_path.to_string(),
            resilience: ResiliencePolicy::default(),
        })
    }

    fn route(prefix: &str, strip: bool, base_path: &str) -> RouteRuntime {
        RouteRuntime {
            id: "r".into(),
            prefix: prefix.to_string(),
            strip_prefix: strip,
            upstream: upstream(base_path),
            auth: AuthPolicy::default(),
            limits: Vec::new(),
            resilience: ResiliencePolicy::default(),
        }
    }

    #[test]
    fn sem_strip_prefix_o_caminho_segue_inteiro() {
        assert_eq!(
            route("/users", false, "").upstream_path("/users/42"),
            "/users/42"
        );
    }

    #[test]
    fn com_strip_prefix_o_prefixo_some() {
        assert_eq!(
            route("/orders", true, "").upstream_path("/orders/42"),
            "/42"
        );
    }

    #[test]
    fn strip_prefix_exato_vira_raiz_e_nao_caminho_vazio() {
        assert_eq!(route("/orders", true, "").upstream_path("/orders"), "/");
    }

    #[test]
    fn base_path_do_upstream_e_prefixado() {
        assert_eq!(
            route("/orders", true, "/api/v1").upstream_path("/orders/42"),
            "/api/v1/42"
        );
    }

    #[test]
    fn scopes_sao_conjuncao() {
        let policy = AuthPolicy {
            required: true,
            scopes: vec!["user.read".into(), "user.write".into()],
        };

        assert!(!policy.scopes_satisfied_by(&["user.read".into()]));
        assert!(policy.scopes_satisfied_by(&[
            "user.read".into(),
            "user.write".into(),
            "extra".into()
        ]));
        assert_eq!(
            policy.missing_scopes(&["user.read".into()]),
            vec!["user.write".to_string()]
        );
    }
}
