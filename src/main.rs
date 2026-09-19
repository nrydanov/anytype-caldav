//! CalDAV server, iCalendar feed and Web Push reminders over one Anytype space.

use std::{path::PathBuf, sync::Arc};

use anytype_caldav::{
    accounts::{self, Accounts},
    anytype_source::{AnytypeTaskSource, build_client},
    config::Config,
    feed::FeedService,
    http, install,
    push::PushService,
    render::VTodoRenderer,
    scheduler::PushScheduler,
    series::{self, AnytypeSeries, SeriesGenerator},
    source::TaskSource,
    state::StateStore,
};
use chrono::Utc;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Environment variable this service reads the Anytype API key from.
const API_KEY_ENV: &str = "ANYTYPE_API_KEY";
/// Environment variable the CalDAV password is read from.
const CALDAV_PASSWORD_ENV: &str = "CALDAV_PASSWORD";
/// Environment variable the secret behind every person's password is read from.
const ACCOUNTS_SECRET_ENV: &str = "ACCOUNTS_SECRET";
/// Environment variable the SDK's `env` keystore reads the token from.
const SDK_TOKEN_ENV: &str = "ANYTYPE_KEY_HTTP_TOKEN";

#[derive(Parser)]
#[command(about, version)]
struct Args {
    /// Path to the TOML configuration file. Every value can also come from an
    /// `ANYTYPE_CALDAV__SECTION__KEY` environment variable, which wins over the
    /// file; without a file the whole configuration comes from there.
    #[arg(long)]
    config: Option<PathBuf>,

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
    /// Print which recurring tasks need their next instance today. Creates
    /// nothing unless --apply is given.
    Generate {
        /// Create the missing instances.
        #[arg(long)]
        apply: bool,
    },
    /// Print the account of every person in the space: name, username and
    /// password. Writes nothing, neither to Anytype nor to disk.
    Users,
}

