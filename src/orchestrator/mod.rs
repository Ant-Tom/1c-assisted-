//! Цикл «вопрос → модель → вызовы функций → результаты → модель → ответ».

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Local;
use serde::Serialize;
use serde_json::{json, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::auth::UserIdentity;
use crate::config::Limits;
use crate::llm::{LlmError, LlmProvider, Message, Reply, ToolCall};
use crate::tools::onec_tools::SEARCH_COUNTERPARTY;
use crate::tools::{describe_period, parse_arguments, ToolError, ToolRegistry};

pub mod conversation;
pub mod facts;
pub mod prompt;

pub use conversation::{ConversationError, ConversationStore};

/// Результат функции больше этого размера модели не передаётся.
const MAX_TOOL_RESULT_BYTES: usize = 32 * 1024;
/// Сколько символов сырых (неразобранных) аргументов сохраняем для журнала.
const MAX_RAW_ARGS_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub user: UserIdentity,
    pub message: String,
    pub conversation_id: Option<String>,
}

/// Источник данных для ответа: какая функция, с какими параметрами, за какой период.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceRef {
    pub function: String,
    pub arguments: Value,
    pub period: Option<String>,
    /// `ok` или код ошибки.
    pub status: String,
}

/// Факт вызова функции: для источников и журнала.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub function: String,
    pub arguments: Value,
    pub status: String,
    pub error: Option<String>,
    /// Размер результата в байтах (то, что ушло модели).
    pub result_size: usize,
    pub duration: Duration,
}

impl ToolInvocation {
    pub fn source(&self) -> SourceRef {
        SourceRef {
            function: self.function.clone(),
            arguments: self.arguments.clone(),
            period: describe_period(&self.arguments),
            status: self.status.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub request_id: String,
    pub conversation_id: String,
    /// Ответ модели с добавленным кодом блоком «Источники».
    pub answer: String,
    pub sources: Vec<SourceRef>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("пустой вопрос")]
    EmptyMessage,
    #[error("вопрос длиннее {max} символов")]
    MessageTooLong { max: usize },
    #[error(transparent)]
    Conversation(#[from] ConversationError),
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("модель не дала ответа за {limit} итераций")]
    IterationLimit { limit: usize },
    #[error("превышено общее время обработки запроса")]
    Timeout,
}

impl OrchestratorError {
    pub fn code(&self) -> &'static str {
        match self {
            OrchestratorError::EmptyMessage => "empty_message",
            OrchestratorError::MessageTooLong { .. } => "message_too_long",
            OrchestratorError::Conversation(ConversationError::NotFound) => {
                "conversation_not_found"
            }
            OrchestratorError::Conversation(ConversationError::InvalidId) => {
                "invalid_conversation_id"
            }
            OrchestratorError::Llm(_) => "llm_error",
            OrchestratorError::IterationLimit { .. } => "iteration_limit",
            OrchestratorError::Timeout => "timeout",
        }
    }
}

/// Состояние одного запроса.
#[derive(Default)]
struct RunState {
    invocations: Vec<ToolInvocation>,
    /// id контрагентов из поисков, вернувших нескольких кандидатов в этом запросе.
    ambiguous_ids: HashSet<String>,
}

pub struct Orchestrator {
    llm: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    conversations: ConversationStore,
    limits: Limits,
}

impl Orchestrator {
    pub fn new(llm: Arc<dyn LlmProvider>, tools: Arc<ToolRegistry>, limits: Limits) -> Self {
        Self {
            llm,
            tools,
            conversations: ConversationStore::default(),
            limits,
        }
    }

    pub fn model_name(&self) -> &str {
        self.llm.model_name()
    }

