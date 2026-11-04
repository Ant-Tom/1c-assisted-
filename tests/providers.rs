//! Полный цикл оркестратора через YandexGPT и GigaChat против фейковых серверов,
//! повторяющих документированный формат API.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use onec_assistant::audit::AuditLog;
use onec_assistant::auth::{Role, UserIdentity};
use onec_assistant::config::{LlmBackend, LlmConfig, Secret};
use onec_assistant::llm::{GigaChat, LlmProvider, YandexGpt};
use onec_assistant::onec::MockOneC;
use onec_assistant::orchestrator::{ChatRequest, ChatResponse, Orchestrator, OrchestratorSettings};
use onec_assistant::tools::onec_registry;

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn orchestrator(llm: Arc<dyn LlmProvider>) -> Orchestrator {
    let tools = Arc::new(onec_registry(Arc::new(MockOneC::new())).unwrap());
    Orchestrator::new(
        llm,
        tools,
        AuditLog::open_in_memory().unwrap(),
        OrchestratorSettings::default(),
    )
}

async fn ask_in(orch: &Orchestrator, message: &str, conversation_id: Option<&str>) -> ChatResponse {
    orch.handle(ChatRequest {
        user: UserIdentity {
            user_id: "u".into(),
            role: Role::Accountant,
        },
        message: message.into(),
        conversation_id: conversation_id.map(str::to_owned),
    })
    .await
    .unwrap()
}

async fn ask(llm: Arc<dyn LlmProvider>, message: &str) -> ChatResponse {
    ask_in(&orchestrator(llm), message, None).await
}

/// Проверяет, что результат функции относится именно к этой функции.
fn assert_result_matches(function: &str, content: &str) {
    let content: Value = serde_json::from_str(content).unwrap();
    let key = match function {
        "search_counterparty" => "candidates",
        "get_counterparty_balance" => "balance",
        other => panic!("неожиданная функция {other}"),
    };
    assert!(
        content["data"].get(key).is_some(),
        "результат {content} не от функции {function}"
    );
}

const SALDO_TEXT: &str = "Сальдо с АО «Вектор» на 2026-11-01: 0.00 RUB.";
const FOUND_TEXT: &str = "Нашёл АО «Вектор», ИНН 7803456789.";

fn wants_saldo(text: &str) -> bool {
    text.to_lowercase().contains("сальдо")
}

// ---------------- YandexGPT ----------------

type Bodies = Arc<Mutex<Vec<Value>>>;

async fn yandex_completion(
    State(bodies): State<Bodies>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if header(&headers, "authorization") != Some("Api-Key ya-test-key")
        || header(&headers, "x-folder-id") != Some("b1gtest")
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Решение принимается по последнему сообщению, как это делала бы модель.
    let last = body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .cloned()
        .unwrap_or_default();
    bodies.lock().unwrap().push(body);
    let alternative = if let Some(results) = last.get("toolResultList") {
        let name = &results["toolResults"][0]["functionResult"]["name"];
        let text = if name == "get_counterparty_balance" {
            SALDO_TEXT
        } else {
            FOUND_TEXT
        };
        json!({"message": {"role": "assistant", "text": text}, "status": "ALTERNATIVE_STATUS_FINAL"})
    } else {
        let call = if wants_saldo(last["text"].as_str().unwrap_or_default()) {
            json!({"name": "get_counterparty_balance",
                   "arguments": {"counterparty_id": "cp-003", "date": "2026-11-01"}})
        } else {
            json!({"name": "search_counterparty", "arguments": {"query": "Вектор"}})
        };
        json!({"message": {"role": "assistant",
                           "toolCallList": {"toolCalls": [{"functionCall": call}]}},
               "status": "ALTERNATIVE_STATUS_TOOL_CALLS"})
    };
    // Как в примере руководства: ответ в обёртке `result`.
    Ok(Json(json!({"result": {
        "alternatives": [alternative],
        "usage": {"inputTextTokens": "100", "completionTokens": "20", "totalTokens": "120"},
        "modelVersion": "test"
    }})))
}

async fn yandex_llm(bodies: Bodies) -> Arc<YandexGpt> {
    let base = serve(
        Router::new()
            .route("/foundationModels/v1/completion", post(yandex_completion))
            .with_state(bodies),
    )
    .await;
    let mut cfg = LlmConfig::for_backend(LlmBackend::YandexGpt);
    cfg.base_url = format!("{base}/foundationModels/v1");
    cfg.api_key = Some(Secret::new("ya-test-key"));
    cfg.folder_id = Some("b1gtest".into());
    Arc::new(YandexGpt::new(&cfg).unwrap())
}

