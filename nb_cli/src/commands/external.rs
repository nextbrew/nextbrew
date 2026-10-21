//! `nb <name>` for an installed tap's command, found as `brew <name>` finds
//! one: `cmd/<name>.rb`, then `cmd/brew-<name>.rb`, then `cmd/brew-<name>`,
//! each an executable file, in the taps in order, and loaded only once its
//! tap or the command itself is trusted. The Ruby ones run inside Homebrew,
//! so nextbrew refuses them. Unlike brew, nextbrew does not look on `PATH`
//! for `brew-<name>`: those are written for brew.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{CommandFactory, Parser};
use console::style;
use nb_io::tap::trust::{TrustKind, TrustStore};
use nb_io::tap::{TapName, Taps};

use crate::cli::{Cli, Commands};
use crate::utils::{get_prefix_path, get_root_path};
use nb_io::network::suggest::did_you_mean;

/// A tap's command for a name.
#[derive(Debug, PartialEq)]
enum Found {
    Executable(TapName, PathBuf),
    Ruby(TapName, PathBuf),
}

impl Found {
    fn tap(&self) -> &TapName {
        match self {
            Found::Executable(tap, _) | Found::Ruby(tap, _) => tap,
        }
    }
}

/// When the command line does not parse: run the tap command an unknown
/// subcommand names, or else fail as clap does.
pub fn dispatch(error: clap::Error) -> ! {
    if error.kind() == ErrorKind::InvalidSubcommand
        && let Some(ContextValue::String(name)) = error.get(ContextKind::InvalidSubcommand)
    {
        let argv: Vec<OsString> = std::env::args_os().collect();
        // The options before the command, read as they would be before any
        // other.
        let at = (1..argv.len()).find_map(|i| {
            if argv[i] != name.as_str() {
                return None;
            }
            let mut before = argv[..i].to_vec();
            before.push("list".into());
            // Only nextbrew's own command, not one of `bundle`'s and such.
            Cli::try_parse_from(before)
                .ok()
                .filter(|cli| matches!(cli.command, Commands::List { .. }))
                .map(|cli| (i, cli))
        });
        if let Some((i, cli)) = at {
            let root = get_root_path(cli.root);
            let prefix = get_prefix_path(cli.prefix, &root);
            let taps = Taps::new(&prefix);
            if let Some(found) = find(&taps, name) {
                let args: Vec<OsString> = argv[i + 1..].to_vec();
                let e = run(&taps, name, found, &args);
                eprintln!("{} {e}", style("error:").red().bold());
                std::process::exit(1);
            }
            unknown_command(&taps, name);
        }
    }
    error.exit()
}

/// brew's `UsageError` for an unknown command: the taps whose commands
/// were skipped as untrusted, the usage, then the error and the commands
/// it may have meant, all on standard error, with status 1.
fn unknown_command(taps: &Taps, name: &str) -> ! {
    let trust = TrustStore::new();
    for (tap, _) in taps.command_dirs() {
        let remote = taps.remote(&tap);
        let commands = taps.command_files(&tap);
        // `partially_trusted_tap?`: one trusted command is enough.
        let trusted = commands
            .keys()
            .any(|command| trust.trusted(TrustKind::Command, &tap, command, remote.as_deref()));
        if !commands.is_empty() && !trusted {
            eprintln!(
                "{} Skipping {tap} because it is not trusted. Run `nb trust {tap}` to trust it.",
                style("Warning:").yellow().bold()
            );
        }
    }
    eprintln!("{}", Cli::command().render_help());
    // `Commands.suggestion_message`: nextbrew's own commands and their
    // aliases first, then the taps' too.
    let own: Vec<String> = Cli::command()
        .get_subcommands()
        .flat_map(|command| {
            std::iter::once(command.get_name().to_string())
                .chain(command.get_all_aliases().map(str::to_string))
        })
        .collect();
    let mut suggestions = did_you_mean(name, &own);
    if suggestions.is_empty() {
        let mut all = own;
        for (tap, _) in taps.command_dirs() {
            all.extend(taps.command_files(&tap).into_keys());
        }
        suggestions = did_you_mean(name, &all);
    }
    let did_you_mean = if suggestions.is_empty() {
        String::new()
    } else {
        format!("\nDid you mean {}?", to_sentence(&suggestions))
    };
    eprintln!(
        "{} Invalid usage: Unknown command: nb {name}{did_you_mean}",
        style("Error:").red().bold()
    );
    std::process::exit(1);
}

