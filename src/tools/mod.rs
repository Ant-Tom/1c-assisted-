//! Реестр функций, доступных модели.
//!
//! Перед вызовом обработчика реестр проверяет: функция существует, роль пользователя
//! разрешена, аргументы являются JSON-объектом и проходят схему. Любая проблема
//! возвращается как [`ToolError`], который оркестратор передаёт модели. Паники нет.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use thiserror::Error;

use crate::auth::Role;
use crate::llm::{ToolCall, ToolSpec};
use crate::onec::OneCError;

pub mod onec_tools;
pub mod schema;

pub use onec_tools::onec_registry;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ToolError {
    #[error("неизвестная функция «{name}». Доступные функции: {available}")]
    UnknownTool { name: String, available: String },
    #[error("функция «{tool}» недоступна для роли {role}")]
    Forbidden { tool: String, role: Role },
    #[error("неверные параметры: {0}")]
    InvalidArguments(String),
    #[error(
        "контрагент неоднозначен: по запросу найдено несколько кандидатов. \
         Покажи пользователю кандидатов (название, ИНН) и попроси выбрать одного; \
         функцию с counterparty_id вызывай только после ответа пользователя"
    )]
    AmbiguousCounterparty,
    #[error("превышен лимит вызовов функций за один шаг ({limit}); вызов не выполнен")]
    TooManyCalls { limit: usize },
    #[error("1С не ответила за отведённое время")]
    Timeout,
    #[error("данные не найдены: {0}")]
    NotFound(String),
    #[error("ошибка источника данных: {0}")]
    Backend(String),
}

impl ToolError {
    /// Короткий машинный код: для модели и для журнала.
    pub fn code(&self) -> &'static str {
        match self {
            ToolError::UnknownTool { .. } => "unknown_tool",
            ToolError::Forbidden { .. } => "forbidden",
            ToolError::InvalidArguments(_) => "invalid_arguments",
            ToolError::AmbiguousCounterparty => "ambiguous_counterparty",
            ToolError::TooManyCalls { .. } => "too_many_calls",
            ToolError::Timeout => "timeout",
            ToolError::NotFound(_) => "not_found",
            ToolError::Backend(_) => "backend_error",
        }
    }

    /// Представление ошибки для модели.
    pub fn to_model_json(&self) -> Value {
        json!({
            "status": "error",
            "error": { "code": self.code(), "message": self.to_string() }
        })
    }
}

impl From<OneCError> for ToolError {
    fn from(err: OneCError) -> Self {
        match err {
            OneCError::Timeout => ToolError::Timeout,
            OneCError::NotFound(what) => ToolError::NotFound(what),
            // Детали транспорта модели не нужны; они уходят в журнал через Display.
            other => ToolError::Backend(other.to_string()),
        }
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("функция «{0}» зарегистрирована дважды")]
    Duplicate(String),
}

/// Обработчик функции. Получает аргументы, уже прошедшие проверку схемы.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    async fn call(&self, args: Value) -> Result<Value, ToolError>;
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema параметров. Модель видит её же.
    pub parameters: Value,
    /// Роли, которым разрешён вызов.
    pub allowed_roles: &'static [Role],
    pub handler: Arc<dyn ToolHandler>,
}

impl ToolDef {
    pub fn allows(&self, role: Role) -> bool {
        self.allowed_roles.contains(&role)
    }

    pub fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.to_string(),
            description: self.description.to_string(),
            parameters: self.parameters.clone(),
        }
    }
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<ToolDef>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, def: ToolDef) -> Result<(), RegistryError> {
        if self.get(def.name).is_some() {
            return Err(RegistryError::Duplicate(def.name.to_string()));
        }
        self.tools.push(def);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// Описания функций, которые разрешены роли. Остальные модель просто не видит.
    pub fn specs_for(&self, role: Role) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|t| t.allows(role))
            .map(ToolDef::spec)
            .collect()
    }

    /// Проверяет и исполняет вызов. Роль проверяется здесь, даже если модель
    /// не должна была видеть функцию.
    pub async fn execute(&self, role: Role, call: &ToolCall) -> Result<Value, ToolError> {
        let Some(def) = self.get(&call.name) else {
            let available = self
                .tools
                .iter()
                .filter(|t| t.allows(role))
                .map(|t| t.name)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ToolError::UnknownTool {
                name: call.name.clone(),
                available,
            });
        };
        if !def.allows(role) {
            return Err(ToolError::Forbidden {
                tool: def.name.to_string(),
                role,
            });
        }
        let args = parse_arguments(&call.arguments)?;
        schema::validate(&def.parameters, &args)
            .map_err(|errs| ToolError::InvalidArguments(errs.join("; ")))?;
        def.handler.call(args).await
    }
}

/// Разбирает аргументы вызова. Пустая строка считается пустым объектом
/// (некоторые серверы так передают вызов без параметров).
pub fn parse_arguments(raw: &str) -> Result<Value, ToolError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|e| ToolError::InvalidArguments(format!("аргументы не являются JSON: {e}")))?;
    if !value.is_object() {
        return Err(ToolError::InvalidArguments(
            "аргументы должны быть JSON-объектом".into(),
        ));
    }
    Ok(value)
}

/// Человекочитаемый период по аргументам вызова: для блока «Источники».
pub fn describe_period(args: &Value) -> Option<String> {
    let s = |key: &str| args.get(key).and_then(Value::as_str);
    match (s("date"), s("date_from"), s("date_to")) {
        (Some(d), _, _) => Some(format!("на {d}")),
        (None, Some(from), Some(to)) => Some(format!("с {from} по {to}")),
        _ => None,
    }
}
