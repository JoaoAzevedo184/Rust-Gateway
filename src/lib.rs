//! Rust Gateway — ponto único de entrada para os serviços internos.
//!
//! A organização dos módulos segue a spec de design em
//! `docs/superpowers/specs/2026-09-06-rust-gateway-design.md`.

pub mod auth;
pub mod clock;
pub mod config;
pub mod error;
pub mod observability;
pub mod peer;
pub mod proxy;
pub mod ratelimit;
pub mod resilience;
pub mod routing;
pub mod server;
pub mod state;
pub mod testing;

use std::pin::Pin;

/// Requisição que atravessa a pilha de layers. O tipo é concreto de ponta a ponta:
/// todo layer recebe e devolve o mesmo par, o que mantém a stack componível sem
/// genéricos de corpo se propagando por toda a árvore.
pub type Request = http::Request<axum::body::Body>;
pub type Response = http::Response<axum::body::Body>;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
