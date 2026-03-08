// Re-export all modules for integration test visibility.
pub mod auth;
pub mod cache;
pub mod clients;
pub mod config;
pub mod content;
pub mod db;
pub mod extractors;
pub mod grpc;
pub mod handlers;
pub mod health;
pub mod logging;
pub mod middleware;
pub mod openapi;
pub mod proto;
pub mod realtime;
pub mod repositories;
pub mod server;
pub mod services;
pub mod shutdown;
pub mod state;
pub mod tasks;

#[cfg(test)]
pub mod test_containers;
