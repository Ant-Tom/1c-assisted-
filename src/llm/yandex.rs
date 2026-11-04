//! Провайдер YandexGPT (Yandex AI Studio, нативный REST API генерации текста).
//!
//! Сверено с документацией (ноябрь 2026):
//! - руководство «Вызов функции из модели»
//!   (aistudio.yandex.ru/docs/ru/ai-studio/operations/generation/function-call);
//! - справочник `TextGeneration.Completion`
//!   (aistudio.yandex.ru/docs/ru/ai-studio/text-generation/api-ref/TextGeneration/completion);
//! - «Аутентификация в API» (aistudio.yandex.ru/docs/ru/ai-studio/api-ref/authentication).
//!
//! Формат:
//! - `POST {base}/completion`, заголовок `Authorization: Api-Key <ключ>`; при заданном
//!   каталоге также `x-folder-id` (как в примере руководства);
//! - тело: `modelUri` (`gpt://<каталог>/<модель>`), `completionOptions`
//!   (`maxTokens` — строка), `messages[]` с `role` и одним из `text` / `toolCallList` /
//!   `toolResultList`, `tools[].function {name, description, parameters}`;
//! - ответ: `alternatives[].message` и `status` (`ALTERNATIVE_STATUS_TOOL_CALLS`,
//!   `ALTERNATIVE_STATUS_FINAL`, ...). Руководство показывает обёртку `result`, справочник —
//!   без неё: принимаем оба варианта;
//! - у вызова функции нет id: результаты сопоставляются по имени, в том же порядке.
//!   Результаты передаются сообщением `role: user` с `toolResultList`.
//!
//! ВНИМАНИЕ: справочник у поля `tools` пишет, что параметр «пока не поддерживается и будет
//! проигнорирован», а руководство по вызову функций использует именно его. Перед боевым
//! запуском проверьте на реальном ключе, что модель возвращает `toolCallList`.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    arguments_object, arguments_to_string, http_client, map_transport, read_body, ChatRole,
    LlmError, LlmProvider, Message, Reply, ToolCall, ToolSpec,
};
use crate::config::{LlmConfig, Secret};

pub struct YandexGpt {
    client: reqwest::Client,
    endpoint: String,
    api_key: Secret,
    folder_id: Option<String>,
    model_uri: String,
    temperature: f32,
    max_tokens: u32,
}

impl YandexGpt {
    pub fn new(config: &LlmConfig) -> Result<Self, LlmError> {
        let api_key = config
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| LlmError::Configuration("для YandexGPT нужен LLM_API_KEY".into()))?;
        let model_uri = model_uri(&config.model, config.folder_id.as_deref())?;
        Ok(Self {
            client: http_client(config)?,
            endpoint: format!("{}/completion", config.base_url.trim_end_matches('/')),
            api_key,
            folder_id: config.folder_id.clone(),
            model_uri,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
        })
    }
}

/// `gpt://<каталог>/<модель>`; полный URI из конфигурации используется как есть.
pub fn model_uri(model: &str, folder_id: Option<&str>) -> Result<String, LlmError> {
    if model.starts_with("gpt://") {
        return Ok(model.to_string());
    }
    match folder_id {
        Some(folder) if !folder.trim().is_empty() => Ok(format!(
            "gpt://{}/{}",
            folder.trim(),
            model.trim_matches('/')
        )),
        _ => Err(LlmError::Configuration(
            "для YandexGPT нужен LLM_FOLDER_ID или LLM_MODEL вида gpt://<каталог>/<модель>".into(),
        )),
    }
}

#[async_trait]
impl LlmProvider for YandexGpt {
    fn model_name(&self) -> &str {
        &self.model_uri
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Reply, LlmError> {
        let body = build_request(
            &self.model_uri,
            self.temperature,
            self.max_tokens,
            messages,
            tools,
        );
        let mut request = self
            .client
            .post(&self.endpoint)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Api-Key {}", self.api_key.expose()),
            )
            .json(&body);
        if let Some(folder) = &self.folder_id {
            request = request.header("x-folder-id", folder);
        }
        let response = request.send().await.map_err(map_transport)?;
        let text = read_body(response).await?;
        parse_response(&text)
    }
}

