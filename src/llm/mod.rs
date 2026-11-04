//! Абстракция языковой модели. Цикл вызова функций пишем сами, без фреймворков.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::config::LlmConfig;

pub mod gigachat;
pub mod mock;
pub mod openai;
pub mod yandex;

pub use gigachat::GigaChat;
pub use mock::{MockProvider, MockStep};
pub use openai::OpenAiCompatible;
pub use yandex::YandexGpt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Запрос модели на вызов функции.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Аргументы как сырая JSON-строка, ровно как их прислала модель.
    /// Разбор и валидация выполняются в реестре функций, чтобы кривой JSON
    /// превращался в ошибку для модели, а не в падение.
    pub arguments: String,
    /// Служебное состояние провайдера, которое нужно вернуть вместе с вызовом
    /// (например, `functions_state_id` у GigaChat). Оркестратор его не трогает.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: &Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
            state: None,
        }
    }

    /// Вызов с произвольной (возможно, невалидной) строкой аргументов.
    pub fn raw(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
            state: None,
        }
    }
}

/// Сообщение диалога в нейтральном формате; провайдеры сами переводят его в свой.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: ChatRole,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self::plain(ChatRole::System, text)
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::plain(ChatRole::User, text)
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::plain(ChatRole::Assistant, text)
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: None,
            tool_calls: calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    fn plain(role: ChatRole, text: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(text.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

/// Описание функции для модели.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema параметров.
    pub parameters: Value,
}

/// Ответ модели: либо итоговый текст, либо запрос на вызов функций.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Text(String),
    ToolCalls(Vec<ToolCall>),
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("ошибка соединения с моделью: {0}")]
    Transport(String),
    #[error("таймаут запроса к модели")]
    Timeout,
    #[error("модель вернула HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("неожиданный ответ модели: {0}")]
    InvalidResponse(String),
    #[error("сценарий MockProvider исчерпан")]
    ScriptExhausted,
    #[error("ошибка конфигурации провайдера: {0}")]
    Configuration(String),
}

/// Сколько символов тела ошибки провайдера сохраняем в тексте ошибки.
pub(crate) const MAX_ERROR_BODY_CHARS: usize = 500;

/// HTTP-клиент для провайдера: таймаут и, при необходимости, дополнительные
/// корневые сертификаты (например, НУЦ Минцифры для GigaChat).
pub(crate) fn http_client(config: &LlmConfig) -> Result<reqwest::Client, LlmError> {
    let mut builder = reqwest::Client::builder().timeout(config.http_timeout);
    if let Some(path) = &config.ca_cert_path {
        let pem = std::fs::read(path).map_err(|e| {
            LlmError::Configuration(format!(
                "не удалось прочитать сертификат {}: {e}",
                path.display()
            ))
        })?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
            LlmError::Configuration(format!("неверный PEM {}: {e}", path.display()))
        })?;
        builder = builder.tls_certs_merge(certs);
    }
    builder
        .build()
        .map_err(|e| LlmError::Configuration(e.without_url().to_string()))
}

/// Ошибка транспорта без URL (в нём могут быть параметры).
pub(crate) fn map_transport(err: reqwest::Error) -> LlmError {
    if err.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::Transport(err.without_url().to_string())
    }
}

/// Читает тело ответа; при неуспешном статусе возвращает типизированную ошибку.
pub(crate) async fn read_body(response: reqwest::Response) -> Result<String, LlmError> {
    let status = response.status();
    let text = response.text().await.map_err(map_transport)?;
    if !status.is_success() {
        return Err(LlmError::Status {
            status: status.as_u16(),
            body: text.chars().take(MAX_ERROR_BODY_CHARS).collect(),
        });
    }
    Ok(text)
}

/// Аргументы вызова как JSON-объект (для провайдеров, которые ждут объект, а не строку).
pub(crate) fn arguments_object(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => v,
        _ => Value::Object(Default::default()),
    }
}

/// Аргументы из ответа провайдера: объект или уже строка.
pub(crate) fn arguments_to_string(value: Option<Value>) -> String {
    match value {
        Some(Value::String(s)) => s,
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Имя модели для журнала.
    fn model_name(&self) -> &str;

    async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Reply, LlmError>;
}
