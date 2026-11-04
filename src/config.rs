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

/// Допустимые значения scope для получения токена GigaChat.
pub const GIGACHAT_SCOPES: &[&str] =
    &["GIGACHAT_API_PERS", "GIGACHAT_API_B2B", "GIGACHAT_API_CORP"];

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub backend: LlmBackend,
    /// Базовый URL API:
    /// - openai_compatible: до `/chat/completions`, например `http://127.0.0.1:8080/v1`;
    /// - yandexgpt: до `/completion`, по умолчанию `https://ai.api.cloud.yandex.net/foundationModels/v1`;
    /// - gigachat: до `/chat/completions`, по умолчанию `https://api.giga.chat/v1`.
    pub base_url: String,
    /// openai_compatible: Bearer-ключ; yandexgpt: API-ключ сервисного аккаунта (`Api-Key`);
    /// gigachat: ключ авторизации, которым получается токен доступа.
    pub api_key: Option<Secret>,
    /// Имя модели. Для yandexgpt — часть modelUri после каталога (`yandexgpt/latest`)
    /// или полный `gpt://...`.
    pub model: String,
    pub http_timeout: Duration,
    pub temperature: f32,
    pub max_tokens: u32,
    /// yandexgpt: идентификатор каталога (для modelUri и заголовка `x-folder-id`).
    pub folder_id: Option<String>,
    /// gigachat: адрес получения токена.
    pub oauth_url: String,
    /// gigachat: scope токена.
    pub scope: String,
    /// Дополнительные корневые сертификаты (PEM), например НУЦ Минцифры.
    pub ca_cert_path: Option<PathBuf>,
}

impl LlmConfig {
    /// Конфигурация с умолчаниями для провайдера.
    pub fn for_backend(backend: LlmBackend) -> Self {
        let (base_url, model) = match backend {
            LlmBackend::OpenAiCompatible => ("http://127.0.0.1:8080/v1", "local-model"),
            LlmBackend::YandexGpt => (
                "https://ai.api.cloud.yandex.net/foundationModels/v1",
                "yandexgpt/latest",
            ),
            LlmBackend::GigaChat => ("https://api.giga.chat/v1", "GigaChat-2-Pro"),
        };
        Self {
            backend,
            base_url: base_url.to_string(),
            api_key: None,
            model: model.to_string(),
            http_timeout: Duration::from_secs(60),
            temperature: 0.0,
            max_tokens: 2000,
            folder_id: None,
            oauth_url: "https://ngw.devices.sberbank.ru:9443/api/v2/oauth".to_string(),
            scope: "GIGACHAT_API_PERS".to_string(),
            ca_cert_path: None,
        }
    }
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

        let defaults_llm = LlmConfig::for_backend(llm_backend);
        let llm = LlmConfig {
            backend: llm_backend,
            base_url: env.string_or("LLM_BASE_URL", &defaults_llm.base_url),
            api_key: env.optional("LLM_API_KEY").map(Secret::new),
            model: env.string_or("LLM_MODEL", &defaults_llm.model),
            http_timeout: Duration::from_secs(env.parse_or("LLM_TIMEOUT_SECS", 60u64)?),
            temperature: env.parse_or("LLM_TEMPERATURE", 0.0f32)?,
            max_tokens: env.parse_positive_or("LLM_MAX_TOKENS", defaults_llm.max_tokens)?,
            folder_id: env.optional("LLM_FOLDER_ID"),
            oauth_url: env.string_or("GIGACHAT_OAUTH_URL", &defaults_llm.oauth_url),
            scope: env.string_or("GIGACHAT_SCOPE", &defaults_llm.scope),
            ca_cert_path: env.optional("LLM_CA_CERT_PATH").map(PathBuf::from),
        };
        validate_llm(&llm)?;

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

