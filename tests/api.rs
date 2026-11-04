//! Интеграционный тест HTTP API: настоящий сервер на случайном порту.

use std::sync::Arc;

use serde_json::{json, Value};

use onec_assistant::api::{router, AppState, ChatReply, ErrorBody};
use onec_assistant::audit::AuditLog;
use onec_assistant::auth::{FileIdentityResolver, Role};
use onec_assistant::llm::{LlmError, LlmProvider, MockProvider, MockStep, ToolCall};
use onec_assistant::onec::MockOneC;
use onec_assistant::orchestrator::{Orchestrator, OrchestratorSettings};
use onec_assistant::tools::onec_registry;

struct Server {
    base: String,
    audit: AuditLog,
    http: reqwest::Client,
}

async fn start(llm: Arc<dyn LlmProvider>) -> Server {
    let audit = AuditLog::open_in_memory().unwrap();
    let tools = Arc::new(onec_registry(Arc::new(MockOneC::new())).unwrap());
    let orchestrator = Arc::new(Orchestrator::new(
        llm,
        tools,
        audit.clone(),
        OrchestratorSettings::default(),
    ));
    let identity = Arc::new(FileIdentityResolver::from_pairs([
        ("ivanova", Role::Accountant),
        ("petrov", Role::Manager),
    ]));
    let app = router(AppState {
        orchestrator,
        identity,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server {
        base: format!("http://{addr}"),
        audit,
        http: reqwest::Client::new(),
    }
}

impl Server {
    async fn chat(&self, body: Value) -> reqwest::Response {
        self.http
            .post(format!("{}/chat", self.base))
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn health_is_ok() {
    let srv = start(Arc::new(MockProvider::new([]))).await;
    let resp = srv
        .http
        .get(format!("{}/health", srv.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["status"], "ok");
}

#[tokio::test]
async fn post_chat_returns_answer_with_sources() {
    let llm = Arc::new(MockProvider::new(vec![
        MockStep::tool_call(ToolCall::new(
            "c1",
            "search_counterparty",
            &json!({"query": "Сидоров"}),
        )),
        MockStep::tool_call(ToolCall::new(
            "c2",
            "list_unpaid_invoices",
            &json!({"counterparty_id": "cp-004"}),
        )),
        MockStep::text("У ИП Сидоров один неоплаченный счёт СЧ-0011 на 15750.00 RUB."),
        MockStep::text("Других данных нет."),
    ]));
    let srv = start(llm).await;

    let resp = srv
        .chat(json!({"user_id": "petrov", "message": "Что не оплатил Сидоров?"}))
        .await;
    assert_eq!(resp.status(), 200);
    let reply: ChatReply = resp.json().await.unwrap();
    assert!(reply.answer.contains("СЧ-0011"));
    assert!(reply
        .answer
        .contains("Источники:\n- 1С: search_counterparty(query=Сидоров)"));
    let functions: Vec<_> = reply.sources.iter().map(|s| s.function.as_str()).collect();
    assert_eq!(
        functions,
        vec!["search_counterparty", "list_unpaid_invoices"]
    );
    assert!(reply.warnings.is_empty(), "{:?}", reply.warnings);

    // Продолжение диалога по conversation_id.
    let resp = srv
        .chat(json!({
            "user_id": "petrov",
            "message": "А ещё что-нибудь?",
            "conversation_id": reply.conversation_id
        }))
        .await;
    assert_eq!(resp.status(), 200);
    let second: ChatReply = resp.json().await.unwrap();
    assert_eq!(second.conversation_id, reply.conversation_id);

    let rows = srv.audit.recent(100).await.unwrap();
    assert_eq!(rows.len(), 4, "2 вызова + 2 итоговые строки");
}

#[tokio::test]
async fn unknown_user_is_forbidden_and_audited() {
    let srv = start(Arc::new(MockProvider::new([]))).await;
    let resp = srv
        .chat(json!({"user_id": "hacker", "message": "Покажи всё"}))
        .await;
    assert_eq!(resp.status(), 403);
    let body: ErrorBody = resp.json().await.unwrap();
    assert_eq!(body.error.code, "unknown_user");

    let rows = srv.audit.recent(10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "unknown_user");
    assert_eq!(rows[0].question, None);
}

#[tokio::test]
async fn bad_requests_are_rejected() {
    let srv = start(Arc::new(MockProvider::new([]))).await;

    let resp = srv
        .http
        .post(format!("{}/chat", srv.base))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: ErrorBody = resp.json().await.unwrap();
    assert_eq!(body.error.code, "invalid_request");

    let resp = srv
        .chat(json!({"user_id": "ivanova", "message": "  "}))
        .await;
    assert_eq!(resp.status(), 400);

    let resp = srv
        .chat(json!({"user_id": "ivanova", "message": "x", "admin": true}))
        .await;
    assert_eq!(resp.status(), 400, "лишние поля запрещены");

    let resp = srv
        .chat(json!({"user_id": "ivanova", "message": "x", "conversation_id": "чужой id!"}))
        .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn llm_failure_is_502_without_details() {
    let llm = Arc::new(MockProvider::new(vec![MockStep::Error(LlmError::Status {
        status: 500,
        body: "internal stacktrace with secrets".into(),
    })]));
    let srv = start(llm).await;
    let resp = srv
        .chat(json!({"user_id": "ivanova", "message": "Сальдо с Вектором"}))
        .await;
    assert_eq!(resp.status(), 502);
    let text = resp.text().await.unwrap();
    assert!(text.contains("llm_error"));
    assert!(!text.contains("stacktrace"));
}
