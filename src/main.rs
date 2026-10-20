use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_verbosity_flag::{Verbosity, WarnLevel};
use pixi_oci::{BuildOptions, OutputFormat};

/// Build OCI images from pixi environments, without a container daemon.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    verbose: Verbosity<WarnLevel>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build an image from the `[tool.pixi-oci]` section of a pixi manifest.
    Build(BuildArgs),
    /// Generate shell completions.
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(clap::Args, Debug)]
struct BuildArgs {
    /// Path to `pixi.toml`, `pyproject.toml` or the directory containing it.
    #[arg(long, default_value = ".")]
    manifest_path: PathBuf,

    /// The pixi environment to install (default: `environment` from the config, else `default`).
    #[arg(short, long)]
    environment: Option<String>,

    /// The conda platform to build for, e.g. `linux-64` (default: see README).
    #[arg(short, long)]
    platform: Option<String>,

    /// Output path. `-` writes the tarball to stdout. Defaults to `<name>.tar` or `<name>/`.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::OciTar)]
    format: Format,

    /// Image reference as `name:tag` or `name`; overrides `tag` from the config.
    #[arg(short, long)]
    tag: Option<String>,

    /// A pixi/rattler config file with mirrors or authentication settings.
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum Format {
    /// OCI layout tarball, loadable with `docker load` and `podman load`.
    OciTar,
    /// OCI layout directory.
    OciDir,
}

impl From<Format> for OutputFormat {
    fn from(value: Format) -> Self {
        match value {
            Format::OciTar => OutputFormat::OciTar,
            Format::OciDir => OutputFormat::OciDir,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_max_level(cli.verbose)
        .with_writer(std::io::stderr)
        .init();

    match cli.command {
        Command::Build(args) => {
            let report = pixi_oci::build(BuildOptions {
                manifest_path: args.manifest_path,
                environment: args.environment,
                platform: args.platform,
                output: args.output,
                format: args.format.into(),
                tag: args.tag,
                rattler_config: args.config,
            })
            .await?;
            tracing::info!(
                "manifest {} config {} ({} base + {} package layers{})",
                report.manifest_digest,
                report.config_digest,
                report.base_layers,
                report.package_layers,
                if report.copy_layer {
                    " + copy layer"
                } else {
                    ""
                }
            );
        }
        Command::Completion { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        }
    }
    Ok(())
}