    /// Предупреждения о небезопасной или тестовой конфигурации; выводятся при старте.
    pub fn startup_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self.onec.backend {
            OneCBackend::Mock => {
                out.push("ONEC_BACKEND=mock: ответы строятся на тестовых данных".to_string());
            }
            OneCBackend::Http => {
                if self.onec.password.is_empty() {
                    out.push("ONEC_PASSWORD не задан".to_string());
                } else if self
                    .onec
                    .base_url
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("http://")
                {
                    out.push(
                        "ONEC_BASE_URL использует http://: пароль 1С (Basic) передаётся \
                         открытым текстом; используйте https://"
                            .to_string(),
                    );
                }
            }
        }
        out
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

/// Проверки, которые лучше сделать при старте, чем на первом запросе.
fn validate_llm(llm: &LlmConfig) -> Result<(), ConfigError> {
    if !(0.0..=2.0).contains(&llm.temperature) {
        return Err(ConfigError::Invalid {
            name: "LLM_TEMPERATURE",
            reason: "ожидается число от 0 до 2".into(),
        });
    }
    let has_key = llm.api_key.as_ref().is_some_and(|k| !k.is_empty());
    match llm.backend {
        LlmBackend::OpenAiCompatible => {}
        LlmBackend::YandexGpt => {
            if !has_key {
                return Err(ConfigError::Missing {
                    name: "LLM_API_KEY",
                });
            }
            if llm.folder_id.is_none() && !llm.model.starts_with("gpt://") {
                return Err(ConfigError::Invalid {
                    name: "LLM_FOLDER_ID",
                    reason: "для yandexgpt нужен идентификатор каталога или LLM_MODEL вида gpt://<каталог>/<модель>".into(),
                });
            }
            if llm.temperature > 1.0 {
                return Err(ConfigError::Invalid {
                    name: "LLM_TEMPERATURE",
                    reason: "для yandexgpt допустимо от 0 до 1".into(),
                });
            }
        }
        LlmBackend::GigaChat => {
            if !has_key {
                return Err(ConfigError::Missing {
                    name: "LLM_API_KEY",
                });
            }
            if !GIGACHAT_SCOPES.contains(&llm.scope.as_str()) {
                return Err(ConfigError::Invalid {
                    name: "GIGACHAT_SCOPE",
                    reason: format!("ожидается одно из {}", GIGACHAT_SCOPES.join(", ")),
                });
            }
        }
    }
    Ok(())
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
    fn provider_specific_defaults_and_checks() {
        assert!(
            config_from(&[("LLM_BACKEND", "yandexgpt")]).is_err(),
            "нужен ключ"
        );
        assert!(
            config_from(&[("LLM_BACKEND", "yandexgpt"), ("LLM_API_KEY", "k")]).is_err(),
            "нужен каталог"
        );
        let ya = config_from(&[
            ("LLM_BACKEND", "yandexgpt"),
            ("LLM_API_KEY", "k"),
            ("LLM_FOLDER_ID", "b1gfolder"),
        ])
        .unwrap();
        assert_eq!(ya.llm.model, "yandexgpt/latest");
        assert!(ya.llm.base_url.contains("foundationModels/v1"));

        let giga = config_from(&[("LLM_BACKEND", "gigachat"), ("LLM_API_KEY", "k")]).unwrap();
        assert_eq!(giga.llm.base_url, "https://api.giga.chat/v1");
        assert_eq!(giga.llm.scope, "GIGACHAT_API_PERS");
        assert!(config_from(&[
            ("LLM_BACKEND", "gigachat"),
            ("LLM_API_KEY", "k"),
            ("GIGACHAT_SCOPE", "ADMIN")
        ])
        .is_err());
    }

    /// Пары ключ=значение из .env.example (без закомментированных строк).
    fn env_example() -> HashMap<String, String> {
        include_str!("../.env.example")
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn env_example_keeps_provider_defaults() {
        let base = env_example();
        let load = |extra: &[(&str, &str)]| {
            let mut map = base.clone();
            for (k, v) in extra {
                map.insert(k.to_string(), v.to_string());
            }
            Config::from_lookup(|k| map.get(k).cloned()).unwrap()
        };

        let local = load(&[]);
        assert_eq!(local.llm.base_url, "http://127.0.0.1:8080/v1");

        let giga = load(&[("LLM_BACKEND", "gigachat"), ("LLM_API_KEY", "k")]);
        assert_eq!(giga.llm.base_url, "https://api.giga.chat/v1");
        assert_eq!(giga.llm.model, "GigaChat-2-Pro");

        let ya = load(&[
            ("LLM_BACKEND", "yandexgpt"),
            ("LLM_API_KEY", "k"),
            ("LLM_FOLDER_ID", "b1g"),
        ]);
        assert!(ya
            .llm
            .base_url
            .starts_with("https://ai.api.cloud.yandex.net/"));
        assert_eq!(ya.llm.model, "yandexgpt/latest");

        let onec = load(&[("ONEC_BACKEND", "http"), ("ONEC_PASSWORD", "p")]);
        assert!(onec.onec.base_url.starts_with("https://"));
    }

    #[test]
    fn warns_about_plain_http_basic_auth() {
        let insecure = config_from(&[
            ("ONEC_BACKEND", "http"),
            ("ONEC_BASE_URL", "HTTP://1c.local/base"),
            ("ONEC_PASSWORD", "p"),
        ])
        .unwrap();
        assert!(insecure
            .startup_warnings()
            .iter()
            .any(|w| w.contains("открытым текстом")));

        let secure = config_from(&[
            ("ONEC_BACKEND", "http"),
            ("ONEC_BASE_URL", "https://1c.local/base"),
            ("ONEC_PASSWORD", "p"),
        ])
        .unwrap();
        assert!(secure.startup_warnings().is_empty());

        let no_password = config_from(&[
            ("ONEC_BACKEND", "http"),
            ("ONEC_BASE_URL", "http://1c.local/base"),
        ])
        .unwrap();
        assert_eq!(
            no_password.startup_warnings(),
            vec!["ONEC_PASSWORD не задан"]
        );
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
