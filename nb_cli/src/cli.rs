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
        #[arg(
            long,
            visible_alias = "formulae",
            conflicts_with = "cask",
            help = "Treat the names as formulae"
        )]
        formula: bool,
        #[arg(long, visible_alias = "casks", help = "Treat the names as casks")]
        cask: bool,
        #[command(flatten)]
        cask_options: CaskOptions,
    },
    /// Uninstall and install formulae and casks again
    Reinstall {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
        #[arg(long, short = 's', help = "Build from source instead of using bottles")]
        build_from_source: bool,
        #[arg(
            long,
            visible_alias = "formulae",
            conflicts_with = "cask",
            help = "Treat the names as formulae"
        )]
        formula: bool,
        #[arg(long, visible_alias = "casks", help = "Treat the names as casks")]
        cask: bool,
        #[arg(long, help = "Also remove the casks' user data (zap stanza)")]
        zap: bool,
        #[command(flatten)]
        cask_options: CaskOptions,
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
        #[arg(
            long,
            visible_alias = "formulae",
            conflicts_with = "cask",
            help = "Treat the names as formulae"
        )]
        formula: bool,
        #[arg(long, visible_alias = "casks", help = "Treat the names as casks")]
        cask: bool,
        #[arg(long, help = "Also remove the casks' user data (zap stanza)")]
        zap: bool,
        #[arg(
            long,
            help = "Uninstall even if other installed packages depend on these"
        )]
        ignore_dependencies: bool,
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
        #[arg(
            long,
            visible_alias = "formulae",
            conflicts_with = "cask",
            help = "List only formulae, by name"
        )]
        formula: bool,
        #[arg(long, visible_alias = "casks", help = "List only casks, by token")]
        cask: bool,
        #[arg(long, help = "Show the version of each package, one per line")]
        versions: bool,
        #[arg(short = '1', help = "List names one per line")]
        one: bool,
    },
    /// Show information about an installed package, or about a cask
    Info {
        #[arg(help = "Name of the installed package, or of a cask")]
        formula: String,
        #[arg(
            long = "formula",
            visible_alias = "formulae",
            help = "Treat the name as a formula"
        )]
        formula_only: bool,
        #[arg(long, visible_alias = "casks", help = "Treat the name as a cask")]
        cask: bool,
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
        #[command(flatten)]
        cask_options: CaskOptions,
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
    /// Symlink installed formulae, or casks' binaries, man pages and
    /// completions, into the prefix
    Link {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
        #[arg(
            long,
            short = 'f',
            help = "Also link keg-only formulae; for casks, replace their own old links"
        )]
        force: bool,
        #[arg(
            long,
            help = "Delete files that already exist in the prefix while linking"
        )]
        overwrite: bool,
        #[arg(
            long,
            short = 'n',
            help = "List files which would be linked or deleted without linking or deleting any"
        )]
        dry_run: bool,
        #[arg(
            long,
            visible_alias = "formulae",
            conflicts_with = "cask",
            help = "Treat all names as formulae"
        )]
        formula: bool,
        #[arg(long, visible_alias = "casks", help = "Treat all names as casks")]
        cask: bool,
    },
    /// Remove installed formulae's or casks' symlinks from the prefix
    Unlink {
        #[arg(required = true, num_args = 1..)]
        formulas: Vec<String>,
        #[arg(
            long,
            visible_alias = "formulae",
            conflicts_with = "cask",
            help = "Treat all names as formulae"
        )]
        formula: bool,
        #[arg(long, visible_alias = "casks", help = "Treat all names as casks")]
        cask: bool,
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
    /// Tap a formula repository, or list the tapped ones
    Tap {
        /// `user/repo`
        name: Option<String>,
        /// Where to fetch it from instead of github.com/user/homebrew-repo
        url: Option<String>,
        #[arg(long, short = 'f', help = "Tap even a tap nextbrew reads from the API")]
        force: bool,
        #[arg(long, help = "Change an installed tap's remote to URL")]
        custom_remote: bool,
    },
    /// Remove tapped formula repositories
    Untap {
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
        #[arg(
            long,
            short = 'f',
            help = "Uninstall what was installed from the tap first"
        )]
        force: bool,
    },
    /// Trust third-party taps, formulae, casks or commands so they may be loaded
    Trust {
        names: Vec<String>,
        #[arg(long, visible_alias = "taps", conflicts_with_all = ["formula", "cask", "command"])]
        tap: bool,
        #[arg(long, visible_alias = "formulae", conflicts_with_all = ["cask", "command"])]
        formula: bool,
        #[arg(long, visible_alias = "casks", conflicts_with = "command")]
        cask: bool,
        #[arg(long, visible_alias = "commands")]
        command: bool,
    },
    /// Stop trusting third-party taps, formulae, casks or commands
    Untrust {
        names: Vec<String>,
        #[arg(long, visible_alias = "taps", conflicts_with_all = ["formula", "cask", "command"])]
        tap: bool,
        #[arg(long, visible_alias = "formulae", conflicts_with_all = ["cask", "command"])]
        formula: bool,
        #[arg(long, visible_alias = "casks", conflicts_with = "command")]
        cask: bool,
        #[arg(long, visible_alias = "commands")]
        command: bool,
    },
    /// Manage background services with launchd
    Services {
        #[command(subcommand)]
        command: Option<ServicesCommands>,
    },
}

