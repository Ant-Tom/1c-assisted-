//! Провайдер GigaChat (REST API v1).
//!
//! Сверено с официальной спецификацией OpenAPI GigaChat API
//! (developers.sber.ru/docs/files/openapi/gigachat/api.yml, ноябрь 2026) и руководствами
//! «Авторизация», «Работа с функциями», «Генерация аргументов для пользовательских функций».
//!
//! Формат:
//! - токен: `POST https://ngw.devices.sberbank.ru:9443/api/v2/oauth`, заголовки
//!   `Authorization: Basic <ключ авторизации>`, `RqUID: <uuid4>`, тело
//!   `scope=GIGACHAT_API_PERS|GIGACHAT_API_B2B|GIGACHAT_API_CORP`; ответ
//!   `{access_token, expires_at}` (`expires_at` — unix-время в миллисекундах), токен живёт 30 минут;
//! - генерация: `POST https://api.giga.chat/v1/chat/completions`, `Authorization: Bearer <токен>`;
//! - функции: массив `functions[] {name, description, parameters}`, `function_call: "auto"`;
//! - вызов приходит в `choices[0].message.function_call {id?, name, arguments}` вместе с
//!   `functions_state_id`; `arguments` — объект (на всякий случай принимаем и строку);
//! - результат возвращается сообщением `{"role": "function", "name", "content"}`, где `content` —
//!   JSON-объект, сериализованный в строку; перед ним в истории должно стоять сообщение
//!   ассистента с тем же `function_call` и `functions_state_id`, иначе API вернёт ошибку
//!   «every assistant function call must have a result in history»;
//! - `temperature` строго больше 0: при `LLM_TEMPERATURE=0` поле не передаётся
//!   (используется значение модели по умолчанию).
//!
//! Сертификаты: если TLS-соединение с серверами Сбера не устанавливается, укажите корневой
//! сертификат НУЦ Минцифры в `LLM_CA_CERT_PATH` (PEM).

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    arguments_object, arguments_to_string, http_client, map_transport, read_body, ChatRole,
    LlmError, LlmProvider, Message, Reply, ToolCall, ToolSpec,
};
use crate::config::{LlmConfig, Secret};

/// Документированный срок жизни токена.
const TOKEN_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// Токен обновляется заранее, чтобы не истёк посреди запроса.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

struct CachedToken {
    value: Secret,
    valid_until: Instant,
}

pub struct GigaChat {
    client: reqwest::Client,
    chat_url: String,
    oauth_url: String,
    auth_key: Secret,
    scope: String,
    model: String,
    temperature: f32,
    max_tokens: u32,
    token: Mutex<Option<CachedToken>>,
}

impl GigaChat {
    pub fn new(config: &LlmConfig) -> Result<Self, LlmError> {
        let auth_key = config
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                LlmError::Configuration("для GigaChat нужен LLM_API_KEY (ключ авторизации)".into())
            })?;
        Ok(Self {
            client: http_client(config)?,
            chat_url: format!("{}/chat/completions", config.base_url.trim_end_matches('/')),
            oauth_url: config.oauth_url.clone(),
            auth_key,
            scope: config.scope.clone(),
            model: config.model.clone(),
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            token: Mutex::new(None),
        })
    }

    /// Действующий токен доступа; при `force_refresh` запрашивается новый.
    /// Блокировка удерживается на время запроса токена, чтобы параллельные
    /// запросы не получали токен одновременно.
    async fn access_token(&self, force_refresh: bool) -> Result<Secret, LlmError> {
        let mut guard = self.token.lock().await;
        if !force_refresh {
            if let Some(cached) = guard.as_ref() {
                if Instant::now() + TOKEN_REFRESH_MARGIN < cached.valid_until {
                    return Ok(cached.value.clone());
                }
            }
        }
        let fresh = self.fetch_token().await?;
        let value = fresh.value.clone();
        *guard = Some(fresh);
        Ok(value)
    }

    async fn fetch_token(&self) -> Result<CachedToken, LlmError> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            #[serde(default)]
            expires_at: Option<i64>,
        }

        let response = self
            .client
            .post(&self.oauth_url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Basic {}", self.auth_key.expose()),
            )
            .header("RqUID", Uuid::new_v4().to_string())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            // scope проверен по списку допустимых значений в конфигурации: кодирование не нужно.
            .body(format!("scope={}", self.scope))
            .send()
            .await
            .map_err(map_transport)?;
        let text = read_body(response).await?;
        let token: TokenResponse = serde_json::from_str(&text)
            .map_err(|e| LlmError::InvalidResponse(format!("ответ OAuth: {e}")))?;
        if token.access_token.is_empty() {
            return Err(LlmError::InvalidResponse("пустой access_token".into()));
        }
        tracing::debug!("получен новый токен GigaChat");
        Ok(CachedToken {
            value: Secret::new(token.access_token),
            valid_until: Instant::now() + token_lifetime(token.expires_at, SystemTime::now()),
        })
    }

    async fn send_chat(&self, body: &Value, token: &Secret) -> Result<String, LlmError> {
        let response = self
            .client
            .post(&self.chat_url)
            .bearer_auth(token.expose())
            .json(body)
            .send()
            .await
            .map_err(map_transport)?;
        read_body(response).await
    }
}

