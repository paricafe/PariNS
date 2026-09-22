use std::path::PathBuf;

use anyhow::{Result, bail};
use parins::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut path = PathBuf::from("parins.toml");
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                path = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?
                    .into()
            }
            "--check" => check = true,
            "--help" | "-h" => {
                println!("Usage: parins [--config PATH] [--check]\nDefault config: parins.toml");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("parins {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }
    let config = Config::load(&path)?;
    if check {
        println!("configuration valid");
        return Ok(());
    }
    let server = parins::server::Server::bind(config).await?;
    eprintln!("PariNS listening on {} (UDP/TCP)", server.local_addr()?);
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    server
        .run(async {
            #[cfg(unix)]
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result { eprintln!("shutdown signal error: {error}"); }
                }
                _ = terminate.recv() => {}
            }
            #[cfg(not(unix))]
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("shutdown signal error: {error}");
            }
        })
        .await?;
    eprintln!("PariNS stopped");
    Ok(())
}
