//! `POST /mcp` — a minimal Model Context Protocol server over the inbox
//! API (streamable-HTTP transport, JSON-RPC 2.0), so agent clients —
//! Claude Code, Claude Desktop, anything MCP-speaking — can review the
//! queue with the same verbs and the same authorization as the web inbox.
//!
//! Deliberately small: `initialize`, `ping`, `tools/list`, `tools/call`,
//! and 202-for-notifications is the whole protocol surface; there is no
//! SSE stream (GET answers 405) and no sessions — every call carries the
//! bearer token through the shared protected-router middleware, and each
//! tool delegates to the SAME handler/action code paths as the REST
//! routes, so MCP can do nothing the inbox API cannot.

use super::{actions, parse_report_id, ApiError};
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// Latest revision this implementation tracks. Version negotiation is
/// trivial by design: the surface below is a strict subset of every
/// published revision, so we always answer with ours.
const PROTOCOL_VERSION: &str = "2025-06-18";

const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
const JSONRPC_INVALID_PARAMS: i64 = -32602;

pub async fn rpc(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    // Notifications (no id) expect no response body: 202, per the
    // streamable-HTTP transport. `notifications/initialized` lands here.
    let Some(id) = body.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = body.get("params").cloned().unwrap_or(Value::Null);

    let outcome = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "merge0",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_catalog() })),
        "tools/call" => call_tool(&state, &params).await,
        other => Err((
            JSONRPC_METHOD_NOT_FOUND,
            format!("method {other:?} not found"),
        )),
    };

    let response = match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    };
    Json(response).into_response()
}

/// The catalog mirrors the inbox API one-to-one; descriptions are written
/// for the model that will read them.
fn tool_catalog() -> Value {
    json!([
        {
            "name": "list_reports",
            "description": "List triage reports, optionally filtered by lifecycle status (pending, awaiting_review, dispatched, pr_open, completed, dismissed, handed_off).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "Lifecycle status filter; omit for all reports." }
                }
            }
        },
        {
            "name": "get_report",
            "description": "Everything the loop knows about one report: the report, gate decision and its replayable context, work order, dispatch trail, outcomes, owner routing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Report ULID." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "approve_report",
            "description": "Approve an awaiting_review report: dispatches its Work Order to the configured agent runner (or files a tracker story, per the install's delivery mode). This spends customer-side compute — only call it when the human you are working for has decided.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Report ULID." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "dismiss_report",
            "description": "Dismiss a report with a reason. Reasons feed gate precision, so pick the honest one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Report ULID." },
                    "reason": {
                        "type": "string",
                        "enum": ["intended_behavior", "wont_fix", "duplicate", "bad_evidence"]
                    }
                },
                "required": ["id", "reason"]
            }
        },
        {
            "name": "get_telemetry",
            "description": "Loop health snapshot: merge rate, runner yield, gate precision, fix efficacy, token spend.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window_days": { "type": "integer", "description": "Rolling window, default 30, max 365." }
                }
            }
        }
    ])
}

/// Run one tool. Unknown tool = protocol error; a tool that ran and
/// failed (bad id, wrong status) = a successful `tools/call` whose result
/// carries `isError: true`, per spec — the calling model is the one who
/// needs to read the failure, not the transport.
async fn call_tool(state: &AppState, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((JSONRPC_INVALID_PARAMS, "missing tool name".to_string()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let result = match name {
        "list_reports" => {
            let status = args
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string);
            super::reports::list(
                State(state.clone()),
                Query(super::reports::ListParams { status }),
            )
            .await
            .map(|Json(v)| v)
        }
        "get_report" => match required_id_string(&args) {
            Ok(id) => super::reports::detail(State(state.clone()), Path(id))
                .await
                .map(|Json(v)| v),
            Err(e) => Err(e),
        },
        "approve_report" => match parse_id(&args) {
            Ok(id) => actions::approve(state, id, actions::DispatchedBy::Mcp).await,
            Err(e) => Err(e),
        },
        "dismiss_report" => match parse_dismiss_args(&args) {
            Ok((id, reason)) => actions::dismiss(state, id, reason).await,
            Err(e) => Err(e),
        },
        "get_telemetry" => {
            let window_days = args
                .get("window_days")
                .and_then(Value::as_u64)
                .map(|n| n as u32);
            super::telemetry::snapshot(
                State(state.clone()),
                Query(super::telemetry::Params { window_days }),
            )
            .await
            .map(|Json(v)| v)
        }
        other => return Err((JSONRPC_INVALID_PARAMS, format!("unknown tool {other:?}"))),
    };

    Ok(match result {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "isError": false,
        }),
        Err(ApiError::Status(status, message)) => json!({
            "content": [{ "type": "text", "text": format!("{status}: {message}") }],
            "isError": true,
        }),
    })
}

fn required_id_string(args: &Value) -> Result<String, ApiError> {
    args.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ApiError::bad_request("missing required argument: id"))
}

fn parse_id(args: &Value) -> Result<ulid::Ulid, ApiError> {
    parse_report_id(&required_id_string(args)?)
}

fn parse_dismiss_args(
    args: &Value,
) -> Result<(ulid::Ulid, merge0_signal::DismissReason), ApiError> {
    let id = parse_id(args)?;
    let reason = args
        .get("reason")
        .cloned()
        .ok_or_else(|| ApiError::bad_request("missing required argument: reason"))?;
    let reason = serde_json::from_value(reason)
        .map_err(|_| ApiError::bad_request("unknown dismiss reason"))?;
    Ok((id, reason))
}
