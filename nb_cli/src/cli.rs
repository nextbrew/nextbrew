use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "nb")]
#[command(about = "NextBrew - A fast Homebrew-compatible package installer")]
#[command(version)]
pub struct Cli {
    #[arg(long, env = "NEXTBREW_ROOT", help = "Path to nextbrew data directory")]
    pub root: Option<PathBuf>,

    #[arg(
        long,
        help = "Path to Homebrew-style prefix (overrides NEXTBREW_PREFIX)"
    )]
    pub prefix: Option<PathBuf>,

    #[arg(
        long,
        default_value = "20",
        value_parser = parse_concurrency,
        help = "Number of concurrent download threads"
    )]
    pub concurrency: usize,

    #[arg(
        long = "auto-init",
        global = true,
        env = "NEXTBREW_AUTO_INIT",
        help = "Automatically initialize without prompting"
    )]
    pub auto_init: bool,

    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count, help = "Increase output verbosity")]
    pub verbose: u8,

    #[arg(
        long,
        short = 'q',
        global = true,
        conflicts_with = "verbose",
        help = "Suppress output (only show errors)"
    )]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Commands,
}

fn parse_concurrency(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid value '{}': expected a positive integer", value))?;
    if parsed == 0 {
        return Err("concurrency must be at least 1".to_string());
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn accepts_positive_concurrency() {
        let cli = Cli::try_parse_from(["nb", "--concurrency", "4", "list"]).unwrap();
        assert_eq!(cli.concurrency, 4);
    }

    #[test]
    fn rejects_zero_concurrency() {
        let result = Cli::try_parse_from(["nb", "--concurrency", "0", "list"]);
        assert!(result.is_err());
        let err = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("at least 1"));
    }

    #[test]
    fn accepts_verbose_levels() {
        let cli = Cli::try_parse_from(["nb", "-vv", "list"]).unwrap();
        assert_eq!(cli.verbose, 2);
        assert!(!cli.quiet);
    }

    #[test]
    fn rejects_quiet_with_verbose() {
        let result = Cli::try_parse_from(["nb", "-v", "-q", "list"]);
        assert!(result.is_err());
    }

    #[test]
    fn outdated_quiet_and_verbose_conflict() {
        let result = Cli::try_parse_from(["nb", "outdated", "--quiet", "--verbose"]);
        assert!(result.is_err());
    }

    #[test]
    fn outdated_quiet_and_json_conflict() {
        let result = Cli::try_parse_from(["nb", "outdated", "--quiet", "--json"]);
        assert!(result.is_err());
    }

    #[test]
    fn outdated_verbose_and_json_conflict() {
        let result = Cli::try_parse_from(["nb", "outdated", "--verbose", "--json"]);
        assert!(result.is_err());
    }
}

#[derive(Subcommand)]
pub enum Commands {
    /// Install formulas and casks
    Install {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
        #[arg(long, help = "Do not create symlinks after installation")]
        no_link: bool,
        #[arg(long, short = 's', help = "Build from source instead of using bottles")]
        build_from_source: bool,
    },
    /// Install or dump from a Brewfile
    #[command(args_conflicts_with_subcommands = true)]
    Bundle {
        #[command(flatten)]
        install: BundleInstallArgs,
        #[command(subcommand)]
        command: Option<BundleCommands>,
    },
    /// Uninstall formulas and casks
    Uninstall {
        #[arg(required_unless_present = "all", num_args = 1..)]
        formulas: Vec<String>,
        #[arg(long, help = "Uninstall all installed packages")]
        all: bool,
        #[arg(long, help = "Also remove the casks' user data (zap stanza)")]
        zap: bool,
    },
    /// Uninstall formulae that were only installed as dependencies and are no longer needed
    Autoremove {
        #[arg(
            long,
            short = 'n',
            help = "List what would be removed without removing it"
        )]
        dry_run: bool,
    },
    /// Migrate packages from Homebrew
    Migrate {
        #[arg(long, short = 'y', help = "Skip confirmation prompts")]
        yes: bool,
        #[arg(long, help = "Force uninstall from Homebrew even if errors occur")]
        force: bool,
    },
    /// List installed packages
    List {
        #[arg(long, help = "List only pinned formulae, by name")]
        pinned: bool,
    },
    /// Show information about an installed package
    Info {
        #[arg(help = "Name of the installed package")]
        formula: String,
    },
    /// Run diagnostics and optionally repair issues
    Doctor {
        #[arg(long, help = "Automatically repair detected issues")]
        repair: bool,
    },
    /// Remove unreferenced store entries
    Gc,
    /// Reset nextbrew data directories
    Reset {
        #[arg(long, short = 'y', help = "Skip confirmation prompts")]
        yes: bool,
    },
    /// Initialize nextbrew directories
    Init {
        #[arg(long, help = "Do not modify shell configuration files")]
        no_modify_path: bool,
    },
    /// Generate shell completions
    Completion {
        #[arg(
            value_enum,
            help = "Target shell for completions (e.g., bash, zsh, fish)"
        )]
        shell: clap_complete::shells::Shell,
    },
    /// Run an installed formula as a command
    Run {
        #[arg(help = "Name of the formula to run")]
        formula: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Refresh cached formula metadata
    Update,
    /// List installed packages with newer versions available
    Outdated {
        #[arg(long, conflicts_with_all = ["quiet", "verbose"], help = "Output as JSON")]
        json: bool,
    },
    /// Upgrade installed packages to the latest versions
    Upgrade {
        #[arg(required = false, num_args = 0..)]
        formulas: Vec<String>,
        #[arg(long, short = 's', help = "Build from source instead of using bottles")]
        build_from_source: bool,
        #[arg(long, help = "Do not create symlinks after installation")]
        no_link: bool,
    },
    /// Search formulae and casks by name, or by /regex/
    Search {
        query: String,
        #[arg(long, conflicts_with = "cask", help = "Only search formulae")]
        formula: bool,
        #[arg(long, help = "Only search casks")]
        cask: bool,
    },
    /// Show the runtime dependencies of formulae
    Deps {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
        #[arg(
            long,
            short = '1',
            help = "Only the dependencies the formulae name themselves"
        )]
        direct: bool,
        #[arg(
            long,
            help = "Show the dependencies of any of the formulae, not only shared ones"
        )]
        union: bool,
    },
    /// Show the formulae and casks that depend on formulae
    Uses {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
        #[arg(long, help = "Only list installed formulae and casks")]
        installed: bool,
        #[arg(long, help = "Also count dependencies of dependencies")]
        recursive: bool,
        #[arg(
            long,
            visible_alias = "formulae",
            conflicts_with = "cask",
            help = "Only list formulae"
        )]
        formula: bool,
        #[arg(long, visible_alias = "casks", help = "Only list casks")]
        cask: bool,
    },
    /// Symlink installed formulae into the prefix
    Link {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
        #[arg(long, short = 'f', help = "Also link keg-only formulae")]
        force: bool,
    },
    /// Remove installed formulae's symlinks from the prefix
    Unlink {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
    },
    /// Keep installed formulae at their versions on upgrade
    Pin {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
    },
    /// Let pinned formulae be upgraded again
    Unpin {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
    },
    /// Run the post-install steps of installed packages again
    Postinstall {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
    },
    /// Manage background services with launchd
    Services {
        #[command(subcommand)]
        command: Option<ServicesCommands>,
    },
}

