//! Журнал: записи создаются на каждый запрос и вызов, секреты в файл не попадают.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use onec_assistant::audit::AuditLog;
use onec_assistant::auth::{Role, UserIdentity};
use onec_assistant::config::{Config, Limits};
use onec_assistant::llm::{MockProvider, MockStep, Reply, ToolCall};
use onec_assistant::onec::MockOneC;
use onec_assistant::orchestrator::{ChatRequest, Orchestrator, OrchestratorSettings};
use onec_assistant::tools::onec_registry;

const ONEC_PASSWORD: &str = "OneC-Pa55-secret";
const LLM_KEY: &str = "sk-live-ABCDEF123456";

struct TempDb(PathBuf);

impl TempDb {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("audit-test-{}.sqlite", uuid::Uuid::new_v4())))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn config() -> Config {
    let env: HashMap<&str, &str> = [("ONEC_PASSWORD", ONEC_PASSWORD), ("LLM_API_KEY", LLM_KEY)]
        .into_iter()
        .collect();
    Config::from_lookup(|k| env.get(k).map(|v| v.to_string())).unwrap()
}

fn orchestrator(llm: MockProvider, audit: AuditLog, log_questions: bool) -> Orchestrator {
    let tools = Arc::new(onec_registry(Arc::new(MockOneC::new())).unwrap());
    Orchestrator::new(
        Arc::new(llm),
        tools,
        audit,
        OrchestratorSettings {
            limits: Limits {
                max_iterations: 3,
                ..Limits::default()
            },
            log_questions,
        },
    )
}

fn request(message: &str) -> ChatRequest {
    ChatRequest {
        user: UserIdentity {
            user_id: "ivanova".into(),
            role: Role::Accountant,
        },
        message: message.into(),
        conversation_id: None,
    }
}

#[tokio::test]
async fn audit_rows_are_written_without_secrets() {
    let db = TempDb::new();
    let cfg = config();
    let audit = AuditLog::open(&db.0).unwrap().with_secrets(cfg.secrets());

    let llm = MockProvider::new(vec![
        // Модель «утащила» секрет в аргументы: в журнале его быть не должно.
        MockStep::tool_call(ToolCall::new(
            "c1",
            "search_counterparty",
            &json!({"query": LLM_KEY}),
        )),
        MockStep::tool_call(ToolCall::new(
            "c2",
            "get_counterparty_balance",
            &json!({"counterparty_id": "cp-003", "date": "2026-11-01"}),
        )),
        MockStep::text("Сальдо с АО «Вектор» на 2026-11-01: 0.00 RUB."),
    ]);
    let orch = orchestrator(llm, audit.clone(), true);

    let resp = orch
        .handle(request(&format!(
            "Мой пароль {ONEC_PASSWORD}. Какое сальдо с Вектором?"
        )))
        .await
        .unwrap();

    let rows = audit.recent(100).await.unwrap();
    assert_eq!(rows.len(), 3, "два вызова и итоговая строка");
    assert!(rows.iter().all(|r| r.request_id == resp.request_id));
    assert!(rows
        .iter()
        .all(|r| r.conversation_id.as_deref() == Some(resp.conversation_id.as_str())));
    assert!(rows.iter().all(|r| r.user_id == "ivanova"));
    assert!(rows.iter().all(|r| r.role.as_deref() == Some("accountant")));
    assert!(rows
        .iter()
        .all(|r| r.model.as_deref() == Some("mock-model")));

    let balance = &rows[1];
    assert_eq!(
        balance.function_name.as_deref(),
        Some("get_counterparty_balance")
    );
    assert_eq!(balance.status, "ok");
    assert!(balance.result_size.unwrap() > 0);
    assert!(balance.arguments.as_deref().unwrap().contains("cp-003"));

    let summary = &rows[2];
    assert_eq!(summary.function_name, None);
    assert_eq!(summary.status, "ok");
    assert_eq!(
        summary.question.as_deref(),
        Some("Мой пароль ***. Какое сальдо с Вектором?")
    );
    assert!(summary.duration_ms >= 0);

    // Проверяем сам файл базы, а не только то, что вернул SELECT.
    let raw = std::fs::read(&db.0).unwrap();
    for secret in [ONEC_PASSWORD, LLM_KEY] {
        assert!(
            !raw.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "секрет «{secret}» найден в файле журнала"
        );
    }
}

#[tokio::test]
async fn questions_are_not_stored_when_disabled() {
    let audit = AuditLog::open_in_memory().unwrap();
    let orch = orchestrator(
        MockProvider::new(vec![MockStep::text("Здравствуйте!")]),
        audit.clone(),
        false,
    );
    orch.handle(request("Личный вопрос")).await.unwrap();
    let rows = audit.recent(10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].question, None);
    assert_eq!(rows[0].status, "ok");
}

#[tokio::test]
async fn failed_requests_are_audited() {
    let audit = AuditLog::open_in_memory().unwrap();
    let orch = orchestrator(
        MockProvider::repeating(Reply::ToolCalls(vec![ToolCall::new(
            "c",
            "search_counterparty",
            &json!({"query": "Вектор"}),
        )])),
        audit.clone(),
        true,
    );
    assert!(orch.handle(request("Зациклись")).await.is_err());
    assert!(orch.handle(request("  ")).await.is_err());

    let rows = audit.recent(10).await.unwrap();
    let summaries: Vec<_> = rows
        .iter()
        .filter(|r| r.function_name.is_none())
        .map(|r| r.status.as_str())
        .collect();
    assert_eq!(summaries, vec!["iteration_limit", "empty_message"]);
    let calls: Vec<_> = rows
        .iter()
        .filter(|r| r.function_name.is_some())
        .map(|r| r.status.as_str())
        .collect();
    assert_eq!(calls, vec!["ok", "ok", "not_executed"]);
    assert!(rows[3].error.as_deref().unwrap().contains("3 итераций"));
}
