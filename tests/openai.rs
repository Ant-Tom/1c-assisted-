//! Полный цикл через OpenAiCompatible против локального фейкового сервера chat completions.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use onec_assistant::audit::AuditLog;
use onec_assistant::auth::{Role, UserIdentity};
use onec_assistant::config::{LlmBackend, LlmConfig, Secret};
use onec_assistant::llm::{LlmError, LlmProvider, Message, OpenAiCompatible};
use onec_assistant::onec::MockOneC;
use onec_assistant::orchestrator::{ChatRequest, Orchestrator, OrchestratorSettings};
use onec_assistant::tools::onec_registry;

type Seen = Arc<Mutex<Vec<Value>>>;

/// Первый запрос: модель просит search_counterparty; второй: отвечает текстом.
async fn completions(
    State(seen): State<Seen>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer test-key-123") {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let has_tool_result = body["messages"]
        .as_array()
        .is_some_and(|m| m.iter().any(|m| m["role"] == "tool"));
    seen.lock().unwrap().push(body);
    let message = if has_tool_result {
        json!({"role": "assistant", "content": "Нашёл АО «Вектор», ИНН 7803456789."})
    } else {
        json!({"role": "assistant", "content": null, "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": {"name": "search_counterparty", "arguments": "{\"query\":\"Вектор\"}"}
        }]})
    };
    Ok(Json(json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}]
    })))
}

async fn start(seen: Seen) -> String {
    let app = Router::new()
        .route("/v1/chat/completions", post(completions))
        .with_state(seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/v1")
}

fn llm_config(base_url: String, key: &str) -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::OpenAiCompatible,
        base_url,
        api_key: Some(Secret::new(key)),
        model: "qwen-test".into(),
        http_timeout: Duration::from_secs(5),
        temperature: 0.0,
    }
}

#[tokio::test]
async fn full_loop_over_http() {
    let seen: Seen = Arc::default();
    let base = start(seen.clone()).await;
    let llm = Arc::new(OpenAiCompatible::new(&llm_config(base, "test-key-123")).unwrap());
    let tools = Arc::new(onec_registry(Arc::new(MockOneC::new())).unwrap());
    let orch = Orchestrator::new(
        llm,
        tools,
        AuditLog::open_in_memory().unwrap(),
        OrchestratorSettings::default(),
    );

    let resp = orch
        .handle(ChatRequest {
            user: UserIdentity {
                user_id: "u".into(),
                role: Role::Viewer,
            },
            message: "Найди Вектор".into(),
            conversation_id: None,
        })
        .await
        .unwrap();
    assert!(resp.answer.starts_with("Нашёл АО «Вектор»"));
    assert_eq!(resp.sources[0].status, "ok");
    assert!(resp.warnings.is_empty(), "{:?}", resp.warnings);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0]["model"], "qwen-test");
    assert_eq!(
        seen[0]["tools"].as_array().unwrap().len(),
        1,
        "viewer видит одну функцию"
    );
    let second = seen[1]["messages"].as_array().unwrap();
    let tool_msg = second.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(tool_msg["tool_call_id"], "call_1");
    let payload: Value = serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(payload["data"]["candidates"][0]["inn"], "7803456789");
}

#[tokio::test]
async fn http_error_is_typed() {
    let base = start(Arc::default()).await;
    let llm = OpenAiCompatible::new(&llm_config(base, "wrong-key")).unwrap();
    let err = llm.chat(&[Message::user("x")], &[]).await.unwrap_err();
    assert!(
        matches!(err, LlmError::Status { status: 401, .. }),
        "{err:?}"
    );
}
