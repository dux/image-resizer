use clap::{Parser, Subcommand};
use image_resizer::app::{self, App};
use image_resizer::config::Config;
use image_resizer::{housekeeping, logs, resize};

#[derive(Parser)]
#[command(
    name = "image-resizer",
    about = "Image resize service: AVIF/WebP, caching, STEP renders"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server
    #[command(alias = "s")]
    Start,
}

fn main() {
    let cli = Cli::parse();
    let Some(Command::Start) = cli.command else {
        use clap::CommandFactory;
        let _ = Cli::command().print_help();
        return;
    };
    let _ = dotenvy::dotenv();
    logs::init_tracing();
    if let Err(e) = run() {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    resize::pipeline::init()?;
    let port = config.port;
    let app = App::new(config)?;
    housekeeping::spawn(app.clone());

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|e| format!("bind :{port}: {e}"))?;
    tracing::info!("Server starting on http://localhost:{port}");
    axum::serve(listener, app::router(app.clone()))
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("Shutting down server...");
    tokio::task::spawn_blocking(move || housekeeping::flush(&app))
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("Server stopped");
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}
