//! Провайдер со сценарием заранее заданных ответов. Для тестов.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;

use super::{LlmError, LlmProvider, Message, Reply, ToolCall, ToolSpec};

/// Шаг сценария.
pub enum MockStep {
    Reply(Reply),
    /// Ответ с задержкой (для проверки общего таймаута).
    Delayed(Duration, Reply),
    Error(LlmError),
}

impl MockStep {
    pub fn text(text: impl Into<String>) -> Self {
        MockStep::Reply(Reply::Text(text.into()))
    }

    pub fn tool_calls(calls: Vec<ToolCall>) -> Self {
        MockStep::Reply(Reply::ToolCalls(calls))
    }

    pub fn tool_call(call: ToolCall) -> Self {
        Self::tool_calls(vec![call])
    }
}

/// Что модель получила на вход при очередном обращении.
#[derive(Debug, Clone)]
pub struct MockRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

pub struct MockProvider {
    model: String,
    script: Mutex<VecDeque<MockStep>>,
    /// Ответ, который возвращается, когда сценарий закончился (например, бесконечные вызовы).
    repeat: Option<Reply>,
    requests: Mutex<Vec<MockRequest>>,
}

impl MockProvider {
    pub fn new(steps: impl IntoIterator<Item = MockStep>) -> Self {
        Self {
            model: "mock-model".into(),
            script: Mutex::new(steps.into_iter().collect()),
            repeat: None,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Всегда отвечает одним и тем же.
    pub fn repeating(reply: Reply) -> Self {
        Self {
            repeat: Some(reply),
            ..Self::new([])
        }
    }

    /// Все обращения к модели в порядке поступления.
    pub fn requests(&self) -> Vec<MockRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn remaining_steps(&self) -> usize {
        self.script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Reply, LlmError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(MockRequest {
                messages: messages.to_vec(),
                tools: tools.to_vec(),
            });
        let step = self
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match step {
            Some(MockStep::Reply(reply)) => Ok(reply),
            Some(MockStep::Delayed(delay, reply)) => {
                tokio::time::sleep(delay).await;
                Ok(reply)
            }
            Some(MockStep::Error(err)) => Err(err),
            None => self.repeat.clone().ok_or(LlmError::ScriptExhausted),
        }
    }
}