/// `brew install --cask`'s options, over `$HOMEBREW_CASK_OPTS`.
#[derive(clap::Args, Clone, Default)]
pub struct CaskOptions {
    #[arg(
        long = "appdir",
        value_name = "DIR",
        help = "Install the Applications folder into DIR"
    )]
    pub appdir: Option<String>,
    #[arg(
        long = "appimagedir",
        value_name = "DIR",
        help = "Install AppImages into DIR"
    )]
    pub appimagedir: Option<String>,
    #[arg(
        long = "keyboard-layoutdir",
        value_name = "DIR",
        help = "Install keyboard layouts into DIR"
    )]
    pub keyboard_layoutdir: Option<String>,
    #[arg(
        long = "colorpickerdir",
        value_name = "DIR",
        help = "Install color pickers into DIR"
    )]
    pub colorpickerdir: Option<String>,
    #[arg(
        long = "prefpanedir",
        value_name = "DIR",
        help = "Install preference panes into DIR"
    )]
    pub prefpanedir: Option<String>,
    #[arg(
        long = "qlplugindir",
        value_name = "DIR",
        help = "Install QuickLook plugins into DIR"
    )]
    pub qlplugindir: Option<String>,
    #[arg(
        long = "mdimporterdir",
        value_name = "DIR",
        help = "Install Spotlight importers into DIR"
    )]
    pub mdimporterdir: Option<String>,
    #[arg(
        long = "dictionarydir",
        value_name = "DIR",
        help = "Install dictionaries into DIR"
    )]
    pub dictionarydir: Option<String>,
    #[arg(long = "fontdir", value_name = "DIR", help = "Install fonts into DIR")]
    pub fontdir: Option<String>,
    #[arg(
        long = "servicedir",
        value_name = "DIR",
        help = "Install services into DIR"
    )]
    pub servicedir: Option<String>,
    #[arg(
        long = "input-methoddir",
        value_name = "DIR",
        help = "Install input methods into DIR"
    )]
    pub input_methoddir: Option<String>,
    #[arg(
        long = "internet-plugindir",
        value_name = "DIR",
        help = "Install internet plugins into DIR"
    )]
    pub internet_plugindir: Option<String>,
    #[arg(
        long = "audio-unit-plugindir",
        value_name = "DIR",
        help = "Install audio unit plugins into DIR"
    )]
    pub audio_unit_plugindir: Option<String>,
    #[arg(
        long = "vst-plugindir",
        value_name = "DIR",
        help = "Install VST plugins into DIR"
    )]
    pub vst_plugindir: Option<String>,
    #[arg(
        long = "vst3-plugindir",
        value_name = "DIR",
        help = "Install VST3 plugins into DIR"
    )]
    pub vst3_plugindir: Option<String>,
    #[arg(
        long = "screen-saverdir",
        value_name = "DIR",
        help = "Install screen savers into DIR"
    )]
    pub screen_saverdir: Option<String>,
    #[arg(
        long,
        value_name = "LANGUAGE",
        help = "Not supported: casks install in their default language"
    )]
    pub language: Option<String>,
    #[arg(long, help = "Refuse a cask without a checksum")]
    pub require_sha: bool,
    #[arg(long, help = "Leave out the casks' binaries")]
    pub no_binaries: bool,
}

