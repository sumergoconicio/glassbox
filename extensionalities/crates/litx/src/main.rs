use clap::{Args, Parser, Subcommand, ValueEnum};
use liteparse::LiteParseConfig;
use litx_inspect::{InspectOptions, inspect};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about = "LiteParse Extended scaffold (not a lit replacement)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Inspect file identity, native content, structure, and embedded previews offline.
    Inspect(InspectArgs),
    /// Print the upstream library's default configuration as JSON.
    Defaults,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Json,
    Text,
}

#[derive(Debug, Args)]
struct InspectArgs {
    path: PathBuf,
    #[arg(long, default_value_t = 16_000)]
    max_chars: usize,
    #[arg(long, default_value_t = 5)]
    max_units: usize,
    #[arg(long)]
    thumbnail: Option<PathBuf>,
    #[arg(long, default_value_t = 320, value_parser = clap::value_parser!(u32).range(1..=4096))]
    thumbnail_max_edge: u32,
    #[arg(long)]
    packet: bool,
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
    #[arg(long)]
    probe_ocr: bool,
    #[arg(long)]
    pretty: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Inspect(args) => {
            let report = inspect(
                &args.path,
                &InspectOptions {
                    max_chars: args.max_chars,
                    max_units: args.max_units,
                    thumbnail: args.thumbnail,
                    thumbnail_max_edge: args.thumbnail_max_edge,
                    packet: args.packet,
                    probe_ocr: args.probe_ocr,
                },
            );
            match args.format {
                Format::Text => println!("{}", report.quick_look()),
                Format::Json if args.pretty => {
                    println!("{}", serde_json::to_string_pretty(&report)?)
                }
                Format::Json => println!("{}", serde_json::to_string(&report)?),
            }
        }
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
