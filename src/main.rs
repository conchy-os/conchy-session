mod autostart;
mod cli;
mod config;
mod dbus;
mod env;
mod subreaper;
mod supervisor;
mod teardown;
mod wayland;

use clap::Parser;
use cli::Cli;
use config::Config;
use std::process::ExitCode;
use supervisor::Supervisor;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Cli::parse();

    // Initialize tracing logger
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

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
