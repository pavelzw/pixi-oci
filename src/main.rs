use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_verbosity_flag::{Verbosity, WarnLevel};
use pixi_oci::{BuildOptions, InjectSpec, OutputFormat};
use rattler_conda_types::PackageName;

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

    /// The conda platform to build for, e.g. `linux-64` (default: see README). Give it
    /// multiple times or comma-separated to build a multi-platform image.
    #[arg(short, long, value_delimiter = ',')]
    platform: Vec<String>,

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

    /// Inject an additional local conda package (`.conda` or `.tar.bz2`) into the environment.
    /// Replaces a locked package of the same name. Prefix with `PLATFORM:` to inject it only
    /// for that platform. Can be given multiple times.
    #[arg(short, long, value_name = "[PLATFORM:]PATH")]
    inject: Vec<InjectSpec>,

    /// Leave a locked conda package out of the environment. Combine with `--inject` to replace
    /// a source package with a package built by `pixi build`. Can be given multiple times.
    #[arg(long, value_name = "PACKAGE")]
    skip: Vec<PackageName>,

    /// Don't check that the dependencies and constraints of all packages are satisfied by the
    /// environment, e.g. after `--skip` or `--inject`.
    #[arg(long)]
    skip_validation: bool,
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

    // Ctrl-C would otherwise leave the staging directories (often gigabytes) and a partial
    // output behind, as the process is terminated without running destructors.
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n🛑 Interrupted, cleaning up");
            let _guard = pixi_oci::cleanup::clean_up();
            std::process::exit(130);
        }
    });

    match cli.command {
        Command::Build(args) => {
            let report = pixi_oci::build(BuildOptions {
                manifest_path: args.manifest_path,
                environment: args.environment,
                platforms: args.platform,
                output: args.output,
                format: args.format.into(),
                tag: args.tag,
                rattler_config: args.config,
                inject: args.inject,
                skip: args.skip,
                skip_validation: args.skip_validation,
            })
            .await?;
            if let Some(index) = &report.index_digest {
                tracing::info!("index {index}");
            }
            for image in &report.images {
                tracing::info!(
                    "{}: manifest {} config {} ({} base + {} package layers{})",
                    image.subdir.as_str(),
                    image.manifest_digest,
                    image.config_digest,
                    image.base_layers,
                    image.package_layers,
                    if image.copy_layer {
                        " + copy layer"
                    } else {
                        ""
                    }
                );
            }
        }
        Command::Completion { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        }
    }
    Ok(())
}
