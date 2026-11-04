//! Четыре функции чтения 1С. Функций записи нет.
//!
//! Итоговые суммы (например, общий долг по счетам) считает код, а не модель.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::NaiveDate;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{RegistryError, ToolDef, ToolError, ToolHandler, ToolRegistry};
use crate::auth::Role;
use crate::onec::{Money, OneCClient};

pub const SEARCH_COUNTERPARTY: &str = "search_counterparty";
pub const GET_COUNTERPARTY_BALANCE: &str = "get_counterparty_balance";
pub const GET_ACCOUNT_TURNOVER: &str = "get_account_turnover";
pub const LIST_UNPAID_INVOICES: &str = "list_unpaid_invoices";

const ALL_ROLES: &[Role] = &[Role::Viewer, Role::Manager, Role::Accountant];
const MANAGER_AND_UP: &[Role] = &[Role::Manager, Role::Accountant];
const ACCOUNTANT_ONLY: &[Role] = &[Role::Accountant];

/// Реестр со всеми функциями 1С.
pub fn onec_registry(client: Arc<dyn OneCClient>) -> Result<ToolRegistry, RegistryError> {
    let mut registry = ToolRegistry::new();

    registry.register(ToolDef {
        name: SEARCH_COUNTERPARTY,
        description: "Поиск контрагента в 1С по названию или ИНН. Возвращает список кандидатов \
                      с id и ИНН. Если кандидатов больше одного (ambiguous = true), \
                      не выбирай сам: покажи кандидатов пользователю и попроси уточнить.",
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 2,
                    "maxLength": 100,
                    "description": "Название контрагента (можно часть) или ИНН"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        allowed_roles: ALL_ROLES,
        handler: Arc::new(SearchCounterparty(client.clone())),
    })?;

    registry.register(ToolDef {
        name: GET_COUNTERPARTY_BALANCE,
        description: "Сальдо взаиморасчётов с контрагентом на дату. Положительное значение \
                      balance: контрагент должен нам; отрицательное: мы должны контрагенту. \
                      counterparty_id бери только из результата search_counterparty.",
        parameters: json!({
            "type": "object",
            "properties": {
                "counterparty_id": {"type": "string", "minLength": 1, "maxLength": 64,
                                    "description": "id контрагента из search_counterparty"},
                "date": {"type": "string", "format": "date",
                         "description": "Дата в формате YYYY-MM-DD"}
            },
            "required": ["counterparty_id", "date"],
            "additionalProperties": false
        }),
        allowed_roles: ACCOUNTANT_ONLY,
        handler: Arc::new(CounterpartyBalanceTool(client.clone())),
    })?;

    registry.register(ToolDef {
        name: GET_ACCOUNT_TURNOVER,
        description: "Обороты по счёту бухгалтерского учёта (например 62.01, 60.01, 51) \
                      за период: сальдо на начало, обороты по дебету и кредиту, сальдо на конец.",
        parameters: json!({
            "type": "object",
            "properties": {
                "account": {"type": "string", "minLength": 2, "maxLength": 10,
                            "description": "Номер счёта по плану счетов, например 62.01"},
                "date_from": {"type": "string", "format": "date",
                              "description": "Начало периода, YYYY-MM-DD"},
                "date_to": {"type": "string", "format": "date",
                            "description": "Конец периода включительно, YYYY-MM-DD"}
            },
            "required": ["account", "date_from", "date_to"],
            "additionalProperties": false
        }),
        allowed_roles: ACCOUNTANT_ONLY,
        handler: Arc::new(AccountTurnoverTool(client.clone())),
    })?;

    registry.register(ToolDef {
        name: LIST_UNPAID_INVOICES,
        description: "Неоплаченные (полностью или частично) счета контрагента с суммами \
                      и общим остатком к оплате. counterparty_id бери только из результата \
                      search_counterparty.",
        parameters: json!({
            "type": "object",
            "properties": {
                "counterparty_id": {"type": "string", "minLength": 1, "maxLength": 64,
                                    "description": "id контрагента из search_counterparty"}
            },
            "required": ["counterparty_id"],
            "additionalProperties": false
        }),
        allowed_roles: MANAGER_AND_UP,
        handler: Arc::new(UnpaidInvoices(client)),
    })?;

    Ok(registry)
}

fn typed<T: DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::InvalidArguments(e.to_string()))
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<Value, ToolError> {
    serde_json::to_value(value).map_err(|e| ToolError::Backend(e.to_string()))
}

struct SearchCounterparty(Arc<dyn OneCClient>);

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
}

#[async_trait]
impl ToolHandler for SearchCounterparty {
    async fn call(&self, args: Value) -> Result<Value, ToolError> {
        let args: SearchArgs = typed(args)?;
        let query = args.query.trim();
        if query.chars().count() < 2 {
            return Err(ToolError::InvalidArguments(
                "query: минимальная длина 2 без учёта пробелов".into(),
            ));
        }
        let found = self.0.search_counterparty(query).await?;
        Ok(json!({
            "query": query,
            "count": found.len(),
            "ambiguous": found.len() > 1,
            "candidates": to_json(&found)?,
        }))
    }
}

struct CounterpartyBalanceTool(Arc<dyn OneCClient>);

#[derive(Deserialize)]
struct BalanceArgs {
    counterparty_id: String,
    date: NaiveDate,
}

#[async_trait]
impl ToolHandler for CounterpartyBalanceTool {
    async fn call(&self, args: Value) -> Result<Value, ToolError> {
        let args: BalanceArgs = typed(args)?;
        let balance = self
            .0
            .get_counterparty_balance(args.counterparty_id.trim(), args.date)
            .await?;
        to_json(&balance)
    }
}