/// Сколько ещё действует токен. `expires_at` по документации в миллисекундах; значение,
/// похожее на секунды, тоже понимаем. Не больше документированных 30 минут.
pub fn token_lifetime(expires_at: Option<i64>, now: SystemTime) -> Duration {
    let Some(expires_at) = expires_at.filter(|v| *v > 0) else {
        return TOKEN_LIFETIME;
    };
    let expires_ms = if expires_at < 100_000_000_000 {
        expires_at.saturating_mul(1000)
    } else {
        expires_at
    };
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let left = u64::try_from(expires_ms.saturating_sub(now_ms)).unwrap_or(0);
    Duration::from_millis(left).min(TOKEN_LIFETIME)
}

#[async_trait]
impl LlmProvider for GigaChat {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Reply, LlmError> {
        let body = build_request(
            &self.model,
            self.temperature,
            self.max_tokens,
            messages,
            tools,
        );
        let token = self.access_token(false).await?;
        let text = match self.send_chat(&body, &token).await {
            // Токен мог быть отозван раньше срока: одна повторная попытка с новым.
            Err(LlmError::Status { status: 401, .. }) => {
                let token = self.access_token(true).await?;
                self.send_chat(&body, &token).await?
            }
            other => other?,
        };
        parse_response(&text)
    }
}

pub fn build_request(
    model: &str,
    temperature: f32,
    max_tokens: u32,
    messages: &[Message],
    tools: &[ToolSpec],
) -> Value {
    // Результаты функций по id вызова: в GigaChat каждый результат идёт сразу за своим вызовом.
    let results: HashMap<&str, &str> = messages
        .iter()
        .filter(|m| m.role == ChatRole::Tool)
        .filter_map(|m| Some((m.tool_call_id.as_deref()?, m.content.as_deref()?)))
        .collect();

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            ChatRole::Tool => {} // уже выведены сразу после своих вызовов
            ChatRole::System => out.push(json!({"role": "system", "content": text_of(m)})),
            ChatRole::User => out.push(json!({"role": "user", "content": text_of(m)})),
            ChatRole::Assistant if m.tool_calls.is_empty() => {
                out.push(json!({"role": "assistant", "content": text_of(m)}));
            }
            ChatRole::Assistant => {
                // GigaChat возвращает по одному вызову за ответ; если вызовов несколько,
                // раскладываем их в пары «вызов → результат».
                for call in &m.tool_calls {
                    let mut function_call = json!({
                        "name": call.name,
                        "arguments": arguments_object(&call.arguments),
                    });
                    if Uuid::parse_str(&call.id).is_ok() {
                        function_call["id"] = Value::String(call.id.clone());
                    }
                    let mut assistant = json!({
                        "role": "assistant",
                        "content": "",
                        "function_call": function_call,
                    });
                    if let Some(state) = &call.state {
                        assistant["functions_state_id"] = Value::String(state.clone());
                    }
                    out.push(assistant);
                    let content = results.get(call.id.as_str()).map_or_else(
                        || json!({"status": "error", "error": {"code": "no_result"}}).to_string(),
                        |c| (*c).to_string(),
                    );
                    out.push(json!({"role": "function", "name": call.name, "content": content}));
                }
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": out,
        "max_tokens": max_tokens,
        "stream": false,
    });
    if temperature > 0.0 {
        body["temperature"] = json!(temperature);
    }
    if !tools.is_empty() {
        body["functions"] = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body["function_call"] = json!("auto");
    }
    body
}

