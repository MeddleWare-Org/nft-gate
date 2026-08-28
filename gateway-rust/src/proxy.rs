//! Reverse-proxy an authorised request to the configured upstream. Body is capped at
//! `max_body_bytes` (a griefing guard) before it is read.
//!
//! Headers stripped from the **forwarded request**: `Host`, `Authorization`,
//! `X-Access-Proof`, and `Content-Length` (recomputed by the HTTP client).
//!
//! Headers stripped from the **upstream response**: `Content-Length`, `Transfer-Encoding`,
//! and `Connection` (hop-by-hop; recomputed / not safe to forward).

use crate::AppState;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Build a JSON `{"error": reason}` response with the given status code.
fn error(status: StatusCode, reason: &str) -> Response {
    (status, Json(json!({ "error": reason }))).into_response()
}

/// Forward `req` to the upstream configured in `app.cfg.upstream_url`.
///
/// Caps the request body at `app.cfg.max_body_bytes` before reading. Strips gateway-specific
/// headers (`Host`, `Authorization`, `X-Access-Proof`, `Content-Length`) before sending.
/// Returns a 413 if the body exceeds the cap.
///
/// # Errors
///
/// Returns 502 if the upstream request fails (connection error, timeout, etc.).
pub async fn forward(app: &AppState, req: Request) -> Response {
    let (parts, body) = req.into_parts();

    let bytes = match axum::body::to_bytes(body, app.cfg.max_body_bytes).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| parts.uri.path());
    let url = format!("{}{}", app.cfg.upstream_url, path_and_query);

    let mut headers = parts.headers.clone();
    headers.remove(header::HOST);
    headers.remove(header::AUTHORIZATION);
    headers.remove(header::CONTENT_LENGTH);
    headers.remove("x-access-proof");

    match app.http.forward(parts.method, &url, headers, bytes).await {
        Ok(resp) => {
            let mut builder = Response::builder().status(resp.status);
            for (name, value) in &resp.headers {
                if name == header::CONTENT_LENGTH
                    || name == header::TRANSFER_ENCODING
                    || name == header::CONNECTION
                {
                    continue; // recomputed by the server / not hop-safe
                }
                builder = builder.header(name, value);
            }
            builder
                .body(Body::from(resp.body))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(e) => {
            tracing::warn!(error = %e, "upstream request failed");
            error(StatusCode::BAD_GATEWAY, "upstream error")
        }
    }
}
