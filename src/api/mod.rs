//! HTTP API: `POST /chat`, `GET /health`.
//!
//! ВНИМАНИЕ: в MVP `user_id` берётся из тела запроса и ничем не подтверждается.
//! Сервис должен стоять за шлюзом, который аутентифицирует пользователя
//! (позже это будет Битрикс24 через `IdentityResolver`).

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::audit::AuditRecord;
use crate::auth::{AuthError, IdentityResolver};
use crate::orchestrator::{ChatRequest, Orchestrator, OrchestratorError, SourceRef};

const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_USER_ID_CHARS: usize = 128;

#[derive(Clone)]
pub struct AppState {
    pub orchestrator: Arc<Orchestrator>,
    pub identity: Arc<dyn IdentityResolver>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/chat", post(chat))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatBody {
    pub user_id: String,
    pub message: String,
    #[serde(default)]
    pub conversation_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatReply {
    pub request_id: String,
    pub conversation_id: String,
    pub answer: String,
    /// Использованные функции с параметрами и периодом.
    pub sources: Vec<SourceRefDto>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceRefDto {
    pub function: String,
    pub arguments: serde_json::Value,
    pub period: Option<String>,
    pub status: String,
}

impl From<SourceRef> for SourceRefDto {
    fn from(s: SourceRef) -> Self {
        Self {
            function: s.function,
            arguments: s.arguments,
            period: s.period,
            status: s.status,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

/// Ошибка API. Внутренние подробности (тексты ошибок модели, 1С) наружу не отдаются:
/// они есть в журнале и в логах.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code.to_string(),
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<OrchestratorError> for ApiError {
    fn from(err: OrchestratorError) -> Self {
        let code = err.code();
        match err {
            OrchestratorError::EmptyMessage
            | OrchestratorError::MessageTooLong { .. }
            | OrchestratorError::Conversation(crate::orchestrator::ConversationError::InvalidId) => {
                ApiError::new(StatusCode::BAD_REQUEST, code, err.to_string())
            }
            OrchestratorError::Conversation(_) => {
                ApiError::new(StatusCode::NOT_FOUND, code, err.to_string())
            }
            OrchestratorError::Llm(_) => ApiError::new(
                StatusCode::BAD_GATEWAY,
                code,
                "языковая модель недоступна, попробуйте позже",
            ),
            OrchestratorError::IterationLimit { .. } => ApiError::new(
                StatusCode::BAD_GATEWAY,
                code,
                "не удалось получить ответ: модель не уложилась в лимит шагов",
            ),
            OrchestratorError::Timeout => ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                code,
                "не удалось получить ответ за отведённое время",
            ),
        }
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok"}))
}

async fn chat(
    State(state): State<AppState>,
    body: Result<Json<ChatBody>, JsonRejection>,
) -> Result<Json<ChatReply>, ApiError> {
    let Json(body) = body.map_err(|rejection| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            rejection.body_text(),
        )
    })?;

    let user_id = body.user_id.trim();
    if user_id.is_empty() || user_id.chars().count() > MAX_USER_ID_CHARS {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_user_id",
            "user_id пустой или слишком длинный",
        ));
    }

    let user = match state.identity.resolve(user_id).await {
        Ok(user) => user,
        Err(AuthError::UnknownUser(_)) => {
            audit_rejected(&state, user_id, "unknown_user").await;
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "unknown_user",
                "пользователь не найден или не имеет доступа",
            ));
        }
        Err(err) => {
            tracing::error!(error = %err, "ошибка определения пользователя");
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "identity_error",
                "не удалось определить пользователя",
            ));
        }
    };

    let response = state
        .orchestrator
        .handle(ChatRequest {
            user,
            message: body.message,
            conversation_id: body.conversation_id,
        })
        .await?;

    Ok(Json(ChatReply {
        request_id: response.request_id,
        conversation_id: response.conversation_id,
        answer: response.answer,
        sources: response.sources.into_iter().map(Into::into).collect(),
        warnings: response.warnings,
    }))
}

/// Запросы неизвестных пользователей тоже попадают в журнал (без текста вопроса).
async fn audit_rejected(state: &AppState, user_id: &str, status: &str) {
    let record = AuditRecord {
        ts: Utc::now(),
        request_id: uuid::Uuid::new_v4().to_string(),
        conversation_id: None,
        user_id: user_id.to_string(),
        role: None,
        question: None,
        function_name: None,
        arguments: None,
        status: status.to_string(),
        result_size: None,
        model: None,
        duration_ms: 0,
        error: None,
    };
    if let Err(err) = state.orchestrator.audit_log().write(vec![record]).await {
        tracing::error!(error = %err, "не удалось записать журнал");
    }
}