fn text_of(m: &Message) -> String {
    m.content.clone().unwrap_or_default()
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    choices: Vec<WireChoice>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    function_call: Option<WireFunctionCall>,
    #[serde(default)]
    functions_state_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireFunctionCall {
    #[serde(default)]
    id: Option<String>,
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

    if let Some(call) = choice.message.function_call {
        return Ok(Reply::ToolCalls(vec![ToolCall {
            id: call.id.unwrap_or_default(),
            name: call.name,
            arguments: arguments_to_string(call.arguments),
            state: choice.message.functions_state_id,
        }]));
    }
    match choice.message.content {
        Some(content) => Ok(Reply::Text(content)),
        None => Err(LlmError::InvalidResponse(format!(
            "в ответе нет ни текста, ни вызова функции (finish_reason: {})",
            choice.finish_reason.as_deref().unwrap_or("нет")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = "77d3fb14-457a-46ba-937e-8d856156d003";
    const CALL_ID: &str = "6f0b1291-c7f3-43c6-bb2e-9f3efb2dc98e";

    #[test]
    fn parses_function_call_with_state() {
        let raw = format!(
            r#"{{"choices":[{{"message":{{"content":"","role":"assistant",
            "function_call":{{"id":"{CALL_ID}","name":"search_counterparty","arguments":{{"query":"Вектор"}}}},
            "functions_state_id":"{STATE}"}},"index":0,"finish_reason":"function_call"}}],
            "created":1,"model":"GigaChat-2-Pro:2.0","object":"chat.completion"}}"#
        );
        let Reply::ToolCalls(calls) = parse_response(&raw).unwrap() else {
            panic!("ожидался вызов функции");
        };
        assert_eq!(calls[0].id, CALL_ID);
        assert_eq!(calls[0].state.as_deref(), Some(STATE));
        let args: Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["query"], "Вектор");
    }

    #[test]
    fn parses_text_and_string_arguments() {
        let reply = parse_response(
            r#"{"choices":[{"message":{"role":"assistant","content":"Привет"},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        assert_eq!(reply, Reply::Text("Привет".into()));
        let Reply::ToolCalls(calls) = parse_response(
            r#"{"choices":[{"message":{"content":"","function_call":{"name":"f","arguments":"{\"a\":1}"}}}]}"#,
        )
        .unwrap() else {
            panic!("ожидался вызов функции");
        };
        assert_eq!(calls[0].arguments, "{\"a\":1}");
        assert_eq!(calls[0].state, None);
        assert!(parse_response(r#"{"choices":[]}"#).is_err());
    }

    #[test]
    fn request_pairs_calls_with_results() {
        let mut call = ToolCall::new(CALL_ID, "search_counterparty", &json!({"query": "Вектор"}));
        call.state = Some(STATE.into());
        let messages = vec![
            Message::system("sys"),
            Message::user("найди"),
            Message::assistant_tool_calls(vec![
                call,
                ToolCall::new(
                    "call_0_1",
                    "list_unpaid_invoices",
                    &json!({"counterparty_id": "cp-1"}),
                ),
            ]),
            Message::tool_result(CALL_ID, "{\"status\":\"ok\"}"),
            Message::tool_result("call_0_1", "{\"status\":\"error\"}"),
            Message::assistant("готово"),
        ];
        let tools = vec![ToolSpec {
            name: "search_counterparty".into(),
            description: "поиск".into(),
            parameters: json!({"type": "object"}),
        }];
        let body = build_request("GigaChat-2-Pro", 0.0, 2000, &messages, &tools);

        assert_eq!(body["model"], "GigaChat-2-Pro");
        assert!(body.get("temperature").is_none(), "0 не передаётся");
        assert_eq!(body["function_call"], "auto");
        assert_eq!(body["functions"][0]["name"], "search_counterparty");

        let msgs = body["messages"].as_array().unwrap();
        let roles: Vec<_> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(
            roles,
            vec![
                "system",
                "user",
                "assistant",
                "function",
                "assistant",
                "function",
                "assistant"
            ]
        );
        assert_eq!(msgs[2]["content"], "");
        assert_eq!(msgs[2]["functions_state_id"], STATE);
        assert_eq!(msgs[2]["function_call"]["id"], CALL_ID);
        assert_eq!(
            msgs[2]["function_call"]["arguments"],
            json!({"query": "Вектор"})
        );
        assert_eq!(msgs[3]["name"], "search_counterparty");
        assert_eq!(msgs[3]["content"], "{\"status\":\"ok\"}");
        assert!(
            msgs[4]["function_call"].get("id").is_none(),
            "не-UUID id не передаётся"
        );
        assert!(msgs[4].get("functions_state_id").is_none());
        assert_eq!(msgs[5]["content"], "{\"status\":\"error\"}");

        let warm = build_request("m", 0.7, 10, &messages, &[]);
        assert!(warm.get("functions").is_none());
        assert!(warm.get("function_call").is_none());
        assert!((warm["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
    }

    #[test]
    fn token_lifetime_handles_units_and_caps() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let in_ms = 1_700_000_000_000 + 10 * 60 * 1000;
        assert_eq!(token_lifetime(Some(in_ms), now), Duration::from_secs(600));
        let in_s = 1_700_000_000 + 600;
        assert_eq!(token_lifetime(Some(in_s), now), Duration::from_secs(600));
        assert_eq!(
            token_lifetime(Some(1_700_000_000_000 + 3_600_000), now),
            TOKEN_LIFETIME
        );
        assert_eq!(token_lifetime(None, now), TOKEN_LIFETIME);
        assert_eq!(token_lifetime(Some(1), now), Duration::ZERO);
    }
}
