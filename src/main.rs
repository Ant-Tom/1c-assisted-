use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use onec_assistant::api::{router, AppState};
use onec_assistant::audit::AuditLog;
use onec_assistant::auth::FileIdentityResolver;
use onec_assistant::config::{Config, LlmBackend, OneCBackend};
use onec_assistant::error::AppError;
use onec_assistant::llm::{GigaChat, LlmProvider, OpenAiCompatible, YandexGpt};
use onec_assistant::onec::{HttpOneC, MockOneC, OneCClient};
use onec_assistant::orchestrator::{Orchestrator, OrchestratorSettings};
use onec_assistant::tools::onec_registry;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %err, "сервис остановлен с ошибкой");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), AppError> {
    let config = Config::from_env()?;

    let identity = Arc::new(FileIdentityResolver::from_file(&config.users_file)?);

    for warning in config.startup_warnings() {
        tracing::warn!("{warning}");
    }

    let onec: Arc<dyn OneCClient> = match config.onec.backend {
        OneCBackend::Mock => Arc::new(MockOneC::new()),
        OneCBackend::Http => Arc::new(HttpOneC::new(&config.onec)?),
    };

    let llm: Arc<dyn LlmProvider> = match config.llm.backend {
        LlmBackend::OpenAiCompatible => Arc::new(OpenAiCompatible::new(&config.llm)?),
        LlmBackend::YandexGpt => Arc::new(YandexGpt::new(&config.llm)?),
        LlmBackend::GigaChat => Arc::new(GigaChat::new(&config.llm)?),
    };

    let tools = Arc::new(onec_registry(onec)?);
    let audit = AuditLog::open(Path::new(&config.audit_db_path))?.with_secrets(config.secrets());

    let orchestrator = Arc::new(Orchestrator::new(
        llm,
        tools,
        audit,
        OrchestratorSettings {
            limits: config.limits,
            log_questions: config.log_questions,
        },
    ));
    let app = router(AppState {
        orchestrator,
        identity: identity.clone(),
    });

    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(
        addr = %config.bind_addr,
        llm = ?config.llm.backend,
        model = %config.llm.model,
        onec = ?config.onec.backend,
        users = identity.len(),
        log_questions = config.log_questions,
        "сервис запущен"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("сервис остановлен");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "не удалось подписаться на Ctrl+C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "не удалось подписаться на SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
