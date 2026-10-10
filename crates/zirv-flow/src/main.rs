mod cli;
mod config;

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command, ConfigCommand};
use crate::config::Config;

fn main() -> ExitCode {
    let cli = Cli::parse();
    ExitCode::from(run(
        cli,
        std::env::vars_os(),
        &mut io::stdout(),
        &mut io::stderr(),
    ))
}

fn run(
    cli: Cli,
    env: impl IntoIterator<Item = (OsString, OsString)>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    let config = match Config::load(cli.config.as_deref(), env) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(stderr, "error: {error}");
            return 2;
        }
    };

    let command = cli.command.unwrap_or(Command::All);
    if matches!(command, Command::Config(ConfigCommand::Check)) {
        match toml::to_string_pretty(&config) {
            Ok(output) => {
                let _ = write!(stdout, "{output}");
                return 0;
            }
            Err(_) => {
                let _ = writeln!(stderr, "error: config: could not serialise configuration");
                return 2;
            }
        }
    }

    let _ = writeln!(
        stderr,
        "error: the {} command is not implemented yet",
        command.name()
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database_env(url: &str) -> Vec<(OsString, OsString)> {
        vec![("ZIRV_FLOW_DATABASE__URL".into(), url.into())]
    }

    #[test]
    fn config_check_redacts_the_database_password() {
        let cli = Cli {
            config: None,
            command: Some(Command::Config(ConfigCommand::Check)),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run(
            cli,
            database_env("postgres://user:hunter2@localhost/database"),
            &mut stdout,
            &mut stderr,
        );
        let output = String::from_utf8(stdout).unwrap();

        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert!(!output.contains("hunter2"));
        assert!(output.contains("***"));
    }

    #[test]
    fn a_bare_invocation_runs_all() {
        let cli = Cli {
            config: None,
            command: None,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run(
            cli,
            database_env("postgres://localhost/database"),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "error: the all command is not implemented yet\n"
        );
    }

    #[test]
    fn a_configuration_error_exits_with_code_two() {
        let cli = Cli {
            config: None,
            command: Some(Command::Serve),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run(cli, [], &mut stdout, &mut stderr);

        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr).unwrap().contains("database.url"));
    }
}
