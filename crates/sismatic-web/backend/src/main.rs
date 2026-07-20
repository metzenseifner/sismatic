//! HTTP front-end over [`sismatic_core`].
//!
//! Serves a device pool loaded from a `devices.toml` and exposes it over a small
//! JSON API. Like the CLI, this is a thin adapter: the protocol and connection
//! logic all live in the core crate; the handlers only translate HTTP requests
//! into instructions and decoded values back into JSON.
//!
//! Configuration is via environment variables:
//! - `SISMATIC_CONFIG` — path to the devices file (default `devices.toml`).
//! - `SISMATIC_ADDR`   — socket address to bind (default `0.0.0.0:3000`).

use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;

use sismatic_core::devices::registry::{Registry, Target};
use sismatic_core::devices::sis_keepalive::SisKeepalive;
use sismatic_core::devices::transport::ssh::RusshConnector;
use sismatic_core::protocol::Value;
use sismatic_core::protocol::instructions::Instruction;
use sismatic_core::protocol::instructions::commands::Command as SisCommand;
use sismatic_core::protocol::instructions::query::Query;
use sismatic_core::protocol::instructions::register::Register;

type AppState = Arc<Registry>;

#[tokio::main]
async fn main() -> Result<()> {
    let config = std::env::var("SISMATIC_CONFIG").unwrap_or_else(|_| "devices.toml".into());
    let addr = std::env::var("SISMATIC_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".into());

    let resolved = sismatic_core::devices::config::load(&config)
        .with_context(|| format!("loading {config}"))?;
    let registry = Registry::build(resolved.devices, resolved.groups, Arc::new(RusshConnector));

    // Eagerly connect and keep warm any device the config marks `eager`. The
    // guard lives until `main` returns (i.e. for the server's lifetime); on
    // shutdown its Drop aborts the SIS keepalive tasks.
    let _sis_keepalive =
        SisKeepalive::spawn(&tokio::runtime::Handle::current(), registry.devices());

    let state: AppState = Arc::new(registry);

    let app = Router::new()
        .route("/health", get(health))
        .route("/devices", get(list_devices))
        .route("/groups", get(list_groups))
        .route("/devices/{id}/query/{name}", get(query))
        .route("/devices/{id}/command/{name}", post(command))
        .route("/devices/{id}/register/{name}", post(register))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    eprintln!("sismatic-web listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn list_devices(State(registry): State<AppState>) -> Json<Vec<String>> {
    let mut ids = registry.ids();
    ids.sort();
    Json(ids)
}

async fn list_groups(State(registry): State<AppState>) -> Json<Vec<String>> {
    let mut ids = registry.group_ids();
    ids.sort();
    Json(ids)
}

async fn query(
    State(registry): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let instruction = Query::from_str(&name)
        .map_err(|e| AppError::BadInstruction(e.to_string()))?
        .instruction();
    run(&registry, &id, &name, instruction).await
}

async fn command(
    State(registry): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    let instruction = SisCommand::from_str(&name)
        .map_err(|e| AppError::BadInstruction(e.to_string()))?
        .instruction();
    run(&registry, &id, &name, instruction).await
}

async fn register(
    State(registry): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    value: String,
) -> Result<Json<serde_json::Value>, AppError> {
    let instruction = Register::from_str(&name)
        .map_err(|e| AppError::BadInstruction(e.to_string()))?
        .instruction(&value);
    run(&registry, &id, &name, instruction).await
}

/// Resolve `id` to a device or group, run one instruction, and render the
/// result as JSON. A device yields a single `value`; a group yields a
/// `results` object mapping each member's device id to its value, so the
/// caller sees the whole fan-out.
async fn run(
    registry: &Registry,
    id: &str,
    name: &str,
    instruction: Instruction,
) -> Result<Json<serde_json::Value>, AppError> {
    match registry
        .target(id)
        .ok_or_else(|| AppError::UnknownDevice(id.to_string()))?
    {
        Target::Device(device) => {
            let value = device
                .run(&instruction)
                .await
                .map_err(|e| AppError::Device(e.to_string()))?;
            Ok(Json(json!({
                "device": id,
                "name": name,
                "value": value_to_json(value),
            })))
        }
        Target::Group(group) => {
            let results = group
                .run(&instruction)
                .await
                .map_err(|e| AppError::Device(e.to_string()))?;
            let results: serde_json::Map<String, serde_json::Value> = results
                .into_iter()
                .map(|(member, value)| (member, value_to_json(value)))
                .collect();
            Ok(Json(json!({
                "group": id,
                "name": name,
                "results": results,
            })))
        }
    }
}

/// Map a decoded [`Value`] onto its natural JSON type, mirroring the Python
/// facade: ports/numbers become integers, flags become booleans, everything
/// else falls back to its string rendering.
fn value_to_json(value: Value) -> serde_json::Value {
    match value {
        Value::Port(p) => json!(p),
        Value::Number(n) => json!(n),
        Value::Flag(b) => json!(b),
        other => json!(other.to_string()),
    }
}

/// A failed request, rendered as a JSON `{ "error": ... }` body with a fitting
/// status code.
enum AppError {
    UnknownDevice(String),
    BadInstruction(String),
    Device(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::UnknownDevice(id) => {
                (StatusCode::NOT_FOUND, format!("unknown device '{id}'"))
            }
            AppError::BadInstruction(e) => (StatusCode::BAD_REQUEST, e),
            AppError::Device(e) => (StatusCode::BAD_GATEWAY, e),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
