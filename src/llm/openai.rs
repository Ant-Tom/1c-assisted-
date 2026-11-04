//! Провайдер для серверов с OpenAI-совместимым `/v1/chat/completions`
//! (llama-server, vLLM и подобные).
//!
//! Учтены расхождения между серверами: отсутствующий `id` вызова (его проставит
//! оркестратор) и `arguments` в виде объекта вместо строки.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ChatRole, LlmError, LlmProvider, Message, Reply, ToolCall, ToolSpec};
use crate::config::{LlmConfig, Secret};

/// Сколько символов тела ошибки сохраняем в тексте ошибки.
const MAX_ERROR_BODY_CHARS: usize = 500;

pub struct OpenAiCompatible {
    client: reqwest::Client,
    endpoint: String,
    api_key: Option<Secret>,
    model: String,
    temperature: f32,
}

impl OpenAiCompatible {
    pub fn new(config: &LlmConfig) -> Result<Self, LlmError> {
        let client = reqwest::Client::builder()
            .timeout(config.http_timeout)
            .build()
            .map_err(|e| LlmError::Transport(e.without_url().to_string()))?;
        Ok(Self {
            client,
            endpoint: format!("{}/chat/completions", config.base_url.trim_end_matches('/')),
            api_key: config.api_key.clone().filter(|k| !k.is_empty()),
            model: config.model.clone(),
            temperature: config.temperature,
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatible {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Reply, LlmError> {
        let body = build_request(&self.model, self.temperature, messages, tools);
        let mut request = self.client.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key.expose());
        }
        let response = request.send().await.map_err(map_transport)?;
        let status = response.status();
        let text = response.text().await.map_err(map_transport)?;
        if !status.is_success() {
            return Err(LlmError::Status {
                status: status.as_u16(),
                body: text.chars().take(MAX_ERROR_BODY_CHARS).collect(),
            });
        }
        parse_response(&text)
    }
}

fn map_transport(err: reqwest::Error) -> LlmError {
    if err.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::Transport(err.without_url().to_string())
    }
}

// ---------- формат запроса ----------

#[derive(Debug, Serialize)]
pub struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    temperature: f32,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct WireMessage {
    role: &'static str,
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCallOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WireToolCallOut {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionOut,
}

#[derive(Debug, Serialize)]
struct WireFunctionOut {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction<'a>,
}

#[derive(Debug, Serialize)]
struct WireToolFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

pub fn build_request<'a>(
    model: &'a str,
    temperature: f32,
    messages: &[Message],
    tools: &'a [ToolSpec],
) -> WireRequest<'a> {
    let messages = messages
        .iter()
        .map(|m| WireMessage {
            role: match m.role {
                ChatRole::System => "system",
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
                ChatRole::Tool => "tool",
            },
            content: m.content.clone(),
            tool_calls: m
                .tool_calls
                .iter()
                .map(|c| WireToolCallOut {
                    id: c.id.clone(),
                    kind: "function",
                    function: WireFunctionOut {
                        name: c.name.clone(),
                        arguments: c.arguments.clone(),
                    },
                })
                .collect(),
            tool_call_id: m.tool_call_id.clone(),
        })
        .collect();
    let wire_tools: Vec<WireTool<'a>> = tools
        .iter()
        .map(|t| WireTool {
            kind: "function",
            function: WireToolFunction {
                name: &t.name,
                description: &t.description,
                parameters: &t.parameters,
            },
        })
        .collect();
    let tool_choice = (!wire_tools.is_empty()).then_some("auto");
    WireRequest {
        model,
        messages,
        tools: wire_tools,
        tool_choice,
        temperature,
        stream: false,
    }
}

// ---------- формат ответа ----------

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    choices: Vec<WireChoice>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireMessageIn,
}

#[derive(Debug, Deserialize)]
struct WireMessageIn {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCallIn>>,
}

#[derive(Debug, Deserialize)]
struct WireToolCallIn {
    #[serde(default)]
    id: Option<String>,
    function: WireFunctionIn,
}

#[derive(Debug, Deserialize)]
struct WireFunctionIn {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

pub fn parse_response(text: &str) -> Result<Reply, LlmError> {
    let response: WireResponse =
        serde_json::from_str(text).map_err(|e| LlmError::InvalidResponse(e.to_string()))?;
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::InvalidResponse("пустой список choices".into()))?;
    let message = choice.message;

    let calls: Vec<ToolCall> = message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|c| ToolCall {
            id: c.id.unwrap_or_default(),
            name: c.function.name,
            arguments: match c.function.arguments {
                Some(Value::String(s)) => s,
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            },
        })
        .collect();
    if !calls.is_empty() {
        return Ok(Reply::ToolCalls(calls));
    }
    match message.content {
        Some(text) => Ok(Reply::Text(text)),
        None => Err(LlmError::InvalidResponse(
            "в ответе нет ни текста, ни вызовов функций".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_has_openai_shape() {
        let messages = vec![
            Message::system("sys"),
            Message::user("вопрос"),
            Message::assistant_tool_calls(vec![ToolCall::new(
                "call_1",
                "search_counterparty",
                &json!({"query": "Вектор"}),
            )]),
            Message::tool_result("call_1", "{\"status\":\"ok\"}"),
        ];
        let tools = vec![ToolSpec {
            name: "search_counterparty".into(),
            description: "поиск".into(),
            parameters: json!({"type": "object"}),
        }];
        let wire = serde_json::to_value(build_request("m", 0.0, &messages, &tools)).unwrap();
        assert_eq!(wire["model"], "m");
        assert_eq!(wire["stream"], false);
        assert_eq!(wire["tool_choice"], "auto");
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["function"]["name"], "search_counterparty");
        assert_eq!(wire["messages"][0]["role"], "system");
        let call = &wire["messages"][2]["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["arguments"], "{\"query\":\"Вектор\"}");
        assert!(wire["messages"][2]["content"].is_null());
        assert_eq!(wire["messages"][3]["role"], "tool");
        assert_eq!(wire["messages"][3]["tool_call_id"], "call_1");

        let no_tools = serde_json::to_value(build_request("m", 0.0, &messages, &[])).unwrap();
        assert!(no_tools.get("tools").is_none());
        assert!(no_tools.get("tool_choice").is_none());
    }

    #[test]
    fn parses_text_reply() {
        let reply = parse_response(
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"Привет"},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        assert_eq!(reply, Reply::Text("Привет".into()));
    }

    #[test]
    fn parses_tool_calls_with_string_and_object_arguments() {
        let reply = parse_response(
            r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[
                {"id":"call_a","type":"function","function":{"name":"search_counterparty","arguments":"{\"query\":\"Ромашка\"}"}},
                {"type":"function","function":{"name":"list_unpaid_invoices","arguments":{"counterparty_id":"cp-001"}}}
            ]},"finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();
        let Reply::ToolCalls(calls) = reply else {
            panic!("ожидались вызовы функций");
        };
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].arguments, "{\"query\":\"Ромашка\"}");
        assert_eq!(calls[1].id, "");
        let args: Value = serde_json::from_str(&calls[1].arguments).unwrap();
        assert_eq!(args["counterparty_id"], "cp-001");
    }

    #[test]
    fn rejects_malformed_responses() {
        assert!(parse_response("not json").is_err());
        assert!(parse_response(r#"{"choices":[]}"#).is_err());
        assert!(parse_response(r#"{"choices":[{"message":{"content":null}}]}"#).is_err());
    }
}
