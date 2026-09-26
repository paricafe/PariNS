use std::path::PathBuf;

use anyhow::{Result, bail};
use parins::config::Config;

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run());
    // A cancelled blocking filesystem syscall cannot be interrupted. Do not let
    // Tokio's default unbounded destructor defeat the terminal IO budget.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    result
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut path = PathBuf::from("parins.toml");
    let mut check = false;
    let mut manage = false;
    let mut state_dir = PathBuf::from("parins-state");
    let mut data_dir = PathBuf::from("parins-data");
    let mut data_given = false;
    let mut web_listen = "0.0.0.0:3000".parse()?;
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
            "--data-dir" => {
                data_given = true;
                data_dir = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--data-dir requires a path"))?
                    .into();
            }
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
                    "Usage: parins [--config PATH] [--data-dir DIR] [--check]\n       parins --manage [--state-dir DIR] [--web-listen IP:PORT] [--check]\n       parins --build-info=json\nDefault config: parins.toml; file runtime data: parins-data; managed state: parins-state; web listen: 0.0.0.0:3000 (HTTP until inbound DoH with a matching certificate is enabled)\nIPv6: --web-listen [::]:3000; local-only: --web-listen 127.0.0.1:3000\nManaged --check reads existing saved configuration without starting services or creating state."
                );
                return Ok(());
            }
            "--version" | "-V" => {
                println!("parins {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--build-info=json" => {
                println!(
                    "{}",
                    serde_json::to_string(&parins::update::build_info::BuildInfo::current())?
                );
                return Ok(());
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }
    if manage {
        if config_given || data_given {
            bail!(
                "--manage cannot be combined with --config or --data-dir; import TOML in the console"
            );
        }
        if check {
            let report = parins::manage::check::managed(&state_dir, web_listen)?;
            println!(
                "{}",
                serde_json::to_string(&parins::update::ipc::ManagedCheckOutput {
                    check: report,
                    build_info: parins::update::build_info::BuildInfo::current(),
                })?
            );
            return Ok(());
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
    let settings = parins::storage::RuntimeSettings::from_config(&config);
    let runtime_dir = data_dir.clone();
    let services = tokio::task::spawn_blocking(move || {
        parins::runtime_services::RuntimeServices::open(&runtime_dir, settings, 0)
    })
    .await??;
    let data_dir = data_dir.canonicalize()?;
    eprintln!("PariNS runtime data: {}", data_dir.display());
    let persistence =
        std::sync::Arc::new(parins::cache_persistence::CachePersistence::new(&data_dir));
    let snapshot =
        parins::runtime_lifecycle::consume(persistence.clone(), config.cache.persistence.clone())
            .await?;
    let server =
        parins::server::Server::bind_with_services(config.clone(), None, services.clone()).await?;
    let resolver = server.resolver().clone();
    let fingerprint =
        parins::cache::persistence::semantic_fingerprint(&config, &resolver.policy_digest())?;
    let report =
        parins::runtime_lifecycle::restore(snapshot, resolver.cache(), fingerprint).await?;
    eprintln!("PariNS cache restore: {}", serde_json::to_string(&report)?);
    services.set_dns_state(true, 1);
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
    #[cfg(unix)]
    let mut reload_task: Option<tokio::task::JoinHandle<Result<()>>> = None;
    #[cfg(unix)]
    let mut reload_pending = false;
    let outcome = server
        .run(async {
            #[cfg(unix)]
            loop { tokio::select! {
                biased;
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result { eprintln!("shutdown signal error: {error}"); }
                    break;
                }
                _ = terminate.recv() => break,
                _ = hangup.recv() => {
                    if reload_task.is_some() { reload_pending = true; }
                    else {
                        let reload = reload.clone();
                        reload_task = Some(tokio::task::spawn_blocking(move || reload.reload()));
                    }
                }
                result = async { if let Some(task) = &mut reload_task { task.await } else { std::future::pending().await } }, if reload_task.is_some() => {
                    reload_task = None;
                    match result {
                        Ok(Ok(())) => eprintln!("PariNS rules and certificates reloaded"),
                        Ok(Err(error)) => eprintln!("PariNS reload rejected; previous generation retained: {error}"),
                        Err(error) => eprintln!("PariNS reload worker failed: {error}"),
                    }
                    if reload_pending {
                        reload_pending = false;
                        let reload = reload.clone();
                        reload_task = Some(tokio::task::spawn_blocking(move || reload.reload()));
                    }
                }
            }}
            #[cfg(not(unix))]
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("shutdown signal error: {error}");
            }
        })
        .await;
    services.set_dns_state(false, 1);
    // Server has closed publication before returning: a late blocking read can
    // never change the policy fingerprint or active identities after this cut.
    #[cfg(unix)]
    let reload_drained = if let Some(task) = reload_task {
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .is_ok()
    } else {
        true
    };
    #[cfg(not(unix))]
    let reload_drained = true;
    let quiescent = reload_drained && outcome.is_ok() && resolver.is_quiescent();
    let report = parins::runtime_lifecycle::terminal(
        services,
        persistence,
        Some((resolver, config, quiescent)),
    )
    .await;
    eprintln!("PariNS cache save: {}", serde_json::to_string(&report)?);
    outcome?;
    eprintln!("PariNS stopped");
    Ok(())
}
