use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "hexdb", about = "HexDB CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check health of a node
    Health {
        #[arg(short, long, default_value = "http://localhost:8080")]
        url: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Health { url } => {
            let res = reqwest::get(format!("{}/health", url)).await?;
            let body = res.text().await?;
            println!("Health: {}", body);
        }
    }

    Ok(())
}