/// `Utils::Text.to_sentence` with "or": `a`, `a or b`, `a, b, or c`.
fn to_sentence(words: &[String]) -> String {
    match words {
        [] => String::new(),
        [one] => one.clone(),
        [one, two] => format!("{one} or {two}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    }
}

/// `Homebrew::Trust.require_trusted_command!`, then the command in place
/// of nextbrew. Returns only why it could not run.
fn run(taps: &Taps, name: &str, found: Found, args: &[OsString]) -> nb_core::Error {
    let tap = found.tap();
    if let Err(e) =
        TrustStore::new().require(TrustKind::Command, tap, name, taps.remote(tap).as_deref())
    {
        return e;
    }
    match found {
        Found::Ruby(tap, path) => nb_core::Error::InvalidArgument {
            message: format!(
                "{tap}/{name} is a Ruby command ({}) that runs inside Homebrew, which nextbrew cannot do.\nRun `brew {name}` instead.",
                path.display()
            ),
        },
        Found::Executable(_, path) => {
            // brew runs it with the taps' `cmd` directories on `PATH`.
            let mut dirs: Vec<PathBuf> =
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
            dirs.extend(taps.command_dirs().into_iter().map(|(_, dir)| dir));
            let path_var = std::env::join_paths(dirs).unwrap_or_default();
            let error = std::process::Command::new(&path)
                .args(args)
                .env("PATH", path_var)
                .exec();
            nb_core::Error::ExecutionError {
                message: format!("failed to run {}: {error}", path.display()),
            }
        }
    }
}

/// `Commands.external_ruby_v2_cmd_path`, `external_ruby_cmd_path` and
/// `external_cmd_path`, among the taps' `cmd` directories.
fn find(taps: &Taps, name: &str) -> Option<Found> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let dirs = taps.command_dirs();
    let lookup = |file: &str| {
        dirs.iter().find_map(|(tap, dir)| {
            let path = dir.join(file);
            is_executable(&path).then(|| (tap.clone(), path))
        })
    };
    if let Some((tap, path)) =
        lookup(&format!("{name}.rb")).or_else(|| lookup(&format!("brew-{name}.rb")))
    {
        return Some(Found::Ruby(tap, path));
    }
    lookup(&format!("brew-{name}")).map(|(tap, path)| Found::Executable(tap, path))
}

/// `Utils::Shell.which`'s test: an executable file.
fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(dir: &Path, file: &str, mode: u32) {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(file);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn finds_a_taps_command_as_brew_does() {
        let tmp = tempfile::tempdir().unwrap();
        let taps = Taps::new(tmp.path());
        let a = TapName::parse("a/tools").unwrap();
        let b = TapName::parse("b/tools").unwrap();
        let (a_cmd, b_cmd) = (taps.path(&a).join("cmd"), taps.path(&b).join("cmd"));
        command(&a_cmd, "brew-hello", 0o755);
        command(&b_cmd, "brew-hello", 0o755);
        command(&a_cmd, "brew-quiet", 0o644);
        command(&b_cmd, "brew-ruby.rb", 0o755);
        command(&b_cmd, "brew-both", 0o755);
        command(&a_cmd, "both.rb", 0o755);

        assert_eq!(
            find(&taps, "hello"),
            Some(Found::Executable(a.clone(), a_cmd.join("brew-hello")))
        );
        // Not executable: not a command.
        assert_eq!(find(&taps, "quiet"), None);
        assert_eq!(
            find(&taps, "ruby"),
            Some(Found::Ruby(b.clone(), b_cmd.join("brew-ruby.rb")))
        );
        // A Ruby command comes before an executable of that name.
        assert_eq!(
            find(&taps, "both"),
            Some(Found::Ruby(a.clone(), a_cmd.join("both.rb")))
        );
        assert_eq!(find(&taps, "missing"), None);
        assert_eq!(find(&taps, "../cmd/brew-hello"), None);
    }

    #[test]
    fn lists_suggestions_as_a_sentence() {
        let words = |list: &[&str]| list.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert_eq!(to_sentence(&words(&["install"])), "install");
        assert_eq!(
            to_sentence(&words(&["install", "instal"])),
            "install or instal"
        );
        assert_eq!(to_sentence(&words(&["a", "b", "c"])), "a, b, or c");
    }

    #[test]
    fn tap_commands_are_listed_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let taps = Taps::new(tmp.path());
        let tap = TapName::parse("a/tools").unwrap();
        let dir = taps.path(&tap).join("cmd");
        command(&dir, "brew-hello", 0o755);
        command(&dir, "lint.rb", 0o644);
        let names: Vec<String> = taps.command_files(&tap).into_keys().collect();
        assert_eq!(names, vec!["hello".to_string(), "lint".to_string()]);
    }
}
