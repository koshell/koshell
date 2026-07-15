//! Command-line interface for the `koshell` binary.
//!
//! Without arguments koshell launches the default shell (with shell integration for
//! bash/zsh). `shell-init <shell>` prints the rc snippet for
//! `eval "$(koshell shell-init zsh)"`-style auto-wrap installs. Any other leading
//! positional launches that program directly instead of the default shell —
//! `koshell python3 -i` runs `python3 -i` in the PTY with `#?` armed through the
//! non-integrated capture path; everything after the program name belongs to it, and
//! `--` allows a program whose name starts with a dash. Unknown dashed arguments before
//! the program are rejected so the option namespace stays reserved for future flags.
//!
//! `shell-init`, `daemon`, `auth`, `model`, `preflight`, `status`, and `reload` shadow
//! programs literally named that; launching such a program requires a path form (for example
//! `koshell ./shell-init` or `koshell ./daemon`). Accepted residual of reserving the names.

use clap::{Parser, Subcommand};

use crate::shell_init::InitShell;

/// koshell — a human-centric shared terminal: AI beside your terminal, not above it.
#[derive(Debug, Parser)]
#[command(
    name = "koshell",
    version,
    about,
    max_term_width = 100,
    after_help = "Run `koshell <command> [args...]` to launch a program directly \
                  instead of the default shell (for example `koshell python3 -i`)."
)]
pub struct Cli {
    /// Log level filter (error, warn, info, debug, trace, off; env_logger module
    /// filters also work). Overrides the KOSHELL_LOG environment variable. Logs are
    /// written to a file under the XDG state directory, never to the terminal.
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,

    /// What to run; omit to launch the default shell.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Print the startup snippet that makes new interactive shells exec into koshell;
    /// install it as `eval "$(koshell shell-init zsh)"` at the top of ~/.zshrc (bash:
    /// ~/.bashrc).
    ShellInit {
        /// Shell dialect to emit the snippet for.
        #[arg(value_enum, value_name = "SHELL")]
        shell: InitShell,
    },

    /// Fast, TTY-free readiness probe used by the auto-wrap snippet as
    /// `koshell preflight && exec koshell`: it exits 0 only when koshell can start and a
    /// real shell is resolvable, so the snippet keeps the current shell instead of
    /// `exec`-ing into a koshell that would immediately fail and close the terminal.
    Preflight,

    /// Inspect or control the AI daemon (status, start, stop, restart). The
    /// terminal auto-starts the daemon on demand; these are for manual control.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Manage AI provider credentials: OAuth login/logout and status.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },

    /// Discover, inspect, and select the AI model. With no action, opens a
    /// searchable interactive picker.
    Model {
        /// Change only the current conversation, leaving koshell.toml unchanged.
        /// Requires a live conversation inside Koshell.
        #[arg(long)]
        session_only: bool,

        #[command(subcommand)]
        action: Option<ModelAction>,
    },

    /// Report the current koshell instance's state: its daemon connection,
    /// conversation, and active model, plus a daemon summary. Run inside a
    /// koshell shell.
    Status,

    /// Reload koshell.toml into live sessions. By default only the current
    /// instance's conversation resets (its next `#?` uses the new config);
    /// `--all` resets every active instance. Does not start the daemon.
    Reload {
        /// Reset every active instance, not just the current one.
        #[arg(long)]
        all: bool,
    },

    /// Program to launch instead of the default shell, with its arguments.
    #[command(external_subcommand)]
    Launch(Vec<String>),
}

/// Manual lifecycle actions for the AI daemon.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum DaemonAction {
    /// Report whether the daemon is running, and its identity if so.
    Status,
    /// Start the daemon if it is not already running.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Restart the daemon (stop if running, then start).
    Restart,
}

/// Credential actions for `koshell auth`.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum AuthAction {
    /// Sign in to an OAuth provider (for example anthropic, github-copilot,
    /// openai-codex). Ctrl-C aborts the flow.
    Login {
        /// Provider id to sign in to.
        provider: String,
    },
    /// Remove the stored credential for a provider.
    Logout {
        /// Provider id to sign out of.
        provider: String,
    },
    /// Show credential status, for all known providers or one.
    Status {
        /// Provider id to report on; omit for all.
        provider: Option<String>,
    },
}

