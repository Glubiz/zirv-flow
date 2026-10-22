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
    /// Probe this instance's /readyz and exit 0 when it is ready (for container health checks).
    Healthcheck,
    /// Administer the instance: create the owner, reset a password, generate a master key or
    /// replay the erasure ledger after a restore.
    #[command(subcommand)]
    Admin(AdminCommand),
    /// Validate configuration and print it with secrets redacted.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Internal: the render host. Serves length-prefixed JSON render requests on standard input
    /// and output until the input closes. Started by the render pool, never by an operator.
    #[command(hide = true)]
    RenderHost {
        /// Address space to limit this process to, in bytes (Linux only).
        #[arg(long, value_name = "BYTES")]
        memory_limit_bytes: Option<u64>,
    },
}

impl Command {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Serve => "serve",
            Self::Worker => "worker",
            Self::Scheduler => "scheduler",
            Self::Migrate => "migrate",
            Self::Healthcheck => "healthcheck",
            Self::Admin(_) => "admin",
            Self::Config(_) => "config",
            Self::RenderHost { .. } => "render-host",
        }
    }
}

#[derive(Subcommand, Debug)]
pub(crate) enum AdminCommand {
    /// Create the instance owner. Reads the password from one line of standard input.
    CreateOwner {
        /// The owner's e-mail address.
        #[arg(long)]
        email: String,
    },
    /// Set a new password for a user and end their sessions. Reads the password from one
    /// line of standard input.
    ResetPassword {
        /// The user's e-mail address.
        #[arg(long)]
        email: String,
    },
    /// Write a new random master key to a file, readable only by the owner. Needs no
    /// configuration or database.
    GenerateMasterKey {
        /// The file to create, with its parent directories.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
        /// When the file exists and holds a valid key, keep it and succeed.
        #[arg(long)]
        if_missing: bool,
    },
    /// Re-apply the erasure ledger after restoring a backup: re-add the erased suppressions,
    /// erase every ledger person that exists again and drop pending inbox rows naming an
    /// erased identifier. Prints counts only. Run it after every restore, before starting the
    /// workers. Needs the master key and no password.
    ReplayErasures {
        /// Only this workspace, by slug. All workspaces when omitted.
        #[arg(long, value_name = "SLUG")]
        workspace: Option<String>,
    },
    /// Re-wrap every stored data key, and the HMAC root, with the current master key, in one
    /// transaction, and print the count. Ciphertexts are untouched, and a second run re-wraps
    /// nothing. Configure the new key as secrets.master_key_file and the old one as
    /// secrets.previous_master_key_file, restart, run this, then remove the previous key.
    /// Needs the master keys and no password.
    RotateMasterKey,
    /// Runs the assistant's golden prompts against the workspace's configured model. Every prompt
    /// is a gated call that counts against the workspace's budget. Prints a JSON report; never
    /// fails on a low score. Needs the master key and no password.
    EvaluateAssistant {
        /// The workspace, by slug.
        #[arg(long, value_name = "SLUG")]
        workspace: String,
        /// Only this case, by name. Every case when omitted.
        #[arg(long, value_name = "NAME")]
        case: Option<String>,
    },
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
    fn the_render_host_is_a_subcommand_that_help_does_not_list() {
        let parsed =
            Cli::try_parse_from(["zirv-flow", "render-host", "--memory-limit-bytes", "7"]).unwrap();
        assert!(matches!(
            parsed.command,
            Some(Command::RenderHost {
                memory_limit_bytes: Some(7)
            })
        ));
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("render-host"), "{help}");
        assert!(help.contains("serve"), "{help}");
    }

    #[test]
    fn version_flag_is_recognised() {
        let error = Cli::try_parse_from(["zirv-flow", "--version"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn the_admin_subcommands_take_an_email_and_never_a_password_argument() {
        let create =
            Cli::try_parse_from(["zirv-flow", "admin", "create-owner", "--email", "a@b.c"])
                .unwrap();
        let reset =
            Cli::try_parse_from(["zirv-flow", "admin", "reset-password", "--email", "a@b.c"])
                .unwrap();

        assert!(matches!(
            create.command,
            Some(Command::Admin(AdminCommand::CreateOwner { .. }))
        ));
        assert!(matches!(
            reset.command,
            Some(Command::Admin(AdminCommand::ResetPassword { .. }))
        ));
        assert!(
            Cli::try_parse_from([
                "zirv-flow",
                "admin",
                "create-owner",
                "--email",
                "a@b.c",
                "--password",
                "secret"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["zirv-flow", "admin", "create-owner"]).is_err());
    }

    #[test]
    fn rotate_master_key_takes_no_arguments() {
        let parse = |args: &[&str]| {
            let mut full = vec!["zirv-flow", "admin", "rotate-master-key"];
            full.extend(args);
            Cli::try_parse_from(full)
        };

        assert!(matches!(
            parse(&[]).unwrap().command,
            Some(Command::Admin(AdminCommand::RotateMasterKey))
        ));
        assert!(parse(&["--out", "k"]).is_err());
    }

    #[test]
    fn generate_master_key_takes_an_output_path_and_an_optional_if_missing_flag() {
        let parse = |args: &[&str]| {
            let mut full = vec!["zirv-flow", "admin", "generate-master-key"];
            full.extend(args);
            Cli::try_parse_from(full)
        };

        let plain = parse(&["--out", "k"]).unwrap();
        let keep = parse(&["--out", "k", "--if-missing"]).unwrap();

        assert!(matches!(
            plain.command,
            Some(Command::Admin(AdminCommand::GenerateMasterKey {
                if_missing: false,
                ..
            }))
        ));
        assert!(matches!(
            keep.command,
            Some(Command::Admin(AdminCommand::GenerateMasterKey {
                if_missing: true,
                ..
            }))
        ));
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn replay_erasures_takes_an_optional_workspace_and_no_password() {
        let all = Cli::try_parse_from(["zirv-flow", "admin", "replay-erasures"]).unwrap();
        let one = Cli::try_parse_from([
            "zirv-flow",
            "admin",
            "replay-erasures",
            "--workspace",
            "acme",
        ])
        .unwrap();

        assert!(matches!(
            all.command,
            Some(Command::Admin(AdminCommand::ReplayErasures {
                workspace: None
            }))
        ));
        match one.command {
            Some(Command::Admin(AdminCommand::ReplayErasures { workspace })) => {
                assert_eq!(workspace.as_deref(), Some("acme"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["zirv-flow", "admin", "replay-erasures", "--password", "x"])
                .is_err()
        );
    }
    #[test]
    fn evaluate_assistant_parses_workspace_and_case() {
        let all = Cli::try_parse_from([
            "zirv-flow",
            "admin",
            "evaluate-assistant",
            "--workspace",
            "acme",
        ])
        .unwrap();
        let one = Cli::try_parse_from([
            "zirv-flow",
            "admin",
            "evaluate-assistant",
            "--workspace",
            "acme",
            "--case",
            "csv-contacts",
        ])
        .unwrap();

        match all.command {
            Some(Command::Admin(AdminCommand::EvaluateAssistant { workspace, case })) => {
                assert_eq!((workspace.as_str(), case), ("acme", None));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match one.command {
            Some(Command::Admin(AdminCommand::EvaluateAssistant { case, .. })) => {
                assert_eq!(case.as_deref(), Some("csv-contacts"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        // The workspace is required, and there is no password argument.
        assert!(Cli::try_parse_from(["zirv-flow", "admin", "evaluate-assistant"]).is_err());
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
            "healthcheck",
            "admin",
            "config",
        ] {
            assert!(help.contains(command), "help did not list {command}");
        }
    }
}
