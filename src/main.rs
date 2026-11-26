mod autostart;
mod cli;
mod config;
mod dbus;
mod env;
mod logs;
mod subreaper;
mod supervisor;
mod teardown;
mod wayland;

use clap::Parser;
use cli::{Cli, Commands};
use config::Config;
use logs::SharedRotatingWriter;
use std::process::ExitCode;
use supervisor::Supervisor;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Cli::parse();

    if let Some(Commands::Logs(logs_args)) = &args.command {
        return match logs::run_logs_command(logs_args).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Error: {e:#}");
                ExitCode::FAILURE
            }
        };
    }

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    let write_session_log = !args.dump_default_config && !args.dry_run;
    let mut session_log_error: Option<String> = None;
    let session_writer = if write_session_log {
        let log_path = logs::resolve_log_dir().join("session.log");
        match SharedRotatingWriter::open_default(log_path.clone()) {
            Ok(writer) => Some(writer),
            Err(e) => {
                session_log_error = Some(format!(
                    "Failed to open session log file {}: {}",
                    log_path.display(),
                    e
                ));
                None
            }
        }
    } else {
        None
    };

    init_tracing(filter, session_writer);

    if let Some(err) = session_log_error {
        warn!("{}", err);
    }

    info!("Starting conchy-session v{}", env!("CARGO_PKG_VERSION"));

    // Handle --dump-default-config flag
    if args.dump_default_config {
        let def = Config::default();
        println!("{}", def.to_kdl_string());
        return ExitCode::SUCCESS;
    }

    // Set up Linux subreaper and dedicated process group
    if let Err(e) = subreaper::setup_subreaper_and_process_group() {
        error!("Failed to initialize subreaper/process group: {}", e);
    }

    // Load configuration
    let mut config = match Config::load(args.config.as_deref()) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            return ExitCode::FAILURE;
        }
    };

    // Apply CLI overrides
    if let Some(comp) = args.compositor {
        // If compositor string has spaces, split into command and args
        let parts: Vec<String> = comp.split_whitespace().map(|s| s.to_string()).collect();
        if !parts.is_empty() {
            config.compositor.command = parts[0].clone();
            if parts.len() > 1 {
                config.compositor.args = parts[1..].to_vec();
            }
        }
    }

    if args.no_autostart {
        config.session.autostart = false;
    }

    // Run session supervisor
    let mut supervisor = Supervisor::new(config);
    match supervisor.run(args.dry_run).await {
        Ok(code) => {
            if code == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(code as u8)
            }
        }
        Err(e) => {
            error!("Session failed with error: {:#}", e);
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(filter: EnvFilter, session_writer: Option<SharedRotatingWriter>) {
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);

    let file_layer = session_writer.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_target(false)
            .with_ansi(false)
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();
}
