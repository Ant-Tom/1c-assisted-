//! Абстракция языковой модели. Цикл вызова функций пишем сами, без фреймворков.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

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
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: &Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.to_string(),
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
    #[error("провайдер {0} не реализован: формат API нужно сверить с актуальной документацией")]
    NotImplemented(&'static str),
    #[error("сценарий MockProvider исчерпан")]
    ScriptExhausted,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Имя модели для журнала.
    fn model_name(&self) -> &str;

    async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Reply, LlmError>;
}
