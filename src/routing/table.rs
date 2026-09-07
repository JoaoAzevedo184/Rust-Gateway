//! Casamento por prefixo mais longo (spec §6).

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{Config, normalize_prefix};
use crate::routing::runtime::{AuthPolicy, LimitSpec, RouteRuntime, UpstreamRuntime};

/// Snapshot imutável da configuração de roteamento. Substituível como um todo:
/// é o que torna hot reload e discovery dinâmico aditivos (D2).
#[derive(Debug, Default)]
pub struct RouterTable {
    /// Ordenadas por comprimento de prefixo decrescente: a primeira que casa é a
    /// mais longa, sem varrer o resto.
    routes: Vec<Arc<RouteRuntime>>,
    upstreams: HashMap<Arc<str>, Arc<UpstreamRuntime>>,
}

impl RouterTable {
    /// Constrói a partir de uma `Config` **já validada**.
    pub fn build(config: &Config) -> Self {
        let mut upstreams: HashMap<Arc<str>, Arc<UpstreamRuntime>> = HashMap::new();

        for (id, upstream) in &config.upstreams {
            let uri: http::Uri = match upstream.url.parse() {
                Ok(uri) => uri,
                // Inalcançável com config validada; ignorar mantém `build`
                // infalível sem esconder o problema, que já falhou no startup.
                Err(_) => continue,
            };
            let Some(authority) = uri.authority().cloned() else {
                continue;
            };
            let scheme = uri.scheme().cloned().unwrap_or(http::uri::Scheme::HTTP);

            let id: Arc<str> = Arc::from(id.as_str());
            upstreams.insert(
                id.clone(),
                Arc::new(UpstreamRuntime {
                    id: id.clone(),
                    scheme,
                    authority,
                    base_path: uri.path().trim_end_matches('/').to_string(),
                    resilience: crate::config::ResiliencePolicy::default()
                        .with(Some(&config.resilience.default))
                        .with(upstream.resilience.as_ref()),
                }),
            );
        }

        let mut routes: Vec<Arc<RouteRuntime>> = config
            .routes
            .iter()
            .filter_map(|route| {
                let upstream = upstreams.get(route.upstream.as_str())?.clone();
                let auth = route.auth.as_ref()?;

                Some(Arc::new(RouteRuntime {
                    id: Arc::from(route.id.as_str()),
                    prefix: normalize_prefix(&route.match_.prefix),
                    strip_prefix: route.strip_prefix,
                    upstream,
                    auth: AuthPolicy {
                        required: auth.required,
                        scopes: auth.scopes.clone(),
                    },
                    limits: route
                        .rate_limit
                        .iter()
                        .map(|limit| LimitSpec {
                            key: limit.key,
                            capacity: limit.capacity,
                            refill_per_sec: limit.refill_per_sec,
                        })
                        .collect(),
                    resilience: config.resolved_resilience(route),
                }))
            })
            .collect();

        routes.sort_by_key(|route| std::cmp::Reverse(route.prefix.len()));

        Self { routes, upstreams }
    }

    pub fn resolve(&self, path: &str) -> Option<&Arc<RouteRuntime>> {
        self.routes
            .iter()
            .find(|route| prefix_matches(&route.prefix, path))
    }

    pub fn routes(&self) -> &[Arc<RouteRuntime>] {
        &self.routes
    }

    pub fn upstream(&self, id: &str) -> Option<&Arc<UpstreamRuntime>> {
        self.upstreams.get(id)
    }
}

/// Casamento com **fronteira de segmento obrigatória**.
///
/// `/users` casa `/users` e `/users/42`, e não casa `/usersecret`. A ausência
/// dessa checagem é a diferença entre uma rota anônima e um vazamento: bastaria
/// nomear um recurso `/usersecret` para ele herdar a política de `/users`.
pub fn prefix_matches(prefix: &str, path: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }

    if !path.starts_with(prefix) {
        return false;
    }

    matches!(path.as_bytes().get(prefix.len()), None | Some(b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixo_casa_na_fronteira_de_segmento() {
        assert!(prefix_matches("/users", "/users"));
        assert!(prefix_matches("/users", "/users/"));
        assert!(prefix_matches("/users", "/users/42"));
        assert!(prefix_matches("/users", "/users/42/orders"));

        assert!(!prefix_matches("/users", "/usersecret"));
        assert!(!prefix_matches("/users", "/users42"));
        assert!(!prefix_matches("/users", "/user"));
        assert!(!prefix_matches("/users", "/"));
    }

    #[test]
    fn prefixo_raiz_casa_tudo() {
        assert!(prefix_matches("", "/"));
        assert!(prefix_matches("", "/qualquer/coisa"));
    }

    const CONFIG: &str = r#"
upstreams:
  user-service: { url: http://user-service:8080 }
routes:
  - id: users
    match: { prefix: /users }
    upstream: user-service
    auth: { required: false }
  - id: users-signup
    match: { prefix: /users/signup }
    upstream: user-service
    auth: { required: false }
"#;

    fn tabela() -> RouterTable {
        RouterTable::build(&Config::parse(CONFIG, "teste").unwrap())
    }

    #[test]
    fn prefixo_mais_longo_vence_independente_da_ordem_no_arquivo() {
        let table = tabela();

        assert_eq!(&*table.resolve("/users/signup").unwrap().id, "users-signup");
        assert_eq!(
            &*table.resolve("/users/signup/extra").unwrap().id,
            "users-signup"
        );
        assert_eq!(&*table.resolve("/users/42").unwrap().id, "users");
        assert_eq!(&*table.resolve("/users").unwrap().id, "users");
    }

    #[test]
    fn sem_casamento_nao_ha_rota() {
        assert!(tabela().resolve("/orders").is_none());
        assert!(tabela().resolve("/usersecret").is_none());
    }

    #[test]
    fn a_tabela_carrega_os_upstreams_resolvidos() {
        let table = tabela();
        let upstream = table.upstream("user-service").unwrap();

        assert_eq!(upstream.authority.as_str(), "user-service:8080");
        assert_eq!(upstream.base_path, "");
    }
}
