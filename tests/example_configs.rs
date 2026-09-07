//! Os arquivos de configuração versionados em `config/` são exemplos lidos pela
//! documentação e pelo Compose. Este teste garante que eles continuam válidos
//! conforme o schema evolui — sem ele, um campo renomeado só seria descoberto
//! quando alguém tentasse de fato subir o Compose.

use rust_gateway::config::Config;

fn assert_valido(path: &str) {
    let yaml = std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{path}: {err}"));
    Config::parse(&yaml, path).unwrap_or_else(|err| panic!("{path} não valida:\n{err}"));
}

#[test]
fn gateway_yaml_e_valido() {
    assert_valido("config/gateway.yaml");
}

#[test]
fn gateway_auth_yaml_e_valido() {
    assert_valido("config/gateway.auth.yaml");
}

#[test]
fn gateway_example_yaml_e_valido() {
    assert_valido("config/gateway.example.yaml");
}
