//! Клиент HTTP-сервиса 1С.
//!
//! TODO(1c-contract): эндпоинты, имена параметров и формат ответов условные.
//! Ожидаемый сейчас контракт (GET, параметры в query string, ответ JSON):
//! - search_counterparty:   `?query=...`                          → `[Counterparty]`
//! - counterparty_balance:  `?counterparty_id=...&date=YYYY-MM-DD` → `CounterpartyBalance`
//! - account_turnover:      `?account=...&date_from=...&date_to=...` → `AccountTurnover`
//! - unpaid_invoices:       `?counterparty_id=...`                 → `[Invoice]`
//!
//! Согласовать с разработчиком HTTP-сервиса 1С и поправить типы/пути.

use async_trait::async_trait;
use chrono::NaiveDate;
use serde::de::DeserializeOwned;

use super::{AccountTurnover, Counterparty, CounterpartyBalance, Invoice, OneCClient, OneCError};
use crate::config::{OneCConfig, OneCPaths, Secret};

/// Ответы больше этого размера считаются ошибкой: защита от случайной выгрузки всей базы.
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

pub struct HttpOneC {
    client: reqwest::Client,
    base_url: String,
    user: String,
    password: Secret,
    paths: OneCPaths,
}

impl HttpOneC {
    pub fn new(config: &OneCConfig) -> Result<Self, OneCError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .connect_timeout(config.timeout)
            .build()
            .map_err(|e| OneCError::Transport(e.without_url().to_string()))?;
        Ok(Self {
            client,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            user: config.user.clone(),
            password: config.password.clone(),
            paths: config.paths.clone(),
        })
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, OneCError> {
        let url = format!("{}{}", self.base_url, path);
        let mut request = self.client.get(&url).query(query);
        if !self.user.is_empty() {
            request = request.basic_auth(&self.user, Some(self.password.expose()));
        }
        let response = request.send().await.map_err(map_reqwest_error)?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(OneCError::NotFound(format!("ресурс {path}")));
        }
        if !status.is_success() {
            return Err(OneCError::Status(status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(OneCError::InvalidResponse("слишком большой ответ".into()));
        }
        let body = response.bytes().await.map_err(map_reqwest_error)?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(OneCError::InvalidResponse("слишком большой ответ".into()));
        }
        serde_json::from_slice(&body).map_err(|e| OneCError::InvalidResponse(e.to_string()))
    }
}

fn map_reqwest_error(err: reqwest::Error) -> OneCError {
    if err.is_timeout() {
        OneCError::Timeout
    } else {
        // without_url: в URL могут быть параметры запроса, в ошибках они не нужны.
        OneCError::Transport(err.without_url().to_string())
    }
}

#[async_trait]
impl OneCClient for HttpOneC {
    async fn search_counterparty(&self, query: &str) -> Result<Vec<Counterparty>, OneCError> {
        self.get(&self.paths.search_counterparty, &[("query", query)])
            .await
    }

    async fn get_counterparty_balance(
        &self,
        counterparty_id: &str,
        date: NaiveDate,
    ) -> Result<CounterpartyBalance, OneCError> {
        let date = date.to_string();
        self.get(
            &self.paths.counterparty_balance,
            &[("counterparty_id", counterparty_id), ("date", &date)],
        )
        .await
    }

    async fn get_account_turnover(
        &self,
        account: &str,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<AccountTurnover, OneCError> {
        let (from, to) = (date_from.to_string(), date_to.to_string());
        self.get(
            &self.paths.account_turnover,
            &[("account", account), ("date_from", &from), ("date_to", &to)],
        )
        .await
    }

    async fn list_unpaid_invoices(&self, counterparty_id: &str) -> Result<Vec<Invoice>, OneCError> {
        self.get(
            &self.paths.unpaid_invoices,
            &[("counterparty_id", counterparty_id)],
        )
        .await
    }
}
