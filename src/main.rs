//! Bootstrap do processo.

use std::process::ExitCode;

const DEFAULT_CONFIG: &str = "config/gateway.yaml";

#[tokio::main]
async fn main() -> ExitCode {
    let config_path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("GATEWAY_CONFIG").ok())
        .unwrap_or_else(|| DEFAULT_CONFIG.to_string());

    match rust_gateway::server::run(&config_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // A configuração inválida chega aqui com a lista inteira de problemas.
            tracing::error!(config = %config_path, "falha ao subir o gateway:\n{err}");
            ExitCode::FAILURE
        }
    }
}
