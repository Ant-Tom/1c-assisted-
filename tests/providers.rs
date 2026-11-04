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

async fn ask(llm: Arc<dyn LlmProvider>, message: &str) -> ChatResponse {
    let tools = Arc::new(onec_registry(Arc::new(MockOneC::new())).unwrap());
    Orchestrator::new(
        llm,
        tools,
        AuditLog::open_in_memory().unwrap(),
        OrchestratorSettings::default(),
    )
    .handle(ChatRequest {
        user: UserIdentity {
            user_id: "u".into(),
            role: Role::Accountant,
        },
        message: message.into(),
        conversation_id: None,
    })
    .await
    .unwrap()
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
    let has_results = body["messages"]
        .as_array()
        .is_some_and(|m| m.iter().any(|m| m.get("toolResultList").is_some()));
    bodies.lock().unwrap().push(body);
    let alternative = if has_results {
        json!({"message": {"role": "assistant", "text": "Сальдо с АО «Вектор» на 2026-11-01: 0.00 RUB."},
               "status": "ALTERNATIVE_STATUS_FINAL"})
    } else {
        json!({"message": {"role": "assistant", "toolCallList": {"toolCalls": [
                  {"functionCall": {"name": "get_counterparty_balance",
                                    "arguments": {"counterparty_id": "cp-003", "date": "2026-11-01"}}}
               ]}},
               "status": "ALTERNATIVE_STATUS_TOOL_CALLS"})
    };
    // Как в примере руководства: ответ в обёртке `result`.
    Ok(Json(json!({"result": {
        "alternatives": [alternative],
        "usage": {"inputTextTokens": "100", "completionTokens": "20", "totalTokens": "120"},
        "modelVersion": "test"
    }})))
}

#[tokio::test]
async fn yandexgpt_full_loop() {
    let bodies: Bodies = Arc::default();
    let base = serve(
        Router::new()
            .route("/foundationModels/v1/completion", post(yandex_completion))
            .with_state(bodies.clone()),
    )
    .await;

    let mut cfg = LlmConfig::for_backend(LlmBackend::YandexGpt);
    cfg.base_url = format!("{base}/foundationModels/v1");
    cfg.api_key = Some(Secret::new("ya-test-key"));
    cfg.folder_id = Some("b1gtest".into());
    let llm = Arc::new(YandexGpt::new(&cfg).unwrap());
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

// ---------------- GigaChat ----------------

const STATE_ID: &str = "77d3fb14-457a-46ba-937e-8d856156d003";

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
    let has_result = body["messages"]
        .as_array()
        .is_some_and(|m| m.iter().any(|m| m["role"] == "function"));
    s.chat_bodies.push(body);
    let message = if has_result {
        json!({"role": "assistant", "content": "Нашёл АО «Вектор», ИНН 7803456789."})
    } else {
        json!({"role": "assistant", "content": "",
               "function_call": {"name": "search_counterparty", "arguments": {"query": "Вектор"}},
               "functions_state_id": STATE_ID})
    };
    Ok(Json(json!({
        "choices": [{"message": message, "index": 0,
                     "finish_reason": if has_result { "stop" } else { "function_call" }}],
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
