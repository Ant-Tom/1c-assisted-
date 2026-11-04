//! Клиент 1С: только чтение. Функций записи в MVP нет и быть не должно.

use std::fmt;

use async_trait::async_trait;
use chrono::NaiveDate;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

pub mod http;
pub mod mock;

pub use http::HttpOneC;
pub use mock::MockOneC;

/// Денежная сумма в копейках. Наружу сериализуется строкой `"1234.56"`,
/// чтобы не терять точность на f64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Money(pub i64);

impl Money {
    pub fn from_kopecks(kopecks: i64) -> Self {
        Self(kopecks)
    }

    pub fn kopecks(self) -> i64 {
        self.0
    }

    /// Разбирает десятичную запись: `"1234"`, `"1234.5"`, `"-1234,56"`.
    /// Больше двух знаков после запятой не допускается.
    pub fn parse_decimal(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let (negative, digits) = match raw.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        let (int_part, frac_part) = match digits.split_once(['.', ',']) {
            Some((i, f)) => (i, f),
            None => (digits, ""),
        };
        if int_part.is_empty()
            || !int_part.chars().all(|c| c.is_ascii_digit())
            || !frac_part.chars().all(|c| c.is_ascii_digit())
            || frac_part.len() > 2
        {
            return None;
        }
        let int: i64 = int_part.parse().ok()?;
        let frac: i64 = match frac_part.len() {
            0 => 0,
            1 => frac_part.parse::<i64>().ok()? * 10,
            _ => frac_part.parse().ok()?,
        };
        let value = int.checked_mul(100)?.checked_add(frac)?;
        Some(Self(if negative { -value } else { value }))
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        write!(f, "{sign}{}.{:02}", abs / 100, abs % 100)
    }
}

impl Serialize for Money {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Money {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Принимаем и строку, и число: формат ответа HTTP-сервиса 1С ещё не согласован.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Int(i64),
            Float(f64),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Text(s) => Money::parse_decimal(&s)
                .ok_or_else(|| de::Error::custom(format!("неверная сумма: {s}"))),
            Raw::Int(i) => i
                .checked_mul(100)
                .map(Money)
                .ok_or_else(|| de::Error::custom("сумма вне диапазона")),
            Raw::Float(f) if f.is_finite() && f.abs() < 9.0e15 => {
                // Округление до копеек. Допустимо как временная мера, пока 1С отдаёт числа.
                #[allow(clippy::cast_possible_truncation)]
                let kopecks = (f * 100.0).round() as i64;
                Ok(Money(kopecks))
            }
            Raw::Float(_) => Err(de::Error::custom("сумма вне диапазона")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counterparty {
    pub id: String,
    pub name: String,
    pub inn: String,
}

/// Сальдо взаиморасчётов с контрагентом на дату.
/// Положительное `balance`: контрагент должен нам, отрицательное: мы должны контрагенту.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterpartyBalance {
    pub counterparty_id: String,
    pub counterparty_name: String,
    pub date: NaiveDate,
    pub balance: Money,
    pub currency: String,
}

/// Обороты по счёту бухгалтерского учёта за период.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountTurnover {
    pub account: String,
    pub date_from: NaiveDate,
    pub date_to: NaiveDate,
    pub opening_balance: Money,
    pub debit_turnover: Money,
    pub credit_turnover: Money,
    pub closing_balance: Money,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invoice {
    pub number: String,
    pub date: NaiveDate,
    pub due_date: Option<NaiveDate>,
    pub amount: Money,
    pub paid: Money,
    pub outstanding: Money,
    pub currency: String,
}

#[derive(Debug, Error)]
pub enum OneCError {
    #[error("1С не ответила за отведённое время")]
    Timeout,
    #[error("не найдено: {0}")]
    NotFound(String),
    #[error("ошибка соединения с 1С: {0}")]
    Transport(String),
    #[error("1С вернула HTTP {0}")]
    Status(u16),
    #[error("неожиданный ответ 1С: {0}")]
    InvalidResponse(String),
}

/// Клиент 1С. Все методы только читают данные.
#[async_trait]
pub trait OneCClient: Send + Sync {
    async fn search_counterparty(&self, query: &str) -> Result<Vec<Counterparty>, OneCError>;

    async fn get_counterparty_balance(
        &self,
        counterparty_id: &str,
        date: NaiveDate,
    ) -> Result<CounterpartyBalance, OneCError>;

    async fn get_account_turnover(
        &self,
        account: &str,
        date_from: NaiveDate,
        date_to: NaiveDate,
    ) -> Result<AccountTurnover, OneCError>;

    async fn list_unpaid_invoices(&self, counterparty_id: &str) -> Result<Vec<Invoice>, OneCError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_formats_and_parses() {
        assert_eq!(Money(123456).to_string(), "1234.56");
        assert_eq!(Money(-5).to_string(), "-0.05");
        assert_eq!(Money::parse_decimal("1234.56"), Some(Money(123456)));
        assert_eq!(Money::parse_decimal("1234,5"), Some(Money(123450)));
        assert_eq!(Money::parse_decimal("-7"), Some(Money(-700)));
        assert_eq!(Money::parse_decimal("1.234"), None);
        assert_eq!(Money::parse_decimal("abc"), None);
        assert_eq!(Money::parse_decimal(""), None);
    }

    #[test]
    fn money_serde_roundtrip() {
        let json = serde_json::to_string(&Money(100050)).unwrap();
        assert_eq!(json, "\"1000.50\"");
        let back: Money = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Money(100050));
        let from_float: Money = serde_json::from_str("1000.5").unwrap();
        assert_eq!(from_float, Money(100050));
        let from_int: Money = serde_json::from_str("10").unwrap();
        assert_eq!(from_int, Money(1000));
    }
}
