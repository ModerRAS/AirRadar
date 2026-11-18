//! AirRadar binary entry point.
//!
//! Modes:
//! - (default) run the web service (scheduler + HTTP API)
//! - --once [--season YYYY-season] [--max-subjects N] : run one collection and exit
//! - --healthcheck : probe the local /health endpoint (Docker HEALTHCHECK)
//! - --version / --help

use std::sync::Arc;
use std::time::Duration;

use airradar::app::{AppState, RefreshController, RefreshRequest};
use airradar::config::Config;
use airradar::log;
use airradar::model::RunTrigger;
use airradar::pipeline::{Pipeline, RunCoordinator};
use airradar::season::SeasonRef;
use airradar::{api, scheduler, VERSION};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--healthcheck") {
        std::process::exit(healthcheck());
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("airradar {VERSION}");
        return;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return;
    }
    log::init_from_env();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("fatal: cannot start tokio runtime: {error}");
            std::process::exit(1);
        }
    };
    match runtime.block_on(dispatch(args)) {
        Ok(()) => {}
        Err(message) => {
            eprintln!("fatal: {message}");
            std::process::exit(1);
        }
    }
}

async fn dispatch(args: Vec<String>) -> Result<(), String> {
    let mut once = false;
    let mut season_arg: Option<String> = None;
    let mut max_subjects: Option<usize> = None;
    let mut probe_url: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--once" => once = true,
            "--season" => {
                index += 1;
                season_arg = Some(
                    args.get(index)
                        .ok_or("--season requires a value (YYYY-season)")?
                        .clone(),
                );
            }
            "--max-subjects" => {
                index += 1;
                let value = args.get(index).ok_or("--max-subjects requires a value")?;
                max_subjects = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid --max-subjects value {value:?}"))?,
                );
            }
            "--probe-url" => {
                index += 1;
                probe_url = Some(args.get(index).ok_or("--probe-url requires a URL")?.clone());
            }
            other => return Err(format!("unknown argument {other:?}; use --help")),
        }
        index += 1;
    }

    let mut cfg = Config::from_env().map_err(|error| format!("configuration error: {error}"))?;
    if let Some(limit) = max_subjects {
        cfg.max_subjects_per_run = limit.clamp(1, 200);
    }
    let cfg = Arc::new(cfg);
    log::init(&cfg.log_level);
    log::info(format!("AirRadar {VERSION} starting"));
    if let Some(url) = probe_url {
        let pipeline = Pipeline::new(cfg.clone())?;
        let report = pipeline.probe_url(&url).await?;
        let json = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("serialize probe report: {error}"))?;
        println!("{json}");
        return Ok(());
    }
    if once {
        let season = match season_arg {
            Some(text) => Some(
                SeasonRef::parse(&text)
                    .ok_or_else(|| format!("invalid --season {text:?}; expected YYYY-season"))?,
            ),
            None => cfg.season_override,
        };
        log::info(format!(
            "one-shot collection for season {}",
            season
                .map(|season| season.label())
                .unwrap_or_else(|| "current".to_string())
        ));
        let pipeline = Pipeline::new(cfg.clone())?;
        let coordinator = RunCoordinator::new(pipeline);
        let request = RefreshRequest::new(RunTrigger::Cli, season);
        let summary = coordinator
            .clone()
            .run_blocking(request)
            .await
            .ok_or("another refresh is already running")?;
        let json = serde_json::to_string_pretty(&summary)
            .map_err(|error| format!("serialize summary: {error}"))?;
        println!("{json}");
        if summary.ok {
            Ok(())
        } else {
            Err(format!(
                "one-shot run reported errors: {}",
                summary.errors.join("; ")
            ))
        }
    } else {
        serve(cfg).await
    }
}

async fn serve(cfg: Arc<Config>) -> Result<(), String> {
    let pipeline = Pipeline::new(cfg.clone())?;
    let coordinator = RunCoordinator::new(pipeline.clone());
    let store = pipeline.store().clone();
    let cache = pipeline.cache().clone();
    let mut state = AppState::new(cfg.clone(), store, cache, coordinator);
    state.search_providers = pipeline
        .search_configured()
        .iter()
        .map(|provider| provider.to_string())
        .collect();
    state.llm_configured = pipeline.llm_configured();
    let state = Arc::new(state);
    log::info(format!("data dir: {:?}", cfg.data_dir));
    log::info(format!("search providers: {:?}", state.search_providers));
    if state.search_providers.is_empty() {
        log::warn("no search API key configured: only Bangumi and official websites are used");
    }
    if !state.llm_configured {
        log::info("LLM fallback disabled (LLM_BASE_URL/LLM_API_KEY/LLM_MODEL not all set)");
    }
    let app = api::router(state.clone());
    let address = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| format!("bind {address}: {error}"))?;
    log::info(format!("listening on http://{address}"));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let scheduler_handle = tokio::spawn(scheduler::run(state.clone(), shutdown_rx));
    let serve_result = axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await;
    log::info("shutdown requested, stopping scheduler");
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(20), scheduler_handle).await;
    log::info("AirRadar stopped");
    serve_result.map_err(|error| format!("server error: {error}"))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        match signal {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    log::info("termination signal received");
}

/// Probe 127.0.0.1:PORT/health; exit code 0 on HTTP 200.
fn healthcheck() -> i32 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(8080);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return 1,
    };
    runtime.block_on(async move {
        let endpoint = format!("127.0.0.1:{port}");
        let probe = async {
            let mut stream = tokio::net::TcpStream::connect(&endpoint).await?;
            stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await?;
            let mut buffer = [0u8; 256];
            let read = stream.read(&mut buffer).await?;
            let response = String::from_utf8_lossy(&buffer[..read]).to_string();
            Ok::<String, std::io::Error>(response)
        };
        match tokio::time::timeout(Duration::from_secs(5), probe).await {
            Ok(Ok(response)) => {
                if response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200") {
                    0
                } else {
                    1
                }
            }
            _ => 1,
        }
    })
}

fn print_help() {
    println!(
        "AirRadar {VERSION} - self-hosted anime airing schedule index

Usage:
  airradar                                Start the web service (default)
  airradar --once                        Run one collection and exit
  airradar --once --season 2026-fall     Collect a specific season
  airradar --once --max-subjects 5       Limit the collection window
  airradar --probe-url https://example.com/page
                                         Fetch a page and print extracted broadcast slots
  airradar --healthcheck                 Probe http://127.0.0.1:PORT/health
  airradar --version                     Print the version
  airradar --help                        Print this help

Environment: see README.md (PORT, DATA_DIR, ADMIN_TOKEN, BANGUMI_UA,
BRAVE_API_KEY, TAVILY_API_KEY, DEEPSEEK_API_KEY, LLM_*, REFRESH_HOURS,
MAX_SUBJECTS_PER_RUN, MAX_LLM_CALLS, SEASON_OVERRIDE, ...)."
    );
}
