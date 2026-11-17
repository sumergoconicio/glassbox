use clap::{Parser, Subcommand};
use liteparse::LiteParseConfig;

#[derive(Parser, Debug)]
#[command(version, about = "LiteParse Extended scaffold (not a lit replacement)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Print the upstream library's default configuration as JSON.
    Defaults,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Defaults => {
            println!(
                "{}",
                serde_json::to_string_pretty(&LiteParseConfig::default())?
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_command_is_available() {
        let cli = Cli::try_parse_from(["litx", "defaults"]).unwrap();
        assert!(matches!(cli.command, Commands::Defaults));
    }

    #[test]
    fn upstream_defaults_serialize_as_json() {
        let config = serde_json::to_value(LiteParseConfig::default()).unwrap();
        assert!(config.is_object());
        assert_eq!(config["ocr_language"], "eng");
    }
}
