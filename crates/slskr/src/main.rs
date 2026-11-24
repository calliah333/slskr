#[tokio::main]
async fn main() -> anyhow::Result<()> {
    slskr::run(slskr::Cli::parse()).await
}