    pub async fn handle(&self, req: ChatRequest) -> Result<ChatResponse, OrchestratorError> {
        let question = req.message.trim();
        if question.is_empty() {
            return Err(OrchestratorError::EmptyMessage);
        }
        if question.chars().count() > self.limits.max_message_chars {
            return Err(OrchestratorError::MessageTooLong {
                max: self.limits.max_message_chars,
            });
        }

        let (conversation_id, history) = self
            .conversations
            .load(req.conversation_id.as_deref(), &req.user.user_id)?;
        let request_id = Uuid::new_v4().to_string();

        let mut messages = Vec::with_capacity(history.len() + 2);
        messages.push(Message::system(prompt::system_prompt(
            Local::now().date_naive(),
        )));
        messages.extend(history);
        messages.push(Message::user(question));

        let mut run = RunState::default();
        let outcome = tokio::time::timeout(
            self.limits.request_timeout,
            self.run_loop(&req.user, &request_id, &mut messages, &mut run),
        )
        .await;
        let text = match outcome {
            Ok(result) => result?,
            Err(_) => return Err(OrchestratorError::Timeout),
        };

        // Всё, что модель видела до финального ответа, считается известным.
        let mut known = facts::KnownNumbers::default();
        for m in &messages[..messages.len().saturating_sub(1)] {
            if let Some(content) = &m.content {
                known.add_text(content);
            }
        }
        let mut warnings = Vec::new();
        let unverified = facts::unverified_numbers(&text, &known);
        if !unverified.is_empty() {
            tracing::warn!(%request_id, numbers = ?unverified, "в ответе есть числа не из данных");
            warnings.push(format!(
                "unverified_numbers: в ответе есть числа, которых нет в данных 1С: {}",
                unverified.join(", ")
            ));
        }

        self.conversations
            .save(&conversation_id, &req.user.user_id, messages.split_off(1));

        let sources: Vec<SourceRef> = run.invocations.iter().map(ToolInvocation::source).collect();
        Ok(ChatResponse {
            request_id,
            conversation_id,
            answer: format!("{}\n\n{}", text.trim(), sources_footer(&sources)),
            sources,
            warnings,
        })
    }

    async fn run_loop(
        &self,
        user: &UserIdentity,
        request_id: &str,
        messages: &mut Vec<Message>,
        run: &mut RunState,
    ) -> Result<String, OrchestratorError> {
        let specs = self.tools.specs_for(user.role);
        for iteration in 0..self.limits.max_iterations {
            let reply = self.llm.chat(messages, &specs).await?;
            let calls = match reply {
                Reply::Text(text) => {
                    let text = if text.trim().is_empty() {
                        "Не удалось сформулировать ответ.".to_string()
                    } else {
                        text
                    };
                    messages.push(Message::assistant(text.clone()));
                    return Ok(text);
                }
                Reply::ToolCalls(calls) if calls.is_empty() => {
                    return Err(LlmError::InvalidResponse("пустой список вызовов".into()).into());
                }
                Reply::ToolCalls(calls) => normalize_call_ids(calls, iteration),
            };

            tracing::debug!(%request_id, iteration, calls = calls.len(), "модель запросила вызовы");
            messages.push(Message::assistant_tool_calls(calls.clone()));
            for (index, call) in calls.iter().enumerate() {
                let content = if index < self.limits.max_tool_calls_per_iteration {
                    self.execute_call(user, request_id, call, run).await
                } else {
                    let err = ToolError::TooManyCalls {
                        limit: self.limits.max_tool_calls_per_iteration,
                    };
                    self.record(run, call, Err(&err), 0, Duration::ZERO, request_id);
                    err.to_model_json().to_string()
                };
                messages.push(Message::tool_result(call.id.clone(), content));
            }
        }
        Err(OrchestratorError::IterationLimit {
            limit: self.limits.max_iterations,
        })
    }

