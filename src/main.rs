use std::process::ExitCode;

use onec_assistant::auth::FileIdentityResolver;
use onec_assistant::config::Config;
use onec_assistant::error::AppError;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "сервис остановлен с ошибкой");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), AppError> {
    let config = Config::from_env()?;
    let users = FileIdentityResolver::from_file(&config.users_file)?;
    tracing::info!(users = users.len(), "конфигурация загружена");
    Ok(())
}