/// `nb bundle [install]`'s options; `install` is the default subcommand.
#[derive(clap::Args, Clone, Default)]
pub struct BundleInstallArgs {
    #[arg(
        long,
        value_name = "FILE",
        help = "Read this Brewfile (`-` for standard input) instead of ./Brewfile or $HOMEBREW_BUNDLE_FILE"
    )]
    pub file: Option<PathBuf>,
    #[arg(long, help = "Read ~/.Brewfile (or $HOMEBREW_BUNDLE_FILE_GLOBAL)")]
    pub global: bool,
    #[arg(long, help = "Do not upgrade outdated dependencies")]
    pub no_upgrade: bool,
    #[arg(
        long,
        help = "Upgrade outdated dependencies even if $HOMEBREW_BUNDLE_NO_UPGRADE is set"
    )]
    pub upgrade: bool,
    #[arg(
        long,
        visible_alias = "upgrade-formula",
        value_name = "FORMULAE",
        help = "Upgrade these comma-separated formulae even with --no-upgrade"
    )]
    pub upgrade_formulae: Option<String>,
    #[arg(long, help = "Print only failures and the summary")]
    pub quiet: bool,
}

/// `brew services`' subcommands, with its aliases.
#[derive(Subcommand)]
pub enum ServicesCommands {
    /// List all managed services for the current user (or root)
    #[command(visible_alias = "ls")]
    List {
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Show a service's status
    #[command(visible_alias = "i")]
    Info {
        formulas: Vec<String>,
        #[arg(long, help = "List all managed services")]
        all: bool,
        #[arg(long, help = "Output as JSON")]
        json: bool,
    },
    /// Run a service without registering it to launch at login (or boot)
    Run {
        formulas: Vec<String>,
        #[arg(long, help = "Run all services")]
        all: bool,
        #[arg(long, help = "Use the service file from this location")]
        file: Option<PathBuf>,
    },
    /// Start a service now and register it to launch at login (or boot)
    #[command(visible_aliases = ["launch", "load", "s", "l"])]
    Start {
        formulas: Vec<String>,
        #[arg(long, help = "Start all services")]
        all: bool,
        #[arg(long, help = "Use the service file from this location")]
        file: Option<PathBuf>,
    },
    /// Stop a service and unregister it from launching at login (or boot)
    #[command(visible_aliases = ["unload", "terminate", "term", "t", "u"])]
    Stop {
        formulas: Vec<String>,
        #[arg(long, help = "Stop all services")]
        all: bool,
        #[arg(long, help = "Keep the service registered")]
        keep: bool,
        #[arg(
            long,
            conflicts_with = "max_wait",
            help = "Don't wait for the service to stop"
        )]
        no_wait: bool,
        #[arg(
            long,
            value_name = "SECONDS",
            help = "Wait at most this long for the service to stop (default 60, 0 waits indefinitely)"
        )]
        max_wait: Option<f64>,
    },
    /// Stop a service now but keep it registered
    #[command(visible_alias = "k")]
    Kill {
        formulas: Vec<String>,
        #[arg(long, help = "Kill all services")]
        all: bool,
    },
    /// Stop (if necessary) and start a service again
    #[command(visible_aliases = ["relaunch", "reload", "r"])]
    Restart {
        formulas: Vec<String>,
        #[arg(long, help = "Restart all services")]
        all: bool,
        #[arg(long, help = "Use the service file from this location")]
        file: Option<PathBuf>,
    },
    /// Remove all unused services
    #[command(visible_aliases = ["clean", "cl", "rm"])]
    Cleanup,
}

#[derive(Subcommand)]
pub enum BundleCommands {
    /// Install and upgrade everything in a Brewfile
    #[command(visible_alias = "upgrade")]
    Install(BundleInstallArgs),
    /// Dump installed packages to a Brewfile
    Dump {
        #[arg(
            long,
            short = 'f',
            value_name = "FILE",
            default_value = "Brewfile",
            help = "Output file path"
        )]
        file: PathBuf,
        #[arg(long, help = "Overwrite existing file")]
        force: bool,
    },
}
