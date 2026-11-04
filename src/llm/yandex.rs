//! Заготовка провайдера YandexGPT.
//!
//! TODO(yandexgpt): формат запроса/ответа, вызов функций (tools), аутентификацию
//! (API-ключ сервисного аккаунта или IAM-токен), идентификатор каталога и URI модели
//! нужно сверить с актуальной документацией Yandex Cloud Foundation Models.
//! Формат по памяти намеренно не реализован.

use async_trait::async_trait;

use super::{LlmError, LlmProvider, Message, Reply, ToolSpec};
use crate::config::LlmConfig;

pub struct YandexGpt {
    model: String,
    #[allow(dead_code)] // будет использоваться после сверки с документацией
    config: LlmConfig,
}

impl YandexGpt {
    pub fn new(config: LlmConfig) -> Self {
        Self {
            model: config.model.clone(),
            config,
        }
    }
}

#[async_trait]
impl LlmProvider for YandexGpt {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Reply, LlmError> {
        Err(LlmError::NotImplemented("YandexGPT"))
    }
}
