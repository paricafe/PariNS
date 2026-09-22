use std::path::PathBuf;

use anyhow::{Result, bail};
use parins::config::Config;

fn main() -> Result<()> {
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
    let _config = Config::load(&path)?;
    if check {
        println!("configuration valid");
        return Ok(());
    }
    bail!("DNS service is not implemented yet; use --check to validate configuration")
}
