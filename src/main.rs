use std::path::PathBuf;

use anyhow::{Result, bail};
use parins::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut path = PathBuf::from("parins.toml");
    let mut check = false;
    let mut manage = false;
    let mut state_dir = PathBuf::from("parins-state");
    let mut web_listen = "127.0.0.1:3000".parse()?;
    let mut config_given = false;
    let mut management_option = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_given = true;
                path = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?
                    .into()
            }
            "--check" => check = true,
            "--manage" => manage = true,
            "--state-dir" => {
                management_option = true;
                state_dir = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--state-dir requires a path"))?
                    .into();
            }
            "--web-listen" => {
                management_option = true;
                web_listen = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--web-listen requires IP:port"))?
                    .parse()?;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: parins [--config PATH] [--check]\n       parins --manage [--state-dir DIR] [--web-listen 127.0.0.1:3000]\nDefault config: parins.toml; managed state: parins-state"
                );
                return Ok(());
            }
            "--version" | "-V" => {
                println!("parins {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }
    if manage {
        if config_given || check {
            bail!(
                "--manage cannot be combined with --config or --check; import TOML in the console"
            );
        }
        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        return parins::manage::serve(&state_dir, web_listen, async {
            #[cfg(unix)]
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    }
    if management_option {
        bail!("--state-dir and --web-listen require --manage");
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
