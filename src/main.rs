mod config;
mod download;
mod server;
mod youtrack;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::{ServiceExt, transport::io::stdio_pipes};
use std::path::PathBuf;

use config::Config;
use server::YouTrackServer;
use youtrack::YouTrack;

#[derive(Parser)]
#[command(version, about = "Read-only stdio MCP server for YouTrack")]
struct Args {
    /// Путь к TOML-файлу; иначе ./yt-mcp.toml, затем ~/.config/yt-mcp.toml.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
        .init();

    let path = Config::resolve_path(Args::parse().config.as_deref())?;
    let config = Config::load(&path)?;
    let client = YouTrack::new(config)?;
    let service = YouTrackServer::new(client)
        .serve(stdio_pipes())
        .await
        .context("Не удалось запустить MCP через stdio")?;
    tokio::select! {
        result = service.waiting() => { result.context("Ошибка транспорта MCP")?; }
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
