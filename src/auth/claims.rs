//! Validação de claims e leitura de scopes (spec §8.2, §8.3).

use std::collections::BTreeSet;
use std::time::Duration;

use jsonwebtoken::{Algorithm, Validation};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub sub: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// Monta a validação para **um** algoritmo: o escolhido a partir da chave da JWKS.
///
/// O `alg` do header do token não decide nada, e é exatamente esse o ponto: aceitar
/// o algoritmo anunciado pelo token é a vulnerabilidade clássica de JWT, com `none`
/// como caso extremo.
pub fn validation(alg: Algorithm, issuer: &str, audience: &str, leeway: Duration) -> Validation {
    let mut validation = Validation::new(alg);
    validation.algorithms = vec![alg];
    validation.leeway = leeway.as_secs();
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    validation
}

/// Lê a claim de escopo no formato OAuth2 — string separada por espaço — e, quando
/// presente, também `roles` como array. A união das duas é o conjunto concedido.
pub fn granted_scopes(claims: &Claims, scope_claim: &str) -> Vec<String> {
    let mut scopes: BTreeSet<String> = BTreeSet::new();

    if let Some(value) = claims.extra.get(scope_claim) {
        collect_scope_value(value, &mut scopes);
    }

    if let Some(Value::Array(roles)) = claims.extra.get("roles") {
        for role in roles {
            if let Value::String(role) = role {
                scopes.insert(role.clone());
            }
        }
    }

    scopes.into_iter().collect()
}

fn collect_scope_value(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(raw) => out.extend(raw.split_whitespace().map(str::to_string)),
        Value::Array(items) => {
            for item in items {
                if let Value::String(raw) = item {
                    out.insert(raw.clone());
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(json: serde_json::Value) -> Claims {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn scope_no_formato_oauth2_e_separado_por_espaco() {
        let claims = claims(serde_json::json!({ "sub": "u1", "scope": "user.read user.write" }));
        assert_eq!(
            granted_scopes(&claims, "scope"),
            vec!["user.read", "user.write"]
        );
    }

    #[test]
    fn roles_em_array_se_somam_aos_scopes() {
        let claims = claims(serde_json::json!({
            "sub": "u1",
            "scope": "user.read",
            "roles": ["admin", "auditor"],
        }));
        assert_eq!(
            granted_scopes(&claims, "scope"),
            vec!["admin", "auditor", "user.read"]
        );
    }

    #[test]
    fn claim_de_escopo_configuravel() {
        let claims = claims(serde_json::json!({ "sub": "u1", "permissions": "a b" }));
        assert_eq!(granted_scopes(&claims, "permissions"), vec!["a", "b"]);
        assert!(granted_scopes(&claims, "scope").is_empty());
    }

    #[test]
    fn claim_de_escopo_com_tipo_inesperado_nao_derruba_a_validacao() {
        let claims = claims(serde_json::json!({ "sub": "u1", "scope": 42 }));
        assert!(granted_scopes(&claims, "scope").is_empty());
    }

    #[test]
    fn a_validacao_fixa_um_unico_algoritmo() {
        let validation = validation(Algorithm::RS256, "iss", "aud", Duration::from_secs(30));

        assert_eq!(validation.algorithms, vec![Algorithm::RS256]);
        assert_eq!(validation.leeway, 30);
        assert!(validation.validate_exp && validation.validate_nbf);
    }
}