struct AccountTurnoverTool(Arc<dyn OneCClient>);

#[derive(Deserialize)]
struct TurnoverArgs {
    account: String,
    date_from: NaiveDate,
    date_to: NaiveDate,
}

#[async_trait]
impl ToolHandler for AccountTurnoverTool {
    async fn call(&self, args: Value) -> Result<Value, ToolError> {
        let args: TurnoverArgs = typed(args)?;
        if args.date_from > args.date_to {
            return Err(ToolError::InvalidArguments(
                "date_from не может быть позже date_to".into(),
            ));
        }
        let turnover = self
            .0
            .get_account_turnover(args.account.trim(), args.date_from, args.date_to)
            .await?;
        to_json(&turnover)
    }
}

struct UnpaidInvoices(Arc<dyn OneCClient>);

#[derive(Deserialize)]
struct UnpaidArgs {
    counterparty_id: String,
}

#[async_trait]
impl ToolHandler for UnpaidInvoices {
    async fn call(&self, args: Value) -> Result<Value, ToolError> {
        let args: UnpaidArgs = typed(args)?;
        let invoices = self
            .0
            .list_unpaid_invoices(args.counterparty_id.trim())
            .await?;
        let total = invoices
            .iter()
            .try_fold(0i64, |acc, inv| acc.checked_add(inv.outstanding.kopecks()))
            .ok_or_else(|| ToolError::Backend("переполнение при сложении сумм".into()))?;
        Ok(json!({
            "counterparty_id": args.counterparty_id.trim(),
            "count": invoices.len(),
            "total_outstanding": Money(total),
            "currency": "RUB",
            "invoices": to_json(&invoices)?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::onec::mock::MockOneC;

    fn registry() -> (ToolRegistry, Arc<MockOneC>) {
        let onec = Arc::new(MockOneC::new());
        (onec_registry(onec.clone()).unwrap(), onec)
    }

    #[test]
    fn specs_depend_on_role() {
        let (reg, _) = registry();
        let names = |role| {
            reg.specs_for(role)
                .into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(Role::Viewer), vec![SEARCH_COUNTERPARTY]);
        assert_eq!(
            names(Role::Manager),
            vec![SEARCH_COUNTERPARTY, LIST_UNPAID_INVOICES]
        );
        assert_eq!(names(Role::Accountant).len(), 4);
    }

    #[tokio::test]
    async fn executes_valid_call() {
        let (reg, _) = registry();
        let call = ToolCall::new(
            "1",
            GET_COUNTERPARTY_BALANCE,
            &json!({"counterparty_id": "cp-001", "date": "2026-11-01"}),
        );
        let out = reg.execute(Role::Accountant, &call).await.unwrap();
        assert_eq!(out["balance"], "125000.50");
    }

    #[tokio::test]
    async fn unpaid_total_is_computed_by_code() {
        let (reg, _) = registry();
        let call = ToolCall::new(
            "1",
            LIST_UNPAID_INVOICES,
            &json!({"counterparty_id": "cp-001"}),
        );
        let out = reg.execute(Role::Manager, &call).await.unwrap();
        assert_eq!(out["count"], 2);
        assert_eq!(out["total_outstanding"], "125000.50");
    }

    #[tokio::test]
    async fn rejects_forbidden_unknown_and_invalid() {
        let (reg, onec) = registry();
        let forbidden = ToolCall::new(
            "1",
            GET_ACCOUNT_TURNOVER,
            &json!({"account": "51", "date_from": "2026-01-01", "date_to": "2026-01-31"}),
        );
        let err = reg.execute(Role::Manager, &forbidden).await.unwrap_err();
        assert_eq!(err.code(), "forbidden");

        let unknown = ToolCall::new("2", "delete_everything", &json!({}));
        let err = reg.execute(Role::Accountant, &unknown).await.unwrap_err();
        assert_eq!(err.code(), "unknown_tool");

        let bad_json = ToolCall::raw("3", SEARCH_COUNTERPARTY, "{query: ромашка");
        let err = reg.execute(Role::Viewer, &bad_json).await.unwrap_err();
        assert_eq!(err.code(), "invalid_arguments");

        let bad_date = ToolCall::new(
            "4",
            GET_COUNTERPARTY_BALANCE,
            &json!({"counterparty_id": "cp-001", "date": "01.11.2026"}),
        );
        let err = reg.execute(Role::Accountant, &bad_date).await.unwrap_err();
        assert_eq!(err.code(), "invalid_arguments");

        let reversed = ToolCall::new(
            "5",
            GET_ACCOUNT_TURNOVER,
            &json!({"account": "51", "date_from": "2026-02-01", "date_to": "2026-01-01"}),
        );
        let err = reg.execute(Role::Accountant, &reversed).await.unwrap_err();
        assert_eq!(err.code(), "invalid_arguments");

        // Ни один отклонённый вызов не дошёл до 1С.
        assert_eq!(onec.calls(), 0);
    }

    #[tokio::test]
    async fn search_marks_ambiguity() {
        let (reg, _) = registry();
        let call = ToolCall::new("1", SEARCH_COUNTERPARTY, &json!({"query": "Ромашка"}));
        let out = reg.execute(Role::Viewer, &call).await.unwrap();
        assert_eq!(out["ambiguous"], true);
        assert_eq!(out["count"], 2);
    }

    #[test]
    fn duplicate_registration_fails() {
        let (mut reg, onec) = registry();
        let dup = ToolDef {
            name: SEARCH_COUNTERPARTY,
            description: "",
            parameters: json!({}),
            allowed_roles: ALL_ROLES,
            handler: Arc::new(SearchCounterparty(onec)),
        };
        assert!(reg.register(dup).is_err());
    }
}