impl CaskOptions {
    /// The options as `--key=value` and switches, the way
    /// `$HOMEBREW_CASK_OPTS` gives them.
    pub fn options(&self) -> Vec<String> {
        let mut options: Vec<String> = [
            ("appdir", &self.appdir),
            ("appimagedir", &self.appimagedir),
            ("keyboard_layoutdir", &self.keyboard_layoutdir),
            ("colorpickerdir", &self.colorpickerdir),
            ("prefpanedir", &self.prefpanedir),
            ("qlplugindir", &self.qlplugindir),
            ("mdimporterdir", &self.mdimporterdir),
            ("dictionarydir", &self.dictionarydir),
            ("fontdir", &self.fontdir),
            ("servicedir", &self.servicedir),
            ("input_methoddir", &self.input_methoddir),
            ("internet_plugindir", &self.internet_plugindir),
            ("audio_unit_plugindir", &self.audio_unit_plugindir),
            ("vst_plugindir", &self.vst_plugindir),
            ("vst3_plugindir", &self.vst3_plugindir),
            ("screen_saverdir", &self.screen_saverdir),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.as_ref().map(|value| format!("--{key}={value}")))
        .collect();
        if let Some(language) = &self.language {
            options.push(format!("--language={language}"));
        }
        for (on, flag) in [
            (self.require_sha, "--require-sha"),
            (self.no_binaries, "--no-binaries"),
        ] {
            if on {
                options.push(flag.to_string());
            }
        }
        options
    }
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
    /// Check that everything in a Brewfile is installed and up to date
    Check {
        #[arg(
            long,
            value_name = "FILE",
            help = "Read this Brewfile (`-` for standard input)"
        )]
        file: Option<PathBuf>,
        #[arg(long, help = "Read ~/.Brewfile (or $HOMEBREW_BUNDLE_FILE_GLOBAL)")]
        global: bool,
        #[arg(long, help = "Do not count outdated dependencies")]
        no_upgrade: bool,
        #[arg(long, help = "Print nothing when the dependencies are satisfied")]
        quiet: bool,
    },
    /// List the dependencies in a Brewfile (formulae unless told otherwise)
    List {
        #[arg(
            long,
            value_name = "FILE",
            help = "Read this Brewfile (`-` for standard input)"
        )]
        file: Option<PathBuf>,
        #[arg(long, help = "Read ~/.Brewfile (or $HOMEBREW_BUNDLE_FILE_GLOBAL)")]
        global: bool,
        #[arg(long, help = "List every dependency")]
        all: bool,
        #[arg(long, visible_aliases = ["formulae", "brews"], help = "List formulae")]
        formula: bool,
        #[arg(long, visible_alias = "casks", help = "List casks")]
        cask: bool,
        #[arg(long, visible_alias = "taps", help = "List taps")]
        tap: bool,
        #[arg(long, help = "List mas entries")]
        mas: bool,
        #[arg(long, help = "List vscode entries")]
        vscode: bool,
        #[arg(long, help = "List go entries")]
        go: bool,
        #[arg(long, help = "List cargo entries")]
        cargo: bool,
        #[arg(long, help = "List uv entries")]
        uv: bool,
        #[arg(long, help = "List flatpak entries")]
        flatpak: bool,
        #[arg(long, help = "List npm entries")]
        npm: bool,
        #[arg(long, help = "List krew entries")]
        krew: bool,
        #[arg(long, help = "List winget entries")]
        winget: bool,
    },
    /// Add entries to a Brewfile (formulae unless told otherwise)
    Add {
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
        #[arg(
            long,
            value_name = "FILE",
            help = "Change this Brewfile instead of ./Brewfile or $HOMEBREW_BUNDLE_FILE"
        )]
        file: Option<PathBuf>,
        #[arg(long, help = "Change ~/.Brewfile (or $HOMEBREW_BUNDLE_FILE_GLOBAL)")]
        global: bool,
        #[arg(long, help = "Run `install` before adding entries")]
        install: bool,
        #[command(flatten)]
        types: EntryTypes,
        #[arg(long, help = "Do not add description comments above each line")]
        no_describe: bool,
    },
    /// Remove entries matching the names from a Brewfile
    Remove {
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
        #[arg(
            long,
            value_name = "FILE",
            help = "Change this Brewfile instead of ./Brewfile or $HOMEBREW_BUNDLE_FILE"
        )]
        file: Option<PathBuf>,
        #[arg(long, help = "Change ~/.Brewfile (or $HOMEBREW_BUNDLE_FILE_GLOBAL)")]
        global: bool,
        #[arg(long, help = "Run `install` before removing entries")]
        install: bool,
        #[command(flatten)]
        types: EntryTypes,
    },
    /// Run a command in the build environment for the Brewfile's formulae
    Exec {
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
        #[command(flatten)]
        options: BundleEnvArgs,
        #[arg(long, help = "Start the Brewfile's services while the command runs")]
        services: bool,
        #[arg(long, value_name = "PATH", help = "Not supported by nextbrew")]
        sandbox: Option<String>,
        #[arg(long, help = "Not supported by nextbrew")]
        deny_network: bool,
    },
    /// Start a shell in the build environment for the Brewfile's formulae
    Sh {
        #[command(flatten)]
        options: BundleEnvArgs,
        #[arg(long, help = "Start the Brewfile's services while the shell runs")]
        services: bool,
    },
    /// Print the build environment for the Brewfile's formulae as exports
    Env {
        #[command(flatten)]
        options: BundleEnvArgs,
    },
    /// Uninstall what a Brewfile does not list
    Cleanup {
        #[arg(
            long,
            value_name = "FILE",
            help = "Read this Brewfile (`-` for standard input)"
        )]
        file: Option<PathBuf>,
        #[arg(long, help = "Read ~/.Brewfile (or $HOMEBREW_BUNDLE_FILE_GLOBAL)")]
        global: bool,
        #[arg(long, short = 'f', help = "Actually uninstall")]
        force: bool,
        #[arg(long, help = "Clean up every type")]
        all: bool,
        #[arg(long, visible_aliases = ["formulae", "brews"], help = "Clean up formulae")]
        formula: bool,
        #[arg(long, visible_aliases = ["no-formulae", "no-brews"], help = "Clean up without formulae")]
        no_formula: bool,
        #[arg(long, visible_alias = "casks", help = "Clean up casks")]
        cask: bool,
        #[arg(long, visible_alias = "no-casks", help = "Clean up without casks")]
        no_cask: bool,
        #[arg(long, visible_alias = "taps", help = "Clean up taps")]
        tap: bool,
        #[arg(long, visible_alias = "no-taps", help = "Clean up without taps")]
        no_tap: bool,
        #[arg(long, help = "Remove casks with zap instead of uninstall")]
        zap: bool,
        #[command(flatten)]
        extensions: ExtensionFlags,
    },
    /// Write what is installed into a Brewfile
    Dump {
        #[arg(
            long,
            value_name = "FILE",
            help = "Write to this file (`-` for standard output) instead of ./Brewfile or $HOMEBREW_BUNDLE_FILE"
        )]
        file: Option<PathBuf>,
        #[arg(long, help = "Write ~/.Brewfile (or $HOMEBREW_BUNDLE_FILE_GLOBAL)")]
        global: bool,
        #[arg(long, short = 'f', help = "Overwrite an existing Brewfile")]
        force: bool,
        #[arg(long, visible_aliases = ["formulae", "brews"], help = "Dump formulae")]
        formula: bool,
        #[arg(long, visible_aliases = ["no-formulae", "no-brews"], help = "Dump without formulae")]
        no_formula: bool,
        #[arg(long, visible_alias = "casks", help = "Dump casks")]
        cask: bool,
        #[arg(long, visible_alias = "no-casks", help = "Dump without casks")]
        no_cask: bool,
        #[arg(long, visible_alias = "taps", help = "Dump taps")]
        tap: bool,
        #[arg(long, visible_alias = "no-taps", help = "Dump without taps")]
        no_tap: bool,
        #[arg(long, help = "Do not add description comments above each line")]
        no_describe: bool,
        #[arg(long, help = "Do not add restart_service to formula lines")]
        no_restart: bool,
        #[command(flatten)]
        extensions: ExtensionFlags,
    },
}