#[tokio::test]
async fn yandexgpt_full_loop() {
    let bodies: Bodies = Arc::default();
    let llm = yandex_llm(bodies.clone()).await;
    assert_eq!(llm.model_name(), "gpt://b1gtest/yandexgpt/latest");

    let resp = ask(llm, "Сальдо с Вектором на 1 ноября").await;
    assert!(resp.answer.starts_with("Сальдо с АО «Вектор»"));
    assert_eq!(resp.sources.len(), 1);
    assert_eq!(resp.sources[0].status, "ok");
    assert!(resp.warnings.is_empty(), "{:?}", resp.warnings);

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["modelUri"], "gpt://b1gtest/yandexgpt/latest");
    assert_eq!(bodies[0]["tools"].as_array().unwrap().len(), 4);
    let msgs = bodies[1]["messages"].as_array().unwrap();
    let result = &msgs[3]["toolResultList"]["toolResults"][0]["functionResult"];
    assert_eq!(result["name"], "get_counterparty_balance");
    let content: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["data"]["balance"], "0.00");
}

/// Регрессия: у YandexGPT нет id вызовов; во втором вопросе диалога история
/// не должна путать результаты функций из разных вопросов.
#[tokio::test]
async fn yandexgpt_second_question_keeps_history_consistent() {
    let bodies: Bodies = Arc::default();
    let orch = orchestrator(yandex_llm(bodies.clone()).await);

    let first = ask_in(&orch, "Найди Вектор", None).await;
    assert!(first.answer.starts_with(FOUND_TEXT));
    let second = ask_in(
        &orch,
        "А какое сальдо на 1 ноября?",
        Some(&first.conversation_id),
    )
    .await;
    assert!(second.answer.starts_with(SALDO_TEXT));

    let bodies = bodies.lock().unwrap();
    let msgs = bodies.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let mut pairs = 0;
    for (i, m) in msgs.iter().enumerate() {
        let Some(calls) = m.get("toolCallList") else {
            continue;
        };
        let call = &calls["toolCalls"][0]["functionCall"];
        let result = &msgs[i + 1]["toolResultList"]["toolResults"][0]["functionResult"];
        assert_eq!(result["name"], call["name"], "имя результата = имя вызова");
        assert_result_matches(
            call["name"].as_str().unwrap(),
            result["content"].as_str().unwrap(),
        );
        pairs += 1;
    }
    assert_eq!(pairs, 2, "в истории оба вызова: поиск и сальдо");
}

// ---------------- GigaChat ----------------

const STATE_ID: &str = "77d3fb14-457a-46ba-937e-8d856156d003";
const STATE_ID_2: &str = "6f0b1291-c7f3-43c6-bb2e-9f3efb2dc98e";

#[derive(Default)]
struct GigaState {
    tokens_issued: usize,
    /// Токен, который сервер считает отозванным (для проверки обновления после 401).
    revoked: Option<String>,
    revoke_first: bool,
    chat_bodies: Vec<Value>,
}

type Giga = Arc<Mutex<GigaState>>;

