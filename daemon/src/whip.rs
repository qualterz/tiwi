use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum WhipCommand {
    Offer {
        sdp: String,
        reply: oneshot::Sender<Result<WhipAccepted, WhipError>>,
    },
    Delete {
        session_id: String,
        reply: oneshot::Sender<bool>,
    },
}

#[derive(Debug)]
pub struct WhipAccepted {
    pub session_id: String,
    pub answer_sdp: String,
}

#[derive(Debug)]
pub enum WhipError {
    BadRequest(String),
}

#[derive(Clone)]
struct WhipState {
    tx: mpsc::Sender<WhipCommand>,
    whip_token: Option<Arc<str>>,
}

pub fn router(tx: mpsc::Sender<WhipCommand>, whip_token: Option<String>) -> Router {
    let state = WhipState {
        tx,
        whip_token: whip_token.map(Arc::from),
    };

    Router::new()
        .route("/whip", post(handle_offer).options(handle_options))
        .route(
            "/whip/session/{id}",
            delete(handle_delete).options(handle_options),
        )
        .with_state(state)
}

fn check_auth(
    headers: &HeaderMap,
    expected: Option<&str>,
) -> Result<(), (StatusCode, &'static str)> {
    let Some(secret) = expected else {
        return Ok(());
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    if provided == Some(secret) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "Unauthorized"))
    }
}

fn cors_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST, DELETE, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Location"),
    );
    headers
}

async fn handle_options() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, cors_headers())
}

async fn handle_offer(
    State(state): State<WhipState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Err(resp) = check_auth(&headers, state.whip_token.as_deref()) {
        return resp.into_response();
    }

    if body.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "Empty SDP offer").into_response();
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    if state
        .tx
        .send(WhipCommand::Offer {
            sdp: body,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return (StatusCode::SERVICE_UNAVAILABLE, "Media engine unavailable").into_response();
    }

    match reply_rx.await {
        Ok(Ok(accepted)) => {
            let mut resp_headers = cors_headers();
            resp_headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/sdp"),
            );
            if let Ok(loc) =
                HeaderValue::from_str(&format!("/whip/session/{}", accepted.session_id))
            {
                resp_headers.insert(header::LOCATION, loc);
            }
            (StatusCode::CREATED, resp_headers, accepted.answer_sdp).into_response()
        }
        Ok(Err(WhipError::BadRequest(msg))) => (StatusCode::BAD_REQUEST, msg).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Media engine dropped reply",
        )
            .into_response(),
    }
}

async fn handle_delete(
    State(state): State<WhipState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    if let Err(resp) = check_auth(&headers, state.whip_token.as_deref()) {
        return resp.into_response();
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    if state
        .tx
        .send(WhipCommand::Delete {
            session_id,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return (StatusCode::SERVICE_UNAVAILABLE, "Media engine unavailable").into_response();
    }

    match reply_rx.await {
        Ok(true) => (StatusCode::OK, cors_headers()).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "Session not found").into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Media engine dropped reply",
        )
            .into_response(),
    }
}
