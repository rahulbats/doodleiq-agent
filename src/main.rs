#[tokio::main]
async fn main() {
    if let Err(error) = doodleiq::run_cli().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