async fn giga_oauth(
    State(state): State<Giga>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, StatusCode> {
    let rquid_ok = header(&headers, "rquid").is_some_and(|v| uuid::Uuid::parse_str(v).is_ok());
    if header(&headers, "authorization") != Some("Basic giga-auth-key")
        || !rquid_ok
        || body != "scope=GIGACHAT_API_PERS"
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let mut s = state.lock().unwrap();
    s.tokens_issued += 1;
    let token = format!("tok-{}", s.tokens_issued);
    if s.revoke_first && s.tokens_issued == 1 {
        s.revoked = Some(token.clone());
    }
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 30 * 60 * 1000;
    Ok(Json(
        json!({"access_token": token, "expires_at": expires_at}),
    ))
}

async fn giga_chat(
    State(state): State<Giga>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let mut s = state.lock().unwrap();
    let bearer = header(&headers, "authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let latest = format!("tok-{}", s.tokens_issued);
    if bearer != latest || s.revoked.as_deref() == Some(bearer) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let last = body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .cloned()
        .unwrap_or_default();
    s.chat_bodies.push(body);
    // Как в примерах документации: `function_call` без `id`.
    let (message, finish_reason) = if last["role"] == "function" {
        let text = if last["name"] == "get_counterparty_balance" {
            SALDO_TEXT
        } else {
            FOUND_TEXT
        };
        (json!({"role": "assistant", "content": text}), "stop")
    } else if wants_saldo(last["content"].as_str().unwrap_or_default()) {
        (
            json!({"role": "assistant", "content": "",
                   "function_call": {"name": "get_counterparty_balance",
                                     "arguments": {"counterparty_id": "cp-003", "date": "2026-11-01"}},
                   "functions_state_id": STATE_ID_2}),
            "function_call",
        )
    } else {
        (
            json!({"role": "assistant", "content": "",
                   "function_call": {"name": "search_counterparty", "arguments": {"query": "Вектор"}},
                   "functions_state_id": STATE_ID}),
            "function_call",
        )
    };
    Ok(Json(json!({
        "choices": [{"message": message, "index": 0, "finish_reason": finish_reason}],
        "created": 1, "model": "GigaChat-2-Pro:test", "object": "chat.completion"
    })))
}

async fn giga_server(state: Giga) -> LlmConfig {
    let base = serve(
        Router::new()
            .route("/api/v2/oauth", post(giga_oauth))
            .route("/v1/chat/completions", post(giga_chat))
            .with_state(state),
    )
    .await;
    let mut cfg = LlmConfig::for_backend(LlmBackend::GigaChat);
    cfg.base_url = format!("{base}/v1");
    cfg.oauth_url = format!("{base}/api/v2/oauth");
    cfg.api_key = Some(Secret::new("giga-auth-key"));
    cfg
}

#[tokio::test]
async fn gigachat_full_loop_reuses_token() {
    let state: Giga = Arc::default();
    let cfg = giga_server(state.clone()).await;
    let llm = Arc::new(GigaChat::new(&cfg).unwrap());

    let resp = ask(llm, "Найди Вектор").await;
    assert!(resp.answer.starts_with("Нашёл АО «Вектор»"));
    assert_eq!(resp.sources[0].status, "ok");
    assert!(resp.warnings.is_empty(), "{:?}", resp.warnings);

    let s = state.lock().unwrap();
    assert_eq!(s.tokens_issued, 1, "токен получен один раз на два запроса");
    assert_eq!(s.chat_bodies.len(), 2);
    assert_eq!(s.chat_bodies[0]["function_call"], "auto");
    assert_eq!(s.chat_bodies[0]["functions"].as_array().unwrap().len(), 4);

    let msgs = s.chat_bodies[1]["messages"].as_array().unwrap();
    let assistant = msgs
        .iter()
        .find(|m| m.get("function_call").is_some())
        .unwrap();
    assert_eq!(assistant["functions_state_id"], STATE_ID);
    assert_eq!(assistant["content"], "");
    let function = msgs.iter().find(|m| m["role"] == "function").unwrap();
    assert_eq!(function["name"], "search_counterparty");
    let content: Value = serde_json::from_str(function["content"].as_str().unwrap()).unwrap();
    assert!(content.is_object(), "content — JSON-объект в строке");
    assert_eq!(content["data"]["candidates"][0]["inn"], "7803456789");
}

#[tokio::test]
async fn gigachat_refreshes_token_after_401() {
    let state: Giga = Arc::new(Mutex::new(GigaState {
        revoke_first: true,
        ..GigaState::default()
    }));
    let cfg = giga_server(state.clone()).await;
    let llm = Arc::new(GigaChat::new(&cfg).unwrap());

    let resp = ask(llm, "Найди Вектор").await;
    assert!(resp.answer.starts_with("Нашёл"));
    assert_eq!(state.lock().unwrap().tokens_issued, 2);
}

#[tokio::test]
async fn gigachat_wrong_key_is_typed_error() {
    let state: Giga = Arc::default();
    let mut cfg = giga_server(state.clone()).await;
    cfg.api_key = Some(Secret::new("wrong"));
    let llm = GigaChat::new(&cfg).unwrap();
    let err = llm
        .chat(&[onec_assistant::llm::Message::user("x")], &[])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
    assert!(!err.to_string().contains("wrong"));
}

/// Регрессия: GigaChat не прислал `function_call.id`; во втором вопросе диалога
/// каждый вызов должен идти со своим результатом и своим `functions_state_id`.
#[tokio::test]
async fn gigachat_second_question_keeps_history_consistent() {
    let state: Giga = Arc::default();
    let cfg = giga_server(state.clone()).await;
    let orch = orchestrator(Arc::new(GigaChat::new(&cfg).unwrap()));

    let first = ask_in(&orch, "Найди Вектор", None).await;
    assert!(first.answer.starts_with(FOUND_TEXT));
    let second = ask_in(
        &orch,
        "А какое сальдо на 1 ноября?",
        Some(&first.conversation_id),
    )
    .await;
    assert!(second.answer.starts_with(SALDO_TEXT));

    let s = state.lock().unwrap();
    let msgs = s.chat_bodies.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    let mut states = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        let Some(call) = m.get("function_call") else {
            continue;
        };
        let result = &msgs[i + 1];
        assert_eq!(result["role"], "function");
        assert_eq!(result["name"], call["name"], "имя результата = имя вызова");
        assert_result_matches(
            call["name"].as_str().unwrap(),
            result["content"].as_str().unwrap(),
        );
        states.push(m["functions_state_id"].as_str().unwrap().to_string());
    }
    assert_eq!(states, vec![STATE_ID, STATE_ID_2]);
}
