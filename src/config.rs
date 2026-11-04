//! Конфигурация из переменных окружения.
//!
//! Секреты хранятся в [`Secret`], у которого `Debug`/`Display` не раскрывают значение,
//! поэтому случайный `tracing::debug!(?config)` не утечёт пароль в лог.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

/// Строка-секрет. Значение доступно только через [`Secret::expose`].
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Явный доступ к значению. Используйте только там, где значение уходит в заголовок запроса.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("переменная окружения {name} не задана")]
    Missing { name: &'static str },
    #[error("переменная окружения {name} имеет неверное значение: {reason}")]
    Invalid { name: &'static str, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmBackend {
    OpenAiCompatible,
    YandexGpt,
    GigaChat,
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub backend: LlmBackend,
    /// Базовый URL, например `http://127.0.0.1:8080/v1` для llama-server.
    pub base_url: String,
    pub api_key: Option<Secret>,
    pub model: String,
    pub http_timeout: Duration,
    pub temperature: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneCBackend {
    Mock,
    Http,
}

/// Пути HTTP-сервиса 1С. Эндпоинты условные: контракт с 1С ещё не согласован,
/// поэтому вынесены в конфигурацию целиком.
#[derive(Debug, Clone)]
pub struct OneCPaths {
    pub search_counterparty: String,
    pub counterparty_balance: String,
    pub account_turnover: String,
    pub unpaid_invoices: String,
}

#[derive(Debug, Clone)]
pub struct OneCConfig {
    pub backend: OneCBackend,
    pub base_url: String,
    pub user: String,
    pub password: Secret,
    pub timeout: Duration,
    pub paths: OneCPaths,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Максимум обращений к модели за один запрос пользователя.
    pub max_iterations: usize,
    /// Максимум вызовов функций, исполняемых за одну итерацию. Остальные получают ошибку.
    pub max_tool_calls_per_iteration: usize,
    /// Общий таймаут обработки одного запроса.
    pub request_timeout: Duration,
    /// Таймаут одного вызова функции (обращения к 1С).
    pub tool_timeout: Duration,
    /// Максимальная длина вопроса в символах.
    pub max_message_chars: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_iterations: 5,
            max_tool_calls_per_iteration: 4,
            request_timeout: Duration::from_secs(60),
            tool_timeout: Duration::from_secs(10),
            max_message_chars: 4000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub users_file: PathBuf,
    pub audit_db_path: String,
    /// Хранить ли текст вопроса в журнале.
    pub log_questions: bool,
    pub llm: LlmConfig,
    pub onec: OneCConfig,
    pub limits: Limits,
}

impl Config {
    /// Читает конфигурацию из окружения процесса.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Читает конфигурацию через произвольную функцию поиска (удобно для тестов).
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let env = Env { lookup };
        let defaults = Limits::default();

        let llm_backend = match env.string_or("LLM_BACKEND", "openai_compatible").as_str() {
            "openai_compatible" => LlmBackend::OpenAiCompatible,
            "yandexgpt" => LlmBackend::YandexGpt,
            "gigachat" => LlmBackend::GigaChat,
            other => {
                return Err(ConfigError::Invalid {
                    name: "LLM_BACKEND",
                    reason: format!(
                        "ожидается openai_compatible | yandexgpt | gigachat, получено «{other}»"
                    ),
                })
            }
        };

        let onec_backend = match env.string_or("ONEC_BACKEND", "mock").as_str() {
            "mock" => OneCBackend::Mock,
            "http" => OneCBackend::Http,
            other => {
                return Err(ConfigError::Invalid {
                    name: "ONEC_BACKEND",
                    reason: format!("ожидается mock | http, получено «{other}»"),
                })
            }
        };

        let onec_timeout = Duration::from_secs(env.parse_or("ONEC_TIMEOUT_SECS", 10u64)?);

        let onec = OneCConfig {
            backend: onec_backend,
            base_url: match onec_backend {
                OneCBackend::Http => env.required("ONEC_BASE_URL")?,
                OneCBackend::Mock => env.string_or("ONEC_BASE_URL", ""),
            },
            user: env.string_or("ONEC_USER", ""),
            password: Secret::new(env.string_or("ONEC_PASSWORD", "")),
            timeout: onec_timeout,
            paths: OneCPaths {
                search_counterparty: env.string_or(
                    "ONEC_PATH_SEARCH_COUNTERPARTY",
                    "/hs/assistant/counterparties",
                ),
                counterparty_balance: env
                    .string_or("ONEC_PATH_COUNTERPARTY_BALANCE", "/hs/assistant/balance"),
                account_turnover: env
                    .string_or("ONEC_PATH_ACCOUNT_TURNOVER", "/hs/assistant/turnover"),
                unpaid_invoices: env
                    .string_or("ONEC_PATH_UNPAID_INVOICES", "/hs/assistant/unpaid-invoices"),
            },
        };

        let api_key = env.optional("LLM_API_KEY").map(Secret::new);
        let temperature: f32 = env.parse_or("LLM_TEMPERATURE", 0.0f32)?;

        let llm = LlmConfig {
            backend: llm_backend,
            base_url: env.string_or("LLM_BASE_URL", "http://127.0.0.1:8080/v1"),
            api_key,
            model: env.string_or("LLM_MODEL", "local-model"),
            http_timeout: Duration::from_secs(env.parse_or("LLM_TIMEOUT_SECS", 60u64)?),
            temperature,
        };

        let limits = Limits {
            max_iterations: env.parse_positive_or("MAX_ITERATIONS", defaults.max_iterations)?,
            max_tool_calls_per_iteration: env.parse_positive_or(
                "MAX_TOOL_CALLS_PER_ITERATION",
                defaults.max_tool_calls_per_iteration,
            )?,
            request_timeout: Duration::from_secs(
                env.parse_positive_or("REQUEST_TIMEOUT_SECS", defaults.request_timeout.as_secs())?,
            ),
            tool_timeout: onec_timeout,
            max_message_chars: env
                .parse_positive_or("MAX_MESSAGE_CHARS", defaults.max_message_chars)?,
        };

        Ok(Self {
            bind_addr: env.string_or("BIND_ADDR", "127.0.0.1:3000"),
            users_file: PathBuf::from(env.string_or("USERS_FILE", "users.toml")),
            audit_db_path: env.string_or("AUDIT_DB_PATH", "audit.sqlite"),
            log_questions: env.parse_bool_or("LOG_QUESTIONS", true)?,
            llm,
            onec,
            limits,
        })
    }

    /// Все непустые секреты конфигурации: журнал вырезает их из текста перед записью.
    pub fn secrets(&self) -> Vec<Secret> {
        let mut out = Vec::new();
        if !self.onec.password.is_empty() {
            out.push(self.onec.password.clone());
        }
        if let Some(key) = &self.llm.api_key {
            if !key.is_empty() {
                out.push(key.clone());
            }
        }
        out
    }
}

struct Env<F> {
    lookup: F,
}

impl<F> Env<F>
where
    F: Fn(&str) -> Option<String>,
{
    fn optional(&self, name: &str) -> Option<String> {
        (self.lookup)(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    fn string_or(&self, name: &str, default: &str) -> String {
        self.optional(name).unwrap_or_else(|| default.to_string())
    }

    fn required(&self, name: &'static str) -> Result<String, ConfigError> {
        self.optional(name).ok_or(ConfigError::Missing { name })
    }

    fn parse_or<T>(&self, name: &'static str, default: T) -> Result<T, ConfigError>
    where
        T: std::str::FromStr,
        T::Err: fmt::Display,
    {
        match self.optional(name) {
            None => Ok(default),
            Some(raw) => raw.parse::<T>().map_err(|e| ConfigError::Invalid {
                name,
                reason: e.to_string(),
            }),
        }
    }

    fn parse_positive_or<T>(&self, name: &'static str, default: T) -> Result<T, ConfigError>
    where
        T: std::str::FromStr + PartialOrd + Default,
        T::Err: fmt::Display,
    {
        let value = self.parse_or(name, default)?;
        if value <= T::default() {
            return Err(ConfigError::Invalid {
                name,
                reason: "значение должно быть больше нуля".into(),
            });
        }
        Ok(value)
    }

    fn parse_bool_or(&self, name: &'static str, default: bool) -> Result<bool, ConfigError> {
        match self.optional(name).map(|v| v.to_lowercase()) {
            None => Ok(default),
            Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Ok(true),
            Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Ok(false),
            Some(v) => Err(ConfigError::Invalid {
                name,
                reason: format!("ожидается true/false, получено «{v}»"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config_from(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults_are_applied() {
        let cfg = config_from(&[]).unwrap();
        assert_eq!(cfg.limits.max_iterations, 5);
        assert_eq!(cfg.onec.backend, OneCBackend::Mock);
        assert_eq!(cfg.llm.backend, LlmBackend::OpenAiCompatible);
        assert!(cfg.log_questions);
    }

    #[test]
    fn http_backend_requires_base_url() {
        let err = config_from(&[("ONEC_BACKEND", "http")]).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Missing {
                name: "ONEC_BASE_URL"
            }
        ));
    }

    #[test]
    fn secrets_are_not_printed_by_debug() {
        let cfg = config_from(&[
            ("ONEC_PASSWORD", "super-secret-1c"),
            ("LLM_API_KEY", "sk-very-secret"),
        ])
        .unwrap();
        let dump = format!("{cfg:?}");
        assert!(!dump.contains("super-secret-1c"));
        assert!(!dump.contains("sk-very-secret"));
        assert_eq!(cfg.secrets().len(), 2);
    }

    #[test]
    fn invalid_values_are_reported() {
        assert!(config_from(&[("MAX_ITERATIONS", "0")]).is_err());
        assert!(config_from(&[("LOG_QUESTIONS", "maybe")]).is_err());
        assert!(config_from(&[("LLM_BACKEND", "unknown")]).is_err());
        let cfg = config_from(&[("LOG_QUESTIONS", "false")]).unwrap();
        assert!(!cfg.log_questions);
    }
}