    /// Исполняет один вызов и возвращает текст результата для модели.
    async fn execute_call(
        &self,
        user: &UserIdentity,
        request_id: &str,
        call: &ToolCall,
        run: &mut RunState,
    ) -> String {
        let started = Instant::now();

        // Защита от самостоятельного выбора среди похожих контрагентов.
        let ambiguous = parse_arguments(&call.arguments)
            .ok()
            .and_then(|a| {
                a.get("counterparty_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|id| run.ambiguous_ids.contains(id.trim()));

        let result = if ambiguous {
            Err(ToolError::AmbiguousCounterparty)
        } else {
            match tokio::time::timeout(
                self.limits.tool_timeout,
                self.tools.execute(user.role, call),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(ToolError::Timeout),
            }
        };

        if call.name == SEARCH_COUNTERPARTY {
            if let Ok(data) = &result {
                if data.get("ambiguous") == Some(&Value::Bool(true)) {
                    let ids = data
                        .get("candidates")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c.get("id").and_then(Value::as_str))
                        .map(str::to_owned);
                    run.ambiguous_ids.extend(ids);
                }
            }
        }

        // Результат функции передаётся модели как данные в отдельной обёртке.
        let mut outcome = result;
        let mut content = match &outcome {
            Ok(data) => json!({"status": "ok", "data": data}).to_string(),
            Err(err) => err.to_model_json().to_string(),
        };
        if content.len() > MAX_TOOL_RESULT_BYTES {
            let err = ToolError::ResultTooLarge;
            content = err.to_model_json().to_string();
            outcome = Err(err);
        }

        let size = if outcome.is_ok() { content.len() } else { 0 };
        self.record(
            run,
            call,
            outcome.as_ref().map(|_| ()),
            size,
            started.elapsed(),
            request_id,
        );
        content
    }

    fn record(
        &self,
        run: &mut RunState,
        call: &ToolCall,
        outcome: Result<(), &ToolError>,
        result_size: usize,
        duration: Duration,
        request_id: &str,
    ) {
        let arguments = parse_arguments(&call.arguments).unwrap_or_else(|_| {
            json!({ "_raw": call.arguments.chars().take(MAX_RAW_ARGS_CHARS).collect::<String>() })
        });
        let (status, error) = match outcome {
            Ok(()) => ("ok".to_string(), None),
            Err(err) => (err.code().to_string(), Some(err.to_string())),
        };
        tracing::info!(
            %request_id,
            function = %call.name,
            %status,
            duration_ms = duration.as_millis() as u64,
            "вызов функции"
        );
        run.invocations.push(ToolInvocation {
            function: call.name.clone(),
            arguments,
            status,
            error,
            result_size,
            duration,
        });
    }
}

/// Некоторые серверы не присылают id вызова или повторяют его: делаем id уникальными,
/// иначе результаты нельзя однозначно сопоставить вызовам.
fn normalize_call_ids(calls: Vec<ToolCall>, iteration: usize) -> Vec<ToolCall> {
    let mut seen = HashSet::new();
    calls
        .into_iter()
        .enumerate()
        .map(|(i, mut call)| {
            if call.id.trim().is_empty() || !seen.insert(call.id.clone()) {
                call.id = format!("call_{iteration}_{i}");
                seen.insert(call.id.clone());
            }
            call
        })
        .collect()
}

/// Блок «Источники», который формирует код, а не модель.
pub fn sources_footer(sources: &[SourceRef]) -> String {
    if sources.is_empty() {
        return "Источники: данные 1С не запрашивались.".to_string();
    }
    let mut out = String::from("Источники:");
    for s in sources {
        let args = match &s.arguments {
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| match v {
                    Value::String(text) => format!("{k}={text}"),
                    other => format!("{k}={other}"),
                })
                .collect::<Vec<_>>()
                .join(", "),
            other => other.to_string(),
        };
        out.push_str(&format!("\n- 1С: {}({args})", s.function));
        if let Some(period) = &s.period {
            out.push_str(&format!(", {period}"));
        }
        if s.status != "ok" {
            out.push_str(&format!(" — ошибка: {}", s.status));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_ids_are_made_unique() {
        let calls = vec![
            ToolCall::raw("", "a", "{}"),
            ToolCall::raw("x", "b", "{}"),
            ToolCall::raw("x", "c", "{}"),
        ];
        let ids: Vec<_> = normalize_call_ids(calls, 2)
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["call_2_0", "x", "call_2_2"]);
    }

    #[test]
    fn footer_lists_sources_with_period_and_errors() {
        let sources = vec![
            SourceRef {
                function: "get_counterparty_balance".into(),
                arguments: json!({"counterparty_id": "cp-001", "date": "2026-11-01"}),
                period: Some("на 2026-11-01".into()),
                status: "ok".into(),
            },
            SourceRef {
                function: "list_unpaid_invoices".into(),
                arguments: json!({"counterparty_id": "cp-001"}),
                period: None,
                status: "timeout".into(),
            },
        ];
        let footer = sources_footer(&sources);
        assert!(footer.contains(
            "get_counterparty_balance(counterparty_id=cp-001, date=2026-11-01), на 2026-11-01"
        ));
        assert!(footer.contains("ошибка: timeout"));
        assert_eq!(
            sources_footer(&[]),
            "Источники: данные 1С не запрашивались."
        );
    }
}