pub fn build_request(
    model_uri: &str,
    temperature: f32,
    max_tokens: u32,
    messages: &[Message],
    tools: &[ToolSpec],
) -> Value {
    // Имена функций по id вызова: результаты в YandexGPT передаются по имени.
    let names: HashMap<&str, &str> = messages
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .map(|c| (c.id.as_str(), c.name.as_str()))
        .collect();

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut pending_results: Vec<Value> = Vec::new();
    let flush = |out: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            out.push(json!({
                "role": "user",
                "toolResultList": { "toolResults": std::mem::take(pending) }
            }));
        }
    };

    for m in messages {
        if m.role == ChatRole::Tool {
            let id = m.tool_call_id.as_deref().unwrap_or_default();
            pending_results.push(json!({
                "functionResult": {
                    "name": names.get(id).copied().unwrap_or_default(),
                    "content": m.content.clone().unwrap_or_default(),
                }
            }));
            continue;
        }
        flush(&mut out, &mut pending_results);
        let role = match m.role {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant | ChatRole::Tool => "assistant",
        };
        if m.tool_calls.is_empty() {
            out.push(json!({ "role": role, "text": m.content.clone().unwrap_or_default() }));
        } else {
            let calls: Vec<Value> = m
                .tool_calls
                .iter()
                .map(|c| {
                    json!({ "functionCall": {
                        "name": c.name,
                        "arguments": arguments_object(&c.arguments),
                    }})
                })
                .collect();
            out.push(json!({ "role": "assistant", "toolCallList": { "toolCalls": calls } }));
        }
    }
    flush(&mut out, &mut pending_results);

    let mut body = json!({
        "modelUri": model_uri,
        "completionOptions": {
            "stream": false,
            "temperature": temperature,
            "maxTokens": max_tokens.to_string(),
        },
        "messages": out,
    });
    if !tools.is_empty() {
        body["tools"] = tools
            .iter()
            .map(|t| {
                json!({ "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }})
            })
            .collect();
    }
    body
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireResponse {
    #[serde(default)]
    alternatives: Vec<WireAlternative>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAlternative {
    message: WireMessage,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMessage {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_call_list: Option<WireToolCallList>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireToolCallList {
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireToolCall {
    function_call: WireFunctionCall,
}

#[derive(Debug, Deserialize)]
struct WireFunctionCall {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

pub fn parse_response(text: &str) -> Result<Reply, LlmError> {
    let mut root: Value =
        serde_json::from_str(text).map_err(|e| LlmError::InvalidResponse(e.to_string()))?;
    // Руководство показывает ответ в обёртке `result`, справочник — без неё.
    if let Some(inner) = root.get_mut("result").map(Value::take) {
        root = inner;
    }
    let response: WireResponse =
        serde_json::from_value(root).map_err(|e| LlmError::InvalidResponse(e.to_string()))?;
    let alternative = response
        .alternatives
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::InvalidResponse("пустой список alternatives".into()))?;

    let calls: Vec<ToolCall> = alternative
        .message
        .tool_call_list
        .map(|l| l.tool_calls)
        .unwrap_or_default()
        .into_iter()
        .map(|c| ToolCall {
            // id у YandexGPT нет: уникальный id проставит оркестратор.
            id: String::new(),
            name: c.function_call.name,
            arguments: arguments_to_string(c.function_call.arguments),
            state: None,
        })
        .collect();
    if !calls.is_empty() {
        return Ok(Reply::ToolCalls(calls));
    }
    match alternative.message.text {
        Some(text) => Ok(Reply::Text(text)),
        None => Err(LlmError::InvalidResponse(format!(
            "в ответе нет ни текста, ни вызовов функций (status: {})",
            alternative.status.as_deref().unwrap_or("нет")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_uri_is_built_from_folder() {
        assert_eq!(
            model_uri("yandexgpt/latest", Some("b1g")).unwrap(),
            "gpt://b1g/yandexgpt/latest"
        );
        assert_eq!(
            model_uri("gpt://x/yandexgpt", None).unwrap(),
            "gpt://x/yandexgpt"
        );
        assert!(model_uri("yandexgpt", None).is_err());
    }

    #[test]
    fn request_matches_documented_shape() {
        let messages = vec![
            Message::system("sys"),
            Message::user("сальдо?"),
            Message::assistant_tool_calls(vec![
                ToolCall::new(
                    "call_0_0",
                    "search_counterparty",
                    &json!({"query": "Вектор"}),
                ),
                ToolCall::new(
                    "call_0_1",
                    "search_counterparty",
                    &json!({"query": "Альфа"}),
                ),
            ]),
            Message::tool_result("call_0_0", "{\"status\":\"ok\"}"),
            Message::tool_result("call_0_1", "{\"status\":\"error\"}"),
            Message::assistant("ответ"),
        ];
        let tools = vec![ToolSpec {
            name: "search_counterparty".into(),
            description: "поиск".into(),
            parameters: json!({"type": "object"}),
        }];
        let body = build_request("gpt://b1g/yandexgpt/latest", 0.0, 2000, &messages, &tools);

        assert_eq!(body["modelUri"], "gpt://b1g/yandexgpt/latest");
        assert_eq!(body["completionOptions"]["maxTokens"], "2000");
        assert_eq!(body["completionOptions"]["stream"], false);
        assert_eq!(body["tools"][0]["function"]["name"], "search_counterparty");

        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5, "два результата склеены в одно сообщение");
        assert_eq!(msgs[0], json!({"role": "system", "text": "sys"}));
        let call = &msgs[2]["toolCallList"]["toolCalls"][0]["functionCall"];
        assert_eq!(call["name"], "search_counterparty");
        assert_eq!(
            call["arguments"],
            json!({"query": "Вектор"}),
            "аргументы объектом"
        );
        assert_eq!(msgs[3]["role"], "user");
        let results = msgs[3]["toolResultList"]["toolResults"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["functionResult"]["name"], "search_counterparty");
        assert_eq!(
            results[0]["functionResult"]["content"],
            "{\"status\":\"ok\"}"
        );
        assert_eq!(msgs[4], json!({"role": "assistant", "text": "ответ"}));

        let no_tools = build_request("gpt://b1g/m", 0.3, 10, &messages, &[]);
        assert!(no_tools.get("tools").is_none());
    }

    #[test]
    fn parses_tool_calls_with_and_without_result_wrapper() {
        let bare = r#"{"alternatives":[{"message":{"role":"assistant","toolCallList":{"toolCalls":[
            {"functionCall":{"name":"search_counterparty","arguments":{"query":"Вектор"}}}]}},
            "status":"ALTERNATIVE_STATUS_TOOL_CALLS"}],"usage":{"inputTextTokens":"10"},"modelVersion":"x"}"#;
        let wrapped = format!("{{\"result\":{bare}}}");
        for raw in [bare.to_string(), wrapped] {
            let Reply::ToolCalls(calls) = parse_response(&raw).unwrap() else {
                panic!("ожидались вызовы функций");
            };
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "");
            let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
            assert_eq!(args["query"], "Вектор");
        }
    }

    #[test]
    fn parses_text_and_rejects_empty() {
        let reply = parse_response(
            r#"{"alternatives":[{"message":{"role":"assistant","text":"Готово"},"status":"ALTERNATIVE_STATUS_FINAL"}]}"#,
        )
        .unwrap();
        assert_eq!(reply, Reply::Text("Готово".into()));
        assert!(parse_response(r#"{"alternatives":[]}"#).is_err());
        let err = parse_response(
            r#"{"alternatives":[{"message":{"role":"assistant"},"status":"ALTERNATIVE_STATUS_CONTENT_FILTER"}]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("CONTENT_FILTER"));
    }
}
