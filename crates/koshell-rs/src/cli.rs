//! Command-line interface for the `koshell` binary.
//!
//! Without arguments koshell launches the default shell (with shell integration for
//! bash/zsh). A trailing positional command launches that program directly instead —
//! `koshell python3 -i` runs `python3 -i` in the PTY with `#?` armed through the
//! non-integrated capture path. Everything after the first positional belongs to the
//! child program, and `--` allows a command whose name starts with a dash. No real
//! options exist yet; unknown dashed arguments before the command are rejected so the
//! option namespace stays reserved for future flags.

use clap::Parser;

/// koshell — a human-centric shared terminal: AI beside your terminal, not above it.
#[derive(Debug, Parser)]
#[command(name = "koshell", version, about, max_term_width = 100)]
pub struct Cli {
    /// Program to launch instead of the default shell, with its arguments
    /// (for example `koshell python3 -i`). Omit to launch the default shell.
    #[arg(value_name = "COMMAND", trailing_var_arg = true)]
    pub command: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("koshell").chain(args.iter().copied()))
    }

    #[test]
    fn no_arguments_means_default_shell() {
        let cli = parse(&[]).unwrap();
        assert!(cli.command.is_empty());
    }

    #[test]
    fn positional_command_with_dashed_arguments_passes_through() {
        let cli = parse(&["python3", "-i", "--version"]).unwrap();
        assert_eq!(cli.command, ["python3", "-i", "--version"]);
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
        assert_eq!(cli.command, ["--weird-name", "arg"]);
    }

    #[test]
    fn help_and_version_are_native() {
        let error = parse(&["--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let error = parse(&["--version"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    }
}
