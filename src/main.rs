//! Read-only bridge from one Anytype space to an iCalendar VTODO subscription.

use std::{path::PathBuf, sync::Arc};

use anytype_task_exporter::{
    anytype_source::{AnytypeTaskSource, build_client},
    config::Config,
    feed::FeedService,
    http, install,
    push::PushService,
    render::VTodoRenderer,
    scheduler::PushScheduler,
    source::TaskSource,
    state::StateStore,
};
use chrono::Utc;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Environment variable this service reads the Anytype API key from.
const API_KEY_ENV: &str = "ANYTYPE_API_KEY";
/// Environment variable the SDK's `env` keystore reads the token from.
const SDK_TOKEN_ENV: &str = "ANYTYPE_KEY_HTTP_TOKEN";

#[derive(Parser)]
#[command(about, version)]
struct Args {
    /// Path to the TOML configuration file.
    #[arg(long)]
    config: PathBuf,

    /// Without a subcommand the service runs.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Check the configured space against the facade's schema and print what
    /// is missing. Creates nothing unless --apply is given.
    Init {
        /// Create the missing properties.
        #[arg(long)]
        apply: bool,
    },
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let api_key = match std::env::var(API_KEY_ENV) {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            eprintln!("{API_KEY_ENV} must be set to the Anytype API key");
            return std::process::ExitCode::FAILURE;
        }
    };

    // The SDK accepts a token only through its keystore, and its `env` store
    // reads this variable. Setting it here, before the Tokio runtime starts
    // and before any thread exists, is the only sound window: `set_var` is
    // unsafe in edition 2024 precisely because it races with concurrent
    // readers in other threads.
    unsafe {
        std::env::set_var(SDK_TOKEN_ENV, &api_key);
    }
    drop(api_key);

    let config = match Config::load(&args.config) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("configuration error: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("cannot start async runtime: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let result = match args.command {
        None => runtime.block_on(run(config)),
        Some(Command::Init { apply }) => runtime.block_on(init(config, apply)),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn init(config: Config, apply: bool) -> Result<(), Box<dyn std::error::Error>> {
    let client = build_client(&config.anytype)?;
    println!("space {}", config.anytype.space_id);
    install::run(&client, &config.anytype.space_id, apply).await?;
    Ok(())
}

async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    // Secrets are deliberately absent from this line.
    info!(
        anytype_url = %config.anytype.url,
        space_id = %config.anytype.space_id,
        type_key = %config.anytype.type_key,
        timezone = %config.calendar.timezone,
        date_only_timezone = %config.calendar.date_only_timezone,
        listen = %config.server.listen,
        min_refresh_interval = ?config.server.min_refresh_interval,
        request_timeout = ?config.server.request_timeout,
        allowed_origins = ?config.server.allowed_origins,
        "starting anytype-task-exporter"
    );

    let source: Arc<dyn TaskSource> = Arc::new(AnytypeTaskSource::connect(
        config.anytype.clone(),
        config.properties.clone(),
    )?);
    let renderer = VTodoRenderer::new(
        config.calendar.clone(),
        config.reminders.clone(),
        Utc::now(),
    );
    let feed = Arc::new(FeedService::new(
        source.clone(),
        renderer,
        config.server.min_refresh_interval,
        config.server.request_timeout,
    ));

    let (push, scheduler) = match (
        config.push.enabled,
        config.push.private_key_file.as_deref(),
        config.push.state_file.as_deref(),
    ) {
        (true, Some(key_path), Some(state_path)) => {
            let durable_state = Arc::new(StateStore::open(state_path)?);
            let service = PushService::load(key_path, durable_state.clone())?;
            let scheduler = Arc::new(PushScheduler::new(
                source,
                durable_state,
                service.clone(),
                config.calendar.clone(),
                config.reminders.clone(),
                config.push.poll_interval,
                config.push.late_window,
                config.server.request_timeout,
            ));
            info!(
                public_key = %service.public_key(),
                subscriptions = service.subscription_count(),
                state_file = %state_path.display(),
                poll_interval = ?config.push.poll_interval,
                late_window = ?config.push.late_window,
                "web push enabled"
            );
            if !config.reminders.enabled {
                tracing::warn!(
                    "push is enabled but reminders.enabled is false: the scheduler will never send anything"
                );
            }
            (Some(service), Some(scheduler))
        }
        _ => (None, None),
    };
    let scheduler_handle = scheduler.map(|scheduler| tokio::spawn(scheduler.run()));

    let state = http::AppState {
        feed,
        allowed_origins: Arc::new(config.server.allowed_origins.clone()),
        push,
    };

    // The feed is unauthenticated by design, so on a reachable bind the only
    // thing protecting it is an unguessable path.
    if config.server.is_publicly_bound() && config.server.feed_path_is_guessable() {
        tracing::warn!(
            listen = %config.server.listen,
            feed_path = %config.server.feed_path,
            "the feed is bound to a non-loopback address at a guessable path; \
             anyone who reaches this port can read every task. Set server.feed_path \
             to something unguessable, e.g. /f/$(openssl rand -hex 16)/todos.ics"
        );
    }

    let listener = tokio::net::TcpListener::bind(config.server.listen).await?;
    info!(
        listen = %config.server.listen,
        feed_path = %config.server.feed_path,
        "listening"
    );

    let serve_result = axum::serve(listener, http::router(state, &config.server.feed_path))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await;

    if let Some(handle) = scheduler_handle {
        handle.abort();
        let _ = handle.await;
    }
    serve_result?;
    Ok(())
}
