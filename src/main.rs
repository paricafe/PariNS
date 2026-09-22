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
        config.check_files()?;
        println!("configuration valid");
        return Ok(());
    }
    let server = parins::server::Server::bind(config).await?;
    eprintln!("PariNS listening on {} (UDP/TCP)", server.local_addr()?);
    for (protocol, address) in server.encrypted_addrs()? {
        eprintln!("PariNS listening on {address} ({protocol})");
    }
    if let Some(address) = server.admin_addr()? {
        eprintln!("PariNS local metrics on {address}");
    }
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    #[cfg(unix)]
    let reload = server.reload_handle();
    server
        .run(async {
            #[cfg(unix)]
            loop { tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result { eprintln!("shutdown signal error: {error}"); }
                    break;
                }
                _ = terminate.recv() => break,
                _ = hangup.recv() => {
                    let reload = reload.clone();
                    match tokio::task::spawn_blocking(move || reload.reload()).await {
                        Ok(Ok(())) => eprintln!("PariNS rules and certificates reloaded"),
                        Ok(Err(error)) => eprintln!("PariNS reload rejected; previous generation retained: {error}"),
                        Err(error) => eprintln!("PariNS reload worker failed: {error}"),
                    }
                }
            }}
            #[cfg(not(unix))]
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("shutdown signal error: {error}");
            }
        })
        .await?;
    eprintln!("PariNS stopped");
    Ok(())
}