/// What `nb bundle exec`, `sh` and `env` share.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct BundleEnvArgs {
    #[arg(
        long,
        value_name = "FILE",
        help = "Read this Brewfile instead of ./Brewfile or $HOMEBREW_BUNDLE_FILE"
    )]
    pub file: Option<PathBuf>,
    #[arg(long, help = "Read ~/.Brewfile (or $HOMEBREW_BUNDLE_FILE_GLOBAL)")]
    pub global: bool,
    #[arg(long, help = "Run `install` first")]
    pub install: bool,
    #[arg(
        long,
        help = "Check that the Brewfile's dependencies are installed first"
    )]
    pub check: bool,
    #[arg(long, help = "Remove secrets from the environment first")]
    pub no_secrets: bool,
}

/// The entry type `nb bundle add` and `remove` work on: one of these, or
/// none.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct EntryTypes {
    #[arg(long, visible_aliases = ["formulae", "brews"], help = "Formula entries")]
    pub formula: bool,
    #[arg(long, visible_alias = "casks", help = "Cask entries")]
    pub cask: bool,
    #[arg(long, visible_alias = "taps", help = "Tap entries")]
    pub tap: bool,
    #[arg(long, help = "Mac App Store entries")]
    pub mas: bool,
    #[arg(long, help = "VS Code extension entries")]
    pub vscode: bool,
    #[arg(long, help = "Go package entries")]
    pub go: bool,
    #[arg(long, help = "Cargo package entries")]
    pub cargo: bool,
    #[arg(long, help = "uv tool entries")]
    pub uv: bool,
    #[arg(long, help = "Flatpak entries")]
    pub flatpak: bool,
    #[arg(long, help = "Winget entries")]
    pub winget: bool,
    #[arg(long, help = "Krew plugin entries")]
    pub krew: bool,
    #[arg(long, help = "npm package entries")]
    pub npm: bool,
}

