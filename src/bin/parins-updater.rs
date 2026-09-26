#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = parins::update::executor::run_cli().await {
        eprintln!("PariNS updater: {error}");
        std::process::exit(1);
    }
}