/// Actions for `koshell model`; omitting one opens the interactive picker.
#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum ModelAction {
    /// Show the configured default and this conversation's active model.
    Show,
    /// List credential-available models, optionally filtered by a query.
    List {
        /// Include models whose credentials are not currently available.
        #[arg(long)]
        all: bool,
        /// Search provider id, model id, and display name.
        query: Option<String>,
    },
    /// Select a validated provider/id without opening the picker.
    Set {
        /// Model reference as provider/id.
        model: String,
        /// Change only the current conversation, leaving koshell.toml unchanged.
        #[arg(long)]
        session_only: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("koshell").chain(args.iter().copied()))
    }

    fn launch(cli: &Cli) -> &[String] {
        match &cli.command {
            Some(Command::Launch(command)) => command,
            other => panic!("expected a program launch, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_means_default_shell() {
        let cli = parse(&[]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn positional_command_with_dashed_arguments_passes_through() {
        let cli = parse(&["python3", "-i", "--version"]).unwrap();
        assert_eq!(launch(&cli), ["python3", "-i", "--version"]);
    }

    #[test]
    fn log_level_before_command_is_koshell_s_and_after_belongs_to_the_program() {
        let cli = parse(&["--log-level", "debug", "python3", "-i"]).unwrap();
        assert_eq!(cli.log_level.as_deref(), Some("debug"));
        assert_eq!(launch(&cli), ["python3", "-i"]);

        let cli = parse(&["python3", "--log-level", "debug"]).unwrap();
        assert_eq!(cli.log_level, None);
        assert_eq!(launch(&cli), ["python3", "--log-level", "debug"]);
    }

    #[test]
    fn unknown_option_before_command_is_rejected() {
        let error = parse(&["--bogus", "python3"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        let error = parse(&["-x"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn double_dash_allows_dashed_command_names() {
        let cli = parse(&["--", "--weird-name", "arg"]).unwrap();
        assert_eq!(launch(&cli), ["--weird-name", "arg"]);
    }

    #[test]
    fn help_and_version_are_native() {
        let error = parse(&["--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let error = parse(&["--version"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn shell_init_parses_supported_shells() {
        let cli = parse(&["shell-init", "zsh"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::ShellInit {
                shell: InitShell::Zsh
            })
        );
        let cli = parse(&["shell-init", "bash"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::ShellInit {
                shell: InitShell::Bash
            })
        );
    }

    #[test]
    fn shell_init_requires_a_supported_shell() {
        assert!(parse(&["shell-init"]).is_err());
        let error = parse(&["shell-init", "fish"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn a_program_named_like_the_subcommand_needs_a_path_form() {
        // Reserved name: plain `shell-init` is the subcommand.
        assert!(matches!(
            parse(&["shell-init", "zsh"]).unwrap().command,
            Some(Command::ShellInit { .. })
        ));
        // A path spelling still launches a real program of that name.
        let cli = parse(&["./shell-init", "zsh"]).unwrap();
        assert_eq!(launch(&cli), ["./shell-init", "zsh"]);
    }

    #[test]
    fn preflight_parses_and_a_program_named_preflight_needs_a_path_form() {
        assert_eq!(
            parse(&["preflight"]).unwrap().command,
            Some(Command::Preflight)
        );
        // A path spelling still launches a real program of that name.
        let cli = parse(&["./preflight", "--check"]).unwrap();
        assert_eq!(launch(&cli), ["./preflight", "--check"]);
    }

    #[test]
    fn status_parses_and_a_program_named_status_needs_a_path_form() {
        assert_eq!(parse(&["status"]).unwrap().command, Some(Command::Status));
        let cli = parse(&["./status", "--json"]).unwrap();
        assert_eq!(launch(&cli), ["./status", "--json"]);
    }

    #[test]
    fn reload_parses_with_and_without_all() {
        assert_eq!(
            parse(&["reload"]).unwrap().command,
            Some(Command::Reload { all: false })
        );
        assert_eq!(
            parse(&["reload", "--all"]).unwrap().command,
            Some(Command::Reload { all: true })
        );
        // A path spelling still launches a real program of that name.
        let cli = parse(&["./reload", "--now"]).unwrap();
        assert_eq!(launch(&cli), ["./reload", "--now"]);
    }

    #[test]
    fn daemon_subcommand_parses_each_action() {
        for (arg, expected) in [
            ("status", DaemonAction::Status),
            ("start", DaemonAction::Start),
            ("stop", DaemonAction::Stop),
            ("restart", DaemonAction::Restart),
        ] {
            let cli = parse(&["daemon", arg]).unwrap();
            assert_eq!(cli.command, Some(Command::Daemon { action: expected }));
        }
    }

    #[test]
    fn daemon_requires_an_action() {
        assert!(parse(&["daemon"]).is_err());
        let error = parse(&["daemon", "bogus"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn a_program_named_daemon_needs_a_path_form() {
        // Reserved name: plain `daemon` is the subcommand.
        assert!(matches!(
            parse(&["daemon", "status"]).unwrap().command,
            Some(Command::Daemon { .. })
        ));
        // A path spelling still launches a real program of that name.
        let cli = parse(&["./daemon", "status"]).unwrap();
        assert_eq!(launch(&cli), ["./daemon", "status"]);
    }

    #[test]
    fn auth_subcommand_parses_each_action() {
        let cli = parse(&["auth", "login", "anthropic"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Auth {
                action: AuthAction::Login {
                    provider: "anthropic".to_string()
                }
            })
        );
        let cli = parse(&["auth", "logout", "github-copilot"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Auth {
                action: AuthAction::Logout {
                    provider: "github-copilot".to_string()
                }
            })
        );
        let cli = parse(&["auth", "status"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Auth {
                action: AuthAction::Status { provider: None }
            })
        );
        let cli = parse(&["auth", "status", "openai-codex"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Auth {
                action: AuthAction::Status {
                    provider: Some("openai-codex".to_string())
                }
            })
        );
    }

    #[test]
    fn auth_requires_an_action_and_login_requires_a_provider() {
        assert!(parse(&["auth"]).is_err());
        assert!(parse(&["auth", "login"]).is_err());
        assert!(parse(&["auth", "logout"]).is_err());
        let error = parse(&["auth", "bogus"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn a_program_named_auth_needs_a_path_form() {
        // Reserved name: plain `auth` is the subcommand.
        assert!(matches!(
            parse(&["auth", "status"]).unwrap().command,
            Some(Command::Auth { .. })
        ));
        // A path spelling still launches a real program of that name.
        let cli = parse(&["./auth", "status"]).unwrap();
        assert_eq!(launch(&cli), ["./auth", "status"]);
    }

    #[test]
    fn model_picker_and_actions_parse() {
        assert_eq!(
            parse(&["model"]).unwrap().command,
            Some(Command::Model {
                session_only: false,
                action: None,
            })
        );
        assert_eq!(
            parse(&["model", "--session-only"]).unwrap().command,
            Some(Command::Model {
                session_only: true,
                action: None,
            })
        );
        assert_eq!(
            parse(&["model", "show"]).unwrap().command,
            Some(Command::Model {
                session_only: false,
                action: Some(ModelAction::Show),
            })
        );
        assert_eq!(
            parse(&["model", "list", "--all", "sonnet"])
                .unwrap()
                .command,
            Some(Command::Model {
                session_only: false,
                action: Some(ModelAction::List {
                    all: true,
                    query: Some("sonnet".to_string()),
                }),
            })
        );
        assert_eq!(
            parse(&[
                "model",
                "set",
                "openrouter/anthropic/claude",
                "--session-only",
            ])
            .unwrap()
            .command,
            Some(Command::Model {
                session_only: false,
                action: Some(ModelAction::Set {
                    model: "openrouter/anthropic/claude".to_string(),
                    session_only: true,
                }),
            })
        );
    }

    #[test]
    fn a_program_named_model_needs_a_path_form() {
        assert!(matches!(
            parse(&["model", "show"]).unwrap().command,
            Some(Command::Model { .. })
        ));
        let cli = parse(&["./model", "show"]).unwrap();
        assert_eq!(launch(&cli), ["./model", "show"]);
    }
}
