//! Ошибки уровня приложения (запуск сервиса).

use thiserror::Error;

use crate::auth::AuthError;
use crate::config::ConfigError;

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("ошибка ввода-вывода: {0}")]
    Io(#[from] std::io::Error),
    #[error("ошибка инициализации: {0}")]
    Init(String),
}