/// A secret from its variable, or from the file its `_FILE` variable names.
fn secret(name: &str) -> Result<Option<String>, anytype_caldav::config::ConfigError> {
    anytype_caldav::config::read_secret(name, |name| std::env::var(name).ok())
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // A panic inside a spawned task ends only that task: without this hook the
    // generator or the scheduler could stop silently while the feed keeps
    // answering, and nothing in the journal would say when or why.
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(non-string panic payload)".into());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        tracing::error!(%payload, %location, %thread, backtrace = %std::backtrace::Backtrace::force_capture(), "panic");
    }));

    let args = Args::parse();

    let api_key = match secret(API_KEY_ENV) {
        Ok(Some(key)) => key,
        Ok(None) => {
            eprintln!("{API_KEY_ENV} or {API_KEY_ENV}_FILE must be set to the Anytype API key");
            return std::process::ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("{err}");
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

    let overrides: Vec<String> = std::env::vars()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with(anytype_caldav::config::ENV_PREFIX))
        .collect();
    if !overrides.is_empty() {
        info!(?overrides, "configuration values from the environment");
    }
    let config = match Config::load(args.config.as_deref()) {
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

    // With no space in the configuration, the account's only space is served.
    // Looked up on every start, so a space the account joined since is found.
    let mut config = config;
    if config.anytype.space_id.is_empty() {
        let found = runtime.block_on(async {
            let client = build_client(&config.anytype).map_err(|err| err.to_string())?;
            anytype_caldav::anytype_source::only_space(&client).await
        });
        match found {
            Ok((id, name)) => {
                info!(space = %name, space_id = %id, "anytype.space_id not set: serving the only space of the account");
                config.anytype.space_id = id;
            }
            Err(err) => {
                eprintln!("{err}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    let result = match args.command {
        None => runtime.block_on(run(config)),
        Some(Command::Init { apply }) => runtime.block_on(init(config, apply)),
        Some(Command::Generate { apply }) => runtime.block_on(generate(config, apply)),
        Some(Command::Users) => runtime.block_on(users(config)),
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

/// The passwords are handed out by hand, in private: a person's page is read
/// by the whole space, so nothing is written there.
async fn users(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let secret = secret(ACCOUNTS_SECRET_ENV)?.ok_or_else(|| {
        format!(
            "neither {ACCOUNTS_SECRET_ENV} nor {ACCOUNTS_SECRET_ENV}_FILE is set: give it \
             the same value the service runs with"
        )
    })?;
    let accounts = Accounts::new(&secret);
    let client = build_client(&config.anytype)?;
    let people = accounts::holders(&client, &config.anytype.space_id).await?;
    if people.is_empty() {
        println!("no profile in the space links an active member: nobody has an account");
        return Ok(());
    }
    let width = people
        .iter()
        .map(|(_, name)| name.chars().count())
        .max()
        .unwrap_or(0);
    for (id, name) in &people {
        let pad = " ".repeat(width - name.chars().count());
        println!(
            "{name}{pad}  {}  {}",
            accounts::username(id),
            accounts.password_of(id)
        );
    }
    Ok(())
}

async fn generate(config: Config, apply: bool) -> Result<(), Box<dyn std::error::Error>> {
    generate_kind(&config, series::Kind::Task, apply).await?;
    if config.series.events {
        generate_kind(&config, series::Kind::Event, apply).await?;
    }
    Ok(())
}

async fn generate_kind(
    config: &Config,
    kind: series::Kind,
    apply: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let space = AnytypeSeries::new(
        build_client(&config.anytype)?,
        config.anytype.space_id.clone(),
        kind,
    );
    let tz = config.calendar.timezone;
    let now = Utc::now();
    let today = now.with_timezone(&tz).date_naive();
    // The claims are what keep a task deleted by hand from coming back, so the
    // command uses the same database as the service when one is configured.
    // A different machine's database protects only that machine's runs.
    let state = match config.push.state_file.as_deref() {
        Some(path) => Some(Arc::new(StateStore::open(path)?)),
        None => {
            println!("  note    no push.state_file: occurrences created before are not remembered");
            None
        }
    };

    let all = space.series().await?;
    let instances = space.instances().await?;
    println!(
        "space {} ({}): {} series, {} instances, today {today}",
        config.anytype.space_id,
        kind.series_type(),
        all.len(),
        instances.len()
    );
    let until = today + config.series.horizon;
    let (planned, warnings) = series::plan_until(&all, &instances, tz, today, until);
    for warning in &warnings {
        println!("  warn    {warning}");
    }
    if planned.is_empty() {
        println!("nothing to create");
        return Ok(());
    }
    for one in &planned {
        let claimed = match &state {
            Some(state) => state.instance_claimed(&one.series.id, one.day)?,
            None => false,
        };
        let verb = if claimed { "skip   " } else { "create " };
        let why = if claimed {
            "; created once before, perhaps deleted by hand"
        } else {
            ""
        };
        println!(
            "  {verb} {:?} for {} ({}){why}",
            one.series.name, one.day, one.occurrence
        );
    }
    if !apply {
        println!("dry run: nothing was created; re-run with --apply");
        return Ok(());
    }
    match state {
        // The same pass the service runs, claims included.
        Some(state) => {
            let generator = SeriesGenerator::new(
                space,
                state,
                tz,
                config.series.poll_interval,
                config.series.horizon,
            );
            let created = generator.check_at(now).await?;
            println!("  created {created}");
        }
        None => {
            for one in &planned {
                let id = space.create(one).await?;
                println!("  created {id}");
            }
        }
    }
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
        feed_path = %http::redact(&config.server.feed_path, &config.server.feed_path),
        version = env!("CARGO_PKG_VERSION"),
        reminders_enabled = config.reminders.enabled,
        push_enabled = config.push.enabled,
        push_poll_interval = ?config.push.poll_interval,
        state_file = ?config.push.state_file,
        series_enabled = config.series.enabled,
        series_poll_interval = ?config.series.poll_interval,
        tags_selector = config.properties.tags.is_some(),
        assignee_selector = config.properties.assignee.is_some(),
        reminder_selector = config.properties.reminder.is_some(),
        "starting anytype-caldav"
    );

    let anytype = Arc::new(AnytypeTaskSource::connect(
        config.anytype.clone(),
        config.properties.clone(),
    )?);
    let source: Arc<dyn TaskSource> = anytype.clone();
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

    let durable_state = match config.push.state_file.as_deref() {
        Some(path) => Some(Arc::new(StateStore::open(path)?)),
        None => None,
    };

    let (push, scheduler) = match (
        config.push.enabled,
        config.push.private_key_file.as_deref(),
        config.push.state_file.as_deref(),
        durable_state.clone(),
    ) {
        (true, Some(key_path), Some(state_path), Some(durable_state)) => {
            let service =
                PushService::load(key_path, durable_state.clone(), config.push.app_url.clone())?;
            let scheduler = PushScheduler::new(
                source,
                durable_state,
                service.clone(),
                config.calendar.clone(),
                config.reminders.clone(),
                config.push.poll_interval,
                config.push.late_window,
                config.server.request_timeout,
            );
            let scheduler = Arc::new(match (config.caldav.events, config.reminders.enabled) {
                (true, _) => {
                    scheduler.with_events(Arc::new(anytype_caldav::events::AnytypeEvents::new(
                        build_client(&config.anytype)?,
                        config.anytype.space_id.clone(),
                    )))
                }
                (false, _) => scheduler,
            });
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

    let durable_state_for_documents = durable_state.clone();

    // Validation guarantees a state file whenever a generator is enabled.
    let mut generator_handles = Vec::new();
    for (kind, enabled) in [
        (series::Kind::Task, config.series.enabled),
        (series::Kind::Event, config.series.events),
    ] {
        if let (true, Some(durable_state)) = (enabled, durable_state.clone()) {
            let generator = Arc::new(SeriesGenerator::new(
                AnytypeSeries::new(
                    build_client(&config.anytype)?,
                    config.anytype.space_id.clone(),
                    kind,
                ),
                durable_state,
                config.calendar.timezone,
                config.series.poll_interval,
                config.series.horizon,
            ));
            info!(?kind, poll_interval = ?config.series.poll_interval, horizon_days = config.series.horizon.num_days(), "series generator enabled");
            generator_handles.push(tokio::spawn(generator.run()));
        }
    }

    let caldav = if config.caldav.enabled {
        let password = secret(CALDAV_PASSWORD_ENV)?.ok_or_else(|| {
            format!(
                "caldav.enabled is true but neither {CALDAV_PASSWORD_ENV} nor \
                 {CALDAV_PASSWORD_ENV}_FILE is set"
            )
        })?;
        info!(
            username = %config.caldav.username,
            base = anytype_caldav::caldav::BASE,
            writable = config.caldav.writable,
            "caldav facade enabled"
        );
        let credentials =
            anytype_caldav::caldav::Credentials::new(&config.caldav.username, &password);
        // With the secret set, every person of the space also has an account
        // of their own (`exporter users` prints them).
        let credentials = match secret(ACCOUNTS_SECRET_ENV)? {
            Some(secret) => {
                info!("caldav accounts per person enabled");
                credentials.with_accounts(&secret)
            }
            None => credentials,
        };
        Some(Arc::new(credentials))
    } else {
        None
    };

    let events = if config.caldav.events {
        use anytype_caldav::events::{AnytypeEvents, EventService};
        info!(
            ttl = ?config.server.min_refresh_interval,
            writable = config.caldav.writable,
            "caldav events collection enabled"
        );
        Some(Arc::new(EventService::new(
            Arc::new(AnytypeEvents::new(
                build_client(&config.anytype)?,
                config.anytype.space_id.clone(),
            )),
            config.calendar.clone(),
            Utc::now(),
            config.server.min_refresh_interval,
        )))
    } else {
        None
    };

    // The calendar app's own documents, kept beside the push subscriptions.
    let documents = config
        .caldav
        .settings
        .then(|| durable_state_for_documents.clone())
        .flatten();
    if config.caldav.settings && documents.is_none() {
        tracing::warn!(
            "caldav.settings is true but push.state_file is not set; client settings are not stored"
        );
    }

    let state = http::AppState {
        documents,
        events,
        events_in_tasks: config.caldav.events_in_tasks,
        calendar_names: config.caldav.names.clone(),
        feed,
        allowed_origins: Arc::new(config.server.allowed_origins.clone()),
        caldav,
        writer: config
            .caldav
            .writable
            .then(|| anytype.clone() as Arc<dyn anytype_caldav::source::TaskWriter>),
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
        feed_path = %http::redact(&config.server.feed_path, &config.server.feed_path),
        "listening"
    );

    let serve_result = axum::serve(listener, http::router(state, &config.server.feed_path))
        .with_graceful_shutdown(async {
            // systemd stops the service with SIGTERM; logging which signal
            // arrived separates a restart from a crash in the journal.
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler installs");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => info!(signal = "SIGINT", "shutting down"),
                    _ = term.recv() => info!(signal = "SIGTERM", "shutting down"),
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
                info!("shutting down");
            }
        })
        .await;

    for handle in scheduler_handle.into_iter().chain(generator_handles) {
        handle.abort();
        let _ = handle.await;
    }
    serve_result?;
    Ok(())
}
