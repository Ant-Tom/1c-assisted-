//! Ошибки уровня приложения (запуск сервиса).

use thiserror::Error;

use crate::audit::AuditError;
use crate::auth::AuthError;
use crate::config::ConfigError;
use crate::llm::LlmError;
use crate::onec::OneCError;
use crate::tools::RegistryError;

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("журнал: {0}")]
    Audit(#[from] AuditError),
    #[error("модель: {0}")]
    Llm(#[from] LlmError),
    #[error("1С: {0}")]
    OneC(#[from] OneCError),
    #[error("реестр функций: {0}")]
    Registry(#[from] RegistryError),
    #[error("ошибка ввода-вывода: {0}")]
    Io(#[from] std::io::Error),
}
