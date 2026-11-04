//! HttpOneC против локального тестового HTTP-сервера.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use chrono::NaiveDate;
use serde_json::{json, Value};

use onec_assistant::config::{OneCBackend, OneCConfig, OneCPaths, Secret};
use onec_assistant::onec::{HttpOneC, Money, OneCClient, OneCError};

const EXPECTED_AUTH: &str = "Basic YXNzaXN0YW50X3JvOnMzY3JldC1wYXNz"; // assistant_ro:s3cret-pass

fn authorized(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(EXPECTED_AUTH)
}

async fn search(
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let query = q.get("query").cloned().unwrap_or_default();
    Ok(Json(json!([
        {"id": "x-1", "name": format!("{query} (тест)"), "inn": "7700000001"}
    ])))
}

async fn balance(Query(q): Query<HashMap<String, String>>) -> Result<Json<Value>, StatusCode> {
    match q.get("counterparty_id").map(String::as_str) {
        Some("x-1") => Ok(Json(json!({
            "counterparty_id": "x-1",
            "counterparty_name": "Тест",
            "date": q.get("date").cloned().unwrap_or_default(),
            "balance": 1500.25,
            "currency": "RUB"
        }))),
        _ => Err(StatusCode::NOT_FOUND),
    }
}

async fn slow() -> Json<Value> {
    tokio::time::sleep(Duration::from_secs(5)).await;
    Json(json!([]))
}

async fn start_server() -> String {
    let app = Router::new()
        .route("/hs/search", get(search))
        .route("/hs/balance", get(balance))
        .route("/hs/slow", get(slow));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn config(base_url: String, password: &str) -> OneCConfig {
    OneCConfig {
        backend: OneCBackend::Http,
        base_url,
        user: "assistant_ro".into(),
        password: Secret::new(password),
        timeout: Duration::from_millis(300),
        paths: OneCPaths {
            search_counterparty: "/hs/search".into(),
            counterparty_balance: "/hs/balance".into(),
            account_turnover: "/hs/slow".into(),
            unpaid_invoices: "/hs/slow".into(),
        },
    }
}

#[tokio::test]
async fn sends_basic_auth_and_parses_response() {
    let base = start_server().await;
    let client = HttpOneC::new(&config(base, "s3cret-pass")).unwrap();
    let found = client.search_counterparty("Ромашка").await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Ромашка (тест)");

    let date = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
    let bal = client.get_counterparty_balance("x-1", date).await.unwrap();
    assert_eq!(bal.balance, Money(150_025));
    assert!(matches!(
        client.get_counterparty_balance("nope", date).await,
        Err(OneCError::NotFound(_))
    ));
}

#[tokio::test]
async fn wrong_password_is_status_error_without_secret() {
    let base = start_server().await;
    let client = HttpOneC::new(&config(base, "wrong-password-value")).unwrap();
    let err = client.search_counterparty("Ромашка").await.unwrap_err();
    assert!(matches!(err, OneCError::Status(401)));
    assert!(!err.to_string().contains("wrong-password-value"));
}

#[tokio::test]
async fn slow_1c_maps_to_timeout() {
    let base = start_server().await;
    let client = HttpOneC::new(&config(base, "s3cret-pass")).unwrap();
    let from = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
    let err = client
        .get_account_turnover("51", from, from)
        .await
        .unwrap_err();
    assert!(matches!(err, OneCError::Timeout), "{err:?}");
}
