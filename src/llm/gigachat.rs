//! Заготовка провайдера GigaChat.
//!
//! TODO(gigachat): получение OAuth-токена (эндпоинт, scope, срок жизни и обновление),
//! формат chat completions, описание функций и разбор вызовов функций, а также
//! работу с сертификатами НУЦ Минцифры нужно сверить с актуальной документацией GigaChat API.
//! Формат по памяти намеренно не реализован.

use async_trait::async_trait;

use super::{LlmError, LlmProvider, Message, Reply, ToolSpec};
use crate::config::LlmConfig;

pub struct GigaChat {
    model: String,
    #[allow(dead_code)] // будет использоваться после сверки с документацией
    config: LlmConfig,
}

impl GigaChat {
    pub fn new(config: LlmConfig) -> Self {
        Self {
            model: config.model.clone(),
            config,
        }
    }
}

#[async_trait]
impl LlmProvider for GigaChat {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Reply, LlmError> {
        Err(LlmError::NotImplemented("GigaChat"))
    }
}