impl EntryTypes {
    /// `selected_types`: the directives asked for, in Homebrew's order.
    pub fn selected(&self) -> Vec<&'static str> {
        [
            ("brew", self.formula),
            ("cask", self.cask),
            ("tap", self.tap),
            ("mas", self.mas),
            ("vscode", self.vscode),
            ("go", self.go),
            ("cargo", self.cargo),
            ("uv", self.uv),
            ("flatpak", self.flatpak),
            ("winget", self.winget),
            ("krew", self.krew),
            ("npm", self.npm),
        ]
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| name)
        .collect()
    }
}

/// `--mas`, `--no-mas` and the like for the package managers `brew bundle`
/// cleans up and dumps besides Homebrew.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct ExtensionFlags {
    #[arg(long, help = "Include Mac App Store apps")]
    pub mas: bool,
    #[arg(long, help = "Leave out Mac App Store apps")]
    pub no_mas: bool,
    #[arg(long, help = "Include VS Code extensions")]
    pub vscode: bool,
    #[arg(long, help = "Leave out VS Code extensions")]
    pub no_vscode: bool,
    #[arg(long, help = "Include Go packages")]
    pub go: bool,
    #[arg(long, help = "Leave out Go packages")]
    pub no_go: bool,
    #[arg(long, help = "Include Cargo packages")]
    pub cargo: bool,
    #[arg(long, help = "Leave out Cargo packages")]
    pub no_cargo: bool,
    #[arg(long, help = "Include uv tools")]
    pub uv: bool,
    #[arg(long, help = "Leave out uv tools")]
    pub no_uv: bool,
    #[arg(long, help = "Include Krew plugins")]
    pub krew: bool,
    #[arg(long, help = "Leave out Krew plugins")]
    pub no_krew: bool,
    #[arg(long, help = "Include npm packages")]
    pub npm: bool,
    #[arg(long, help = "Leave out npm packages")]
    pub no_npm: bool,
}

impl ExtensionFlags {
    /// `(asked for, left out)` for the extension with this directive.
    pub fn get(&self, name: &str) -> (bool, bool) {
        match name {
            "mas" => (self.mas, self.no_mas),
            "vscode" => (self.vscode, self.no_vscode),
            "go" => (self.go, self.no_go),
            "cargo" => (self.cargo, self.no_cargo),
            "uv" => (self.uv, self.no_uv),
            "krew" => (self.krew, self.no_krew),
            "npm" => (self.npm, self.no_npm),
            _ => (false, false),
        }
    }

    /// Whether any extension was asked for by name.
    pub fn any(&self) -> bool {
        self.mas || self.vscode || self.go || self.cargo || self.uv || self.krew || self.npm
    }
}
