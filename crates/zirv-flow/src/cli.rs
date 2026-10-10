use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "zirv-flow", version, about)]
pub(crate) struct Cli {
    /// Read configuration from this TOML file.
    #[arg(long, global = true, env = "ZIRV_FLOW_CONFIG", value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Run the server, worker, and scheduler roles in one process.
    All,
    /// Serve the API, ingestion endpoints, webhooks, assets, UI, and health endpoints.
    Serve,
    /// Claim and execute jobs, with dedicated pools selectable by job kind.
    Worker,
    /// Schedule due jobs and maintain leases, partitions, retention, and source status.
    Scheduler,
    /// Apply database migrations.
    Migrate,
    /// Create an owner, rotate the master key, or replay erasures.
    Admin,
    /// Validate configuration and print it with secrets redacted.
    #[command(subcommand)]
    Config(ConfigCommand),
}

impl Command {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Serve => "serve",
            Self::Worker => "worker",
            Self::Scheduler => "scheduler",
            Self::Migrate => "migrate",
            Self::Admin => "admin",
            Self::Config(_) => "config",
        }
    }
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConfigCommand {
    /// Validate configuration and print the effective values with secrets redacted.
    Check,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_is_recognised() {
        let error = Cli::try_parse_from(["zirv-flow", "--version"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn help_lists_every_role() {
        let error = Cli::try_parse_from(["zirv-flow", "--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string();

        for command in [
            "all",
            "serve",
            "worker",
            "scheduler",
            "migrate",
            "admin",
            "config",
        ] {
            assert!(help.contains(command), "help did not list {command}");
        }
    }
}
