// THROWAWAY SPIKE CLI — see lib.rs.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    spike::run_cli(std::env::args().collect()).await
}
