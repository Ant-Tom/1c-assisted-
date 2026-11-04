//! Журнал в SQLite.
//!
//! На каждый запрос пишется итоговая строка (`function_name IS NULL`) и по строке
//! на каждый вызов функции; строки связаны `request_id`. Перед записью из всех
//! текстовых полей вырезаются секреты конфигурации.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use thiserror::Error;

use crate::config::Secret;

/// Аргументы и тексты ошибок длиннее этого обрезаются.
const MAX_TEXT_CHARS: usize = 2000;
/// Секреты короче этого не вырезаются: иначе пострадают обычные слова и числа.
const MIN_SECRET_CHARS: usize = 4;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("ошибка SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("фоновая задача журнала завершилась с ошибкой: {0}")]
    Join(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub ts: DateTime<Utc>,
    pub request_id: String,
    pub conversation_id: Option<String>,
    pub user_id: String,
    pub role: Option<String>,
    /// `None`, если хранение вопросов отключено (`LOG_QUESTIONS=false`).
    pub question: Option<String>,
    /// `None` для итоговой строки запроса.
    pub function_name: Option<String>,
    pub arguments: Option<String>,
    pub status: String,
    pub result_size: Option<i64>,
    pub model: Option<String>,
    pub duration_ms: i64,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct AuditLog {
    conn: Arc<Mutex<Connection>>,
    secrets: Arc<Vec<Secret>>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS audit_log (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts              TEXT    NOT NULL,
    request_id      TEXT    NOT NULL,
    conversation_id TEXT,
    user_id         TEXT    NOT NULL,
    role            TEXT,
    question        TEXT,
    function_name   TEXT,
    arguments       TEXT,
    status          TEXT    NOT NULL,
    result_size     INTEGER,
    model           TEXT,
    duration_ms     INTEGER NOT NULL,
    error           TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log (ts);
CREATE INDEX IF NOT EXISTS idx_audit_user ON audit_log (user_id, ts);
CREATE INDEX IF NOT EXISTS idx_audit_request ON audit_log (request_id);
";

impl AuditLog {
    pub fn open(path: &Path) -> Result<Self, AuditError> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, AuditError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, AuditError> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            secrets: Arc::new(Vec::new()),
        })
    }

    /// Значения, которые нельзя записывать в журнал (пароль 1С, ключ модели).
    pub fn with_secrets(mut self, secrets: Vec<Secret>) -> Self {
        self.secrets = Arc::new(
            secrets
                .into_iter()
                .filter(|s| s.expose().chars().count() >= MIN_SECRET_CHARS)
                .collect(),
        );
        self
    }

    /// Пишет записи одной транзакцией.
    pub async fn write(&self, records: Vec<AuditRecord>) -> Result<(), AuditError> {
        let records: Vec<AuditRecord> = records.into_iter().map(|r| self.sanitize(r)).collect();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().unwrap_or_else(PoisonError::into_inner);
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO audit_log (ts, request_id, conversation_id, user_id, role, \
                     question, function_name, arguments, status, result_size, model, \
                     duration_ms, error) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                )?;
                for r in &records {
                    stmt.execute(params![
                        r.ts.to_rfc3339(),
                        r.request_id,
                        r.conversation_id,
                        r.user_id,
                        r.role,
                        r.question,
                        r.function_name,
                        r.arguments,
                        r.status,
                        r.result_size,
                        r.model,
                        r.duration_ms,
                        r.error,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| AuditError::Join(e.to_string()))?
    }

    /// Последние записи (новые в конце). Для тестов и диагностики.
    pub async fn recent(&self, limit: usize) -> Result<Vec<AuditRecord>, AuditError> {
        let conn = self.conn.clone();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap_or_else(PoisonError::into_inner);
            let mut stmt = conn.prepare(
                "SELECT ts, request_id, conversation_id, user_id, role, question, function_name, \
                 arguments, status, result_size, model, duration_ms, error \
                 FROM (SELECT * FROM audit_log ORDER BY id DESC LIMIT ?1) ORDER BY id ASC",
            )?;
            let rows = stmt.query_map([limit], |row| {
                let ts: String = row.get(0)?;
                let ts = DateTime::parse_from_rfc3339(&ts)
                    .map(|t| t.with_timezone(&Utc))
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                Ok(AuditRecord {
                    ts,
                    request_id: row.get(1)?,
                    conversation_id: row.get(2)?,
                    user_id: row.get(3)?,
                    role: row.get(4)?,
                    question: row.get(5)?,
                    function_name: row.get(6)?,
                    arguments: row.get(7)?,
                    status: row.get(8)?,
                    result_size: row.get(9)?,
                    model: row.get(10)?,
                    duration_ms: row.get(11)?,
                    error: row.get(12)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(AuditError::from)
        })
        .await
        .map_err(|e| AuditError::Join(e.to_string()))?
    }

    fn sanitize(&self, mut r: AuditRecord) -> AuditRecord {
        let clean = |text: Option<String>| text.map(|t| truncate(&self.redact(&t)));
        r.question = clean(r.question);
        r.arguments = clean(r.arguments);
        r.error = clean(r.error);
        r.user_id = self.redact(&r.user_id);
        r
    }

    fn redact(&self, text: &str) -> String {
        self.secrets
            .iter()
            .fold(text.to_string(), |acc, s| acc.replace(s.expose(), "***"))
    }
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT_CHARS {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(MAX_TEXT_CHARS).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(question: &str) -> AuditRecord {
        AuditRecord {
            ts: Utc::now(),
            request_id: "r1".into(),
            conversation_id: Some("c1".into()),
            user_id: "ivanova".into(),
            role: Some("accountant".into()),
            question: Some(question.into()),
            function_name: None,
            arguments: Some(format!("{{\"q\":\"{question}\"}}")),
            status: "ok".into(),
            result_size: Some(10),
            model: Some("m".into()),
            duration_ms: 5,
            error: None,
        }
    }

    #[tokio::test]
    async fn writes_and_reads_back() {
        let log = AuditLog::open_in_memory().unwrap();
        log.write(vec![record("вопрос 1"), record("вопрос 2")])
            .await
            .unwrap();
        let rows = log.recent(10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].question.as_deref(), Some("вопрос 1"));
        assert_eq!(
            log.recent(1).await.unwrap()[0].question.as_deref(),
            Some("вопрос 2")
        );
    }

    #[tokio::test]
    async fn redacts_secrets_and_truncates() {
        let log = AuditLog::open_in_memory()
            .unwrap()
            .with_secrets(vec![Secret::new("pa$$word-1c"), Secret::new("ab")]);
        let long = "x".repeat(MAX_TEXT_CHARS + 10);
        log.write(vec![record("пароль pa$$word-1c"), record(&long)])
            .await
            .unwrap();
        let rows = log.recent(10).await.unwrap();
        assert_eq!(rows[0].question.as_deref(), Some("пароль ***"));
        assert!(!rows[0]
            .arguments
            .as_deref()
            .unwrap()
            .contains("pa$$word-1c"));
        // Короткие «секреты» не вырезаются, чтобы не портить текст.
        assert!(rows[1].question.as_deref().unwrap().ends_with('…'));
    }
}
