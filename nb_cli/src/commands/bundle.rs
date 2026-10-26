//! `nb bundle`: `brew bundle` over a Brewfile read without Ruby (see
//! [`crate::brewfile`]). Entries nextbrew cannot install (`args:` other
//! than a build from source or a formula option) count as failures and say why, as a failed install does in Homebrew;
//! none are skipped silently.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use console::style;
use nb_io::tap::TapName;
use nb_io::tap::trust::{TrustKind, TrustStore};

use super::bundle_exec;
use super::bundle_ext::{self, Ext, Installed};
use super::{install, services, upgrade};
use crate::brewfile::{self, Entry, Kind, Value};
use crate::cli::{BundleCommands, ServicesCommands};
use crate::ui::StdUi;

/// The taps whose packages nextbrew reads from the API, there without
/// tapping.
const BUILT_IN_TAPS: &[&str] = &["homebrew/core", "homebrew/cask"];

/// Official taps Homebrew has emptied; tapping them fails with this.
const DEPRECATED_TAPS: &[&str] = &["homebrew/bundle", "homebrew/services"];

pub async fn execute(
    installer: &mut nb_io::Installer,
    command: BundleCommands,
    verbose: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    match command {
        BundleCommands::Install(args) => {
            let path = brewfile_path(args.file.as_deref(), args.global)?;
            let options = InstallOptions {
                no_upgrade: !args.upgrade
                    && (args.no_upgrade
                        || std::env::var_os("HOMEBREW_BUNDLE_NO_UPGRADE").is_some()),
                upgrade_formulae: args
                    .upgrade_formulae
                    .unwrap_or_default()
                    .split(',')
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
                    .collect(),
                verbose,
                quiet: args.quiet,
            };
            install_from_file(installer, &path, &options, ui).await
        }
        BundleCommands::Add {
            names,
            file,
            global,
            install,
            types,
            no_describe,
        } => {
            let directive = match types.selected().as_slice() {
                [] => "brew",
                [one @ ("mas" | "winget")] => {
                    return Err(usage(&format!(
                        "The `add` subcommand does not accept the `--{one}` switch."
                    )));
                }
                [one] => one,
                _ => return Err(usage("`add` supports only one type of entry at a time.")),
            };
            let path = brewfile_path(file.as_deref(), global)?;
            if install {
                install_first(installer, &path, verbose, ui).await?;
            }
            let describe =
                !no_describe && std::env::var_os("HOMEBREW_BUNDLE_NO_DESCRIBE").is_none();
            add(installer, &path, &names, directive, describe).await
        }
        BundleCommands::Remove {
            names,
            file,
            global,
            install,
            types,
        } => {
            let directive = match types.selected().as_slice() {
                [] => None,
                [one] => Some(*one),
                _ => {
                    return Err(usage("`remove` supports only one type of entry at a time."));
                }
            };
            let path = brewfile_path(file.as_deref(), global)?;
            if install {
                install_first(installer, &path, verbose, ui).await?;
            }
            remove(installer, &path, &names, directive).await
        }
        BundleCommands::Exec {
            command,
            options,
            services,
            sandbox,
            deny_network,
        } => {
            if sandbox.is_some() || deny_network {
                return Err(usage(
                    "`--sandbox` and `--deny-network` are not supported by nextbrew.",
                ));
            }
            in_build_environment(installer, &options, Some(command), services, verbose, ui).await
        }
        BundleCommands::Sh { options, services } => {
            in_build_environment(installer, &options, None, services, verbose, ui).await
        }
        BundleCommands::Env { options } => {
            let path = brewfile_path(options.file.as_deref(), options.global)?;
            let (old, new) =
                build_environment(installer, &path, &options, None, verbose, ui).await?;
            print!("{}", bundle_exec::exports(&old, &new));
            Ok(())
        }
        BundleCommands::Check {
            file,
            global,
            no_upgrade,
            quiet,
        } => {
            let path = brewfile_path(file.as_deref(), global)?;
            let no_upgrade = no_upgrade || std::env::var_os("HOMEBREW_BUNDLE_NO_UPGRADE").is_some();
            check(installer, &path, no_upgrade, verbose, quiet).await
        }
        BundleCommands::List {
            file,
            global,
            all,
            formula,
            cask,
            tap,
            mas,
            vscode,
            go,
            cargo,
            uv,
            flatpak,
            npm,
            krew,
            winget,
        } => {
            let path = brewfile_path(file.as_deref(), global)?;
            let extensions = [
                ("mas", mas),
                ("vscode", vscode),
                ("go", go),
                ("cargo", cargo),
                ("uv", uv),
                ("flatpak", flatpak),
                ("npm", npm),
                ("krew", krew),
                ("winget", winget),
            ];
            let types: Vec<&str> = extensions
                .iter()
                .filter(|(_, on)| *on)
                .map(|(kind, _)| *kind)
                .collect();
            let no_type_args = !(formula || cask || tap) && types.is_empty();
            for entry in read_brewfile(&path)? {
                let show = match entry.kind {
                    Kind::Brew => formula || all || no_type_args,
                    Kind::Cask => cask || all,
                    Kind::Tap => tap || all,
                    Kind::Extension(kind) => all || types.contains(&kind),
                };
                if show {
                    println!("{}", entry.name);
                }
            }
            Ok(())
        }
        BundleCommands::Cleanup {
            file,
            global,
            force,
            all,
            formula,
            no_formula,
            cask,
            no_cask,
            tap,
            no_tap,
            zap,
            extensions,
        } => {
            let no_type_args = !(formula || cask || tap || extensions.any());
            let selected = |asked: bool, off: bool, env: &str| {
                !off && std::env::var_os(env).is_none() && (asked || all || no_type_args)
            };
            let formulae = selected(formula, no_formula, "HOMEBREW_BUNDLE_CLEANUP_NO_BREW");
            let casks = selected(cask, no_cask, "HOMEBREW_BUNDLE_CLEANUP_NO_CASK");
            let taps = selected(tap, no_tap, "HOMEBREW_BUNDLE_CLEANUP_NO_TAP");
            let extensions = selected_extensions(&extensions, "CLEANUP", selected);
            let path = brewfile_path(file.as_deref(), global)?;
            let types = CleanupTypes {
                formulae,
                casks,
                taps,
                extensions,
            };
            cleanup(installer, &path, force, zap, &types, ui).await
        }
        BundleCommands::Dump {
            file,
            global,
            force,
            formula,
            no_formula,
            cask,
            no_cask,
            tap,
            no_tap,
            no_describe,
            no_restart,
            extensions,
        } => {
            // `type_selected?`: the types asked for, or all of them, minus
            // those turned off by flag or environment.
            let no_type_args = !(formula || cask || tap || extensions.any());
            let selected = |asked: bool, off: bool, env: &str| {
                !off && std::env::var_os(env).is_none() && (asked || no_type_args)
            };
            let options = DumpOptions {
                taps: selected(tap, no_tap, "HOMEBREW_BUNDLE_DUMP_NO_TAP"),
                formulae: selected(formula, no_formula, "HOMEBREW_BUNDLE_DUMP_NO_BREW"),
                casks: selected(cask, no_cask, "HOMEBREW_BUNDLE_DUMP_NO_CASK"),
                extensions: selected_extensions(&extensions, "DUMP", selected),
                describe: !no_describe && std::env::var_os("HOMEBREW_BUNDLE_NO_DESCRIBE").is_none(),
                no_restart,
            };
            let path = match file.as_deref() {
                Some(file) if file == Path::new("-") => PathBuf::from("/dev/stdout"),
                file => brewfile_path(file, global)?,
            };
            dump_to_file(installer, &path, force, &options).await
        }
    }
}

/// `Brewfile.path`: `--file` (`-` is standard input), `--global`
/// (`~/.Brewfile`), `$HOMEBREW_BUNDLE_FILE`, or `./Brewfile`.
fn brewfile_path(file: Option<&Path>, global: bool) -> Result<PathBuf, nb_core::Error> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let path = if global {
        match std::env::var_os("HOMEBREW_BUNDLE_FILE_GLOBAL").filter(|v| !v.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => home.join(".Brewfile"),
        }
    } else if let Some(file) = file {
        if file == Path::new("-") {
            PathBuf::from("/dev/stdin")
        } else {
            file.to_path_buf()
        }
    } else if let Some(path) = std::env::var_os("HOMEBREW_BUNDLE_FILE").filter(|v| !v.is_empty()) {
        PathBuf::from(path)
    } else {
        PathBuf::from("Brewfile")
    };
    Ok(path)
}

fn read_brewfile(path: &Path) -> Result<Vec<Entry>, nb_core::Error> {
    let source = std::fs::read_to_string(path).map_err(|_| nb_core::Error::FileError {
        message: "No Brewfile found".to_string(),
    })?;
    brewfile::parse(&source).map_err(|e| nb_core::Error::ExecutionError {
        message: e.to_string(),
    })
}

/// `$HOMEBREW_BUNDLE_<NAME>` set, as for a switch that reads it.
fn env_switch(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// `run_external_command` up to running: `--install`, `--check`, then the
/// user's environment and the build environment made from it.
async fn build_environment(
    installer: &mut nb_io::Installer,
    path: &Path,
    options: &crate::cli::BundleEnvArgs,
    command: Option<&str>,
    verbose: bool,
    ui: &mut StdUi,
) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>), nb_core::Error> {
    if options.install {
        install_first(installer, path, verbose, ui).await?;
    }
    if options.check || env_switch("HOMEBREW_BUNDLE_CHECK") {
        check(installer, path, false, false, true).await?;
    }
    let mut user: BTreeMap<String, String> = std::env::vars().collect();
    let entries = read_brewfile(path)?;
    let deps = bundle_exec::deps(installer, &entries, &mut user).await?;
    let no_secrets = options.no_secrets || env_switch("HOMEBREW_BUNDLE_NO_SECRETS");
    let env = bundle_exec::compose(user.clone(), &deps, installer.prefix(), command, no_secrets)?;
    Ok((user, env))
}

/// `nb bundle exec` and `sh`: the command (or the user's shell) in the
/// build environment, with the Brewfile's services running if asked.
async fn in_build_environment(
    installer: &mut nb_io::Installer,
    options: &crate::cli::BundleEnvArgs,
    command: Option<Vec<String>>,
    services: bool,
    verbose: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let path = brewfile_path(options.file.as_deref(), options.global)?;
    let program = command.as_ref().and_then(|c| c.first()).cloned();
    let (_, env) =
        build_environment(installer, &path, options, program.as_deref(), verbose, ui).await?;
    let mut cmd = match &command {
        Some(command) => {
            let mut cmd = std::process::Command::new(&command[0]);
            cmd.args(&command[1..]).env_clear().envs(&env);
            cmd
        }
        None => bundle_exec::shell(&env)?,
    };
    if !(services || env_switch("HOMEBREW_BUNDLE_SERVICES")) {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec();
        return Err(nb_core::Error::ExecutionError {
            message: format!("failed to run {}: {e}", program.unwrap_or_default()),
        });
    }
    let entries = read_brewfile(&path)?;
    let (started, to_restart) = start_bundle_services(installer, &entries).await;
    let status = cmd.status();
    for name in started {
        let stop = ServicesCommands::Stop {
            formulas: vec![name.clone()],
            all: false,
            keep: true,
            no_wait: false,
            max_wait: None,
        };
        if services::execute(installer, Some(stop), false)
            .await
            .is_err()
        {
            warn(&format!("Failed to stop {name} service"));
        }
    }
    for name in to_restart {
        let run = ServicesCommands::Run {
            formulas: vec![name.clone()],
            all: false,
            file: None,
        };
        if services::execute(installer, Some(run), false)
            .await
            .is_err()
        {
            warn(&format!("Failed to restart {name} service"));
        }
    }
    match status {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => Err(nb_core::Error::ExecutionError {
            message: format!("failed to run {}: {e}", program.unwrap_or_default()),
        }),
    }
}

fn warn(message: &str) {
    eprintln!("{} {message}", style("Warning:").yellow().bold());
}

/// `run_services`: run each installed Brewfile formula's service that is
/// not running, stopping running services it conflicts with (those named
/// in `conflicts_with:` and other versions of the formula) first. Returns
/// the services started, and the stopped ones to run again afterwards
/// (those registered to start at login).
async fn start_bundle_services(
    installer: &nb_io::Installer,
    entries: &[Entry],
) -> (Vec<String>, Vec<String>) {
    let launchd = nb_io::services::Launchd::current(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
    );
    let running = |name: &str| launchd.find(&nb_io::services::label(name)).running();
    let registered = |name: &str| {
        launchd
            .dest_dir()
            .join(format!("{}.plist", nb_io::services::label(name)))
            .exists()
    };
    let installed: Vec<String> = installer
        .list_installed()
        .unwrap_or_default()
        .into_iter()
        .map(|keg| keg.name)
        .collect();
    let base = |name: &str| name.split('@').next().unwrap_or(name).to_string();
    let (mut started, mut to_restart) = (Vec::new(), Vec::new());
    for entry in entries.iter().filter(|e| e.kind == Kind::Brew) {
        let Ok(service) = installer.service_formula(&entry.name).await else {
            continue;
        };
        if !service.has_service || service.keg.is_none() || running(&service.name) {
            continue;
        }
        let mut conflicts: Vec<String> = installed
            .iter()
            .filter(|other| **other != service.name && base(other) == base(&service.name))
            .cloned()
            .collect();
        if let Some(Value::Array(names)) = entry.option("conflicts_with") {
            conflicts.extend(names.iter().filter_map(|v| v.as_str().map(str::to_string)));
        }
        for conflict in conflicts.into_iter().filter(|c| running(c)) {
            let stop = ServicesCommands::Stop {
                formulas: vec![conflict.clone()],
                all: false,
                keep: true,
                no_wait: false,
                max_wait: None,
            };
            if services::execute(installer, Some(stop), false)
                .await
                .is_ok()
            {
                if registered(&conflict) {
                    to_restart.push(conflict);
                }
            } else {
                warn(&format!("Failed to stop {conflict} service"));
            }
        }
        let run = ServicesCommands::Run {
            formulas: vec![service.name.clone()],
            all: false,
            file: None,
        };
        if services::execute(installer, Some(run), false)
            .await
            .is_err()
        {
            warn(&format!("Failed to start {} service", service.name));
        }
        started.push(service.name);
    }
    (started, to_restart)
}

fn usage(message: &str) -> nb_core::Error {
    nb_core::Error::InvalidArgument {
        message: message.to_string(),
    }
}

/// `--install`: `brew bundle install` first, quietly.
async fn install_first(
    installer: &mut nb_io::Installer,
    path: &Path,
    verbose: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let options = InstallOptions {
        no_upgrade: std::env::var_os("HOMEBREW_BUNDLE_NO_UPGRADE").is_some(),
        upgrade_formulae: Vec::new(),
        verbose,
        quiet: true,
    };
    install_from_file(installer, path, &options, ui).await
}

/// `Adder.add`: `<type> "<name>"` lines at the end of the Brewfile (made
/// if missing), formulae and casks each under their description.
async fn add(
    installer: &nb_io::Installer,
    path: &Path,
    names: &[String],
    directive: &str,
    describe: bool,
) -> Result<(), nb_core::Error> {
    if !path.exists() {
        write_brewfile(path, "")?;
    }
    let mut content = std::fs::read_to_string(path).map_err(|_| nb_core::Error::FileError {
        message: "No Brewfile found".to_string(),
    })?;
    brewfile::parse(&content).map_err(|e| nb_core::Error::ExecutionError {
        message: e.to_string(),
    })?;
    // `brew bundle add user/repo/name` taps and trusts it first.
    if matches!(directive, "brew" | "cask") {
        let full: Vec<String> = names
            .iter()
            .map(|name| match directive {
                "cask" => format!("cask:{name}"),
                _ => name.clone(),
            })
            .collect();
        super::tap::prepare_full_names(installer, &full).await?;
    }
    let mut lines = Vec::new();
    for name in names {
        let desc = match directive {
            "brew" => installer.formula(name).await?.desc,
            "cask" => installer.cask(name).await?["desc"]
                .as_str()
                .map(str::to_string),
            _ => None,
        };
        let entry = format!("{directive} \"{name}\"");
        lines.push(match desc.filter(|_| describe) {
            Some(desc) => describe_lines(Some(&desc)) + &entry,
            None => entry,
        });
    }
    content.push_str(&lines.join("\n"));
    content.push('\n');
    write_brewfile(path, &content)
}

/// `Remover.remove`: drop the lines naming any of `names` (as `<type>
/// "name"`, any type when none is given), and the description comment
/// above each. For formulae, their aliases and old names match too.
async fn remove(
    installer: &nb_io::Installer,
    path: &Path,
    names: &[String],
    directive: Option<&str>,
) -> Result<(), nb_core::Error> {
    let content = std::fs::read_to_string(path).map_err(|_| nb_core::Error::FileError {
        message: "No Brewfile found".to_string(),
    })?;
    brewfile::parse(&content).map_err(|e| nb_core::Error::ExecutionError {
        message: e.to_string(),
    })?;
    let mut patterns: Vec<String> = Vec::new();
    for name in names {
        let candidates = match directive {
            Some("brew") => possible_names(installer, name, true).await?,
            _ => vec![name.clone()],
        };
        for candidate in candidates {
            let pattern = regex::escape(&candidate);
            if !patterns.contains(&pattern) {
                patterns.push(pattern);
            }
        }
    }
    let entry = regex::Regex::new(&format!(
        r#"{}(?:\s+|\(\s*)"({})""#,
        directive.unwrap_or_default(),
        patterns.join("|")
    ))
    .map_err(|e| usage(&e.to_string()))?;
    let comment = regex::Regex::new(r"(?m)^\s*#\s+(?<desc>.+)$").unwrap();

    // Ruby's `split("\n")`: no empty lines at the end.
    let mut lines: Vec<&str> = content.split('\n').collect();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    let mut kept: Vec<&str> = Vec::new();
    for line in lines {
        let Some(found) = entry.captures(line) else {
            kept.push(line);
            continue;
        };
        // `remove_package_description_comment`.
        let above = kept
            .last()
            .and_then(|last| comment.captures(last))
            .map(|c| c["desc"].to_string());
        if let Some(above) = above
            && description(installer, &found[1]).await.as_deref() == Some(above.as_str())
        {
            kept.pop();
        }
    }
    let new_content = format!("{}\n", kept.join("\n"));
    let chomp = |text: &str| text.strip_suffix('\n').unwrap_or(text).to_string();
    if directive.is_none() && chomp(&content) == chomp(&new_content) {
        for name in names {
            let names = possible_names(installer, name, false)
                .await
                .unwrap_or_default();
            if names.len() > 1 {
                eprintln!(
                    "{} No matching entries found in Brewfile. Try again with `--formula` to match formula aliases and old formula names.",
                    style("Warning:").yellow().bold()
                );
                return Ok(());
            }
        }
    }
    write_brewfile(path, &new_content)
}

/// `Remover.possible_names`: what a formula may be written as: the name
/// given, its name, aliases and old names. Nothing for a cask, or for an
/// unknown name unless `strict`.
async fn possible_names(
    installer: &nb_io::Installer,
    name: &str,
    strict: bool,
) -> Result<Vec<String>, nb_core::Error> {
    let formula = match installer.formula(name).await {
        Ok(formula) => formula,
        Err(e) if strict => return Err(e),
        Err(_) => return Ok(Vec::new()),
    };
    let mut names = vec![name.to_string(), formula.name.clone()];
    names.extend(formula.aliases);
    names.extend(formula.oldnames);
    let mut unique: Vec<String> = Vec::new();
    for name in names {
        if !unique.contains(&name) {
            unique.push(name);
        }
    }
    Ok(unique)
}

/// The description of a formula, or else of a cask, by this name.
async fn description(installer: &nb_io::Installer, name: &str) -> Option<String> {
    match installer.formula(name).await {
        Ok(formula) => formula.desc,
        Err(_) => installer.cask(name).await.ok()?["desc"]
            .as_str()
            .map(str::to_string),
    }
}

fn write_brewfile(path: &Path, content: &str) -> Result<(), nb_core::Error> {
    std::fs::write(path, content).map_err(|e| nb_core::Error::FileError {
        message: format!("failed to write {}: {e}", path.display()),
    })
}

struct InstallOptions {
    no_upgrade: bool,
    upgrade_formulae: Vec<String>,
    verbose: bool,
    quiet: bool,
}

/// `Skipper.skip?`: `$HOMEBREW_BUNDLE_<TYPE>_SKIP` lists names to leave.
fn skipped(entry: &Entry) -> bool {
    let variable = match entry.kind {
        Kind::Tap => "HOMEBREW_BUNDLE_TAP_SKIP",
        Kind::Brew => "HOMEBREW_BUNDLE_BREW_SKIP",
        Kind::Cask => "HOMEBREW_BUNDLE_CASK_SKIP",
        Kind::Extension("mas") => "HOMEBREW_BUNDLE_MAS_SKIP",
        Kind::Extension(_) => return false,
    };
    std::env::var(variable)
        .unwrap_or_default()
        .split_whitespace()
        .any(|name| name == entry.name || Some(name) == full_name(entry).as_deref())
}

fn full_name(entry: &Entry) -> Option<String> {
    entry
        .option("full_name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The third-party tap a `brew` or `cask` entry names in full, and the
/// name in it.
fn entry_tap(entry: &Entry) -> Option<(TapName, String)> {
    let name = match entry.kind {
        Kind::Brew => entry.name.clone(),
        Kind::Cask => full_name(entry)?,
        _ => return None,
    };
    TapName::split_full_name(&name).filter(|(tap, _)| !tap.is_official())
}

/// What `nb install` is given for a `brew` or `cask` entry: a tap's in
/// full.
fn install_name(entry: &Entry) -> String {
    match (entry.kind, entry_tap(entry)) {
        (Kind::Cask, Some((tap, item))) => format!("cask:{tap}/{item}"),
        (Kind::Cask, None) => format!("cask:{}", entry.name),
        _ => entry.name.clone(),
    }
}

/// What a `brew` or `cask` entry is installed as: a formula by its name,
/// whichever tap it came from (Homebrew's rack), a cask by its token.
fn installed_name(entry: &Entry) -> String {
    match entry.kind {
        Kind::Cask => format!("cask:{}", entry.name),
        _ => entry
            .name
            .rsplit('/')
            .next()
            .unwrap_or(&entry.name)
            .to_string(),
    }
}

/// The keys a tap's `trusted:` hash may have.
const TRUSTED_ITEM_KEYS: &[&str] = &[
    "formula", "formulae", "cask", "casks", "command", "commands",
];

/// `Bundle::Trust.entries`: what the Brewfile's `trusted:` options trust,
/// named as `brew trust` names them: a tap (by its `clone_target` when
/// that is a custom remote), the formulae, casks and commands its hash
/// lists, and
/// formulae and casks named in full.
fn trust_entries(entries: &[Entry]) -> Result<Vec<(TrustKind, String)>, nb_core::Error> {
    let clone_target = |entry: &Entry| {
        entry
            .option("clone_target")
            .and_then(Value::as_str)
            .filter(|remote| !remote.is_empty())
            .map(str::to_string)
    };
    let remotes: Vec<(String, String)> = entries
        .iter()
        .filter(|entry| entry.kind == Kind::Tap)
        .filter_map(|entry| Some((entry.name.clone(), clone_target(entry)?)))
        .collect();
    let mut trusted = Vec::new();
    for entry in entries {
        let Some(option) = entry.option("trusted") else {
            continue;
        };
        match (entry.kind, option) {
            (Kind::Tap, Value::Bool(true)) => {
                let tap = TapName::parse(&entry.name)?;
                let remote = clone_target(entry);
                trusted.push((
                    TrustKind::Tap,
                    tap.reference(remote.as_deref()).to_lowercase(),
                ));
            }
            (Kind::Tap, Value::Hash(items)) => {
                let unsupported: Vec<&str> = items
                    .iter()
                    .map(|(key, _)| key.as_str())
                    .filter(|key| !TRUSTED_ITEM_KEYS.contains(key))
                    .collect();
                if !unsupported.is_empty() {
                    return Err(usage(&format!(
                        "Unsupported trusted keys: {}",
                        unsupported.join(", ")
                    )));
                }
                let tap = TapName::parse(&entry.name)?;
                let remote = clone_target(entry);
                for (key, value) in items {
                    let kind = match key.as_str() {
                        "formula" | "formulae" => TrustKind::Formula,
                        "cask" | "casks" => TrustKind::Cask,
                        _ => TrustKind::Command,
                    };
                    let values = match value {
                        Value::Array(values) => values.clone(),
                        Value::Nil => Vec::new(),
                        value => vec![value.clone()],
                    };
                    for value in values {
                        let item = match value {
                            Value::Str(item) | Value::Sym(item) => item,
                            Value::Int(item) => item.to_string(),
                            _ => continue,
                        };
                        let item = item.rsplit('/').next().unwrap_or_default();
                        if !item.is_empty() {
                            trusted.push((
                                kind,
                                TrustStore::item_trust_name(&tap, item, remote.as_deref()),
                            ));
                        }
                    }
                }
            }
            (Kind::Brew | Kind::Cask, Value::Bool(true)) => {
                let Some((tap, item)) = entry_tap(entry) else {
                    continue;
                };
                let remote = remotes
                    .iter()
                    .find(|(name, _)| *name == tap.name())
                    .map(|(_, remote)| remote.as_str());
                let kind = if entry.kind == Kind::Brew {
                    TrustKind::Formula
                } else {
                    TrustKind::Cask
                };
                trusted.push((kind, TrustStore::item_trust_name(&tap, &item, remote)));
            }
            _ => {}
        }
    }
    let mut unique = Vec::new();
    for entry in trusted {
        if !unique.contains(&entry) {
            unique.push(entry);
        }
    }
    Ok(unique)
}

fn say_verb(verb: &str, name: &str) {
    println!("{}", style(format!("{verb} {name}")).green());
}

fn say_failed(verb: &str, name: &str, reason: Option<&str>) {
    eprintln!("{}", style(format!("{verb} {name} has failed!")).red());
    match reason {
        // Homebrew's own error, as it prints it.
        Some(reason) if reason.starts_with("Error:") => eprintln!("{reason}"),
        Some(reason) => eprintln!("  {reason}"),
        None => {}
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        "dependency"
    } else {
        "dependencies"
    }
}

/// What nextbrew cannot do for an entry, or `None` if it can.
fn unsupported(entry: &Entry) -> Option<String> {
    match entry.kind {
        Kind::Tap if DEPRECATED_TAPS.contains(&entry.name.as_str()) => Some(format!(
            "Error: {} was deprecated. This tap is now empty and all its contents were either deleted or migrated.",
            entry.name
        )),
        Kind::Tap => None,
        Kind::Brew => brew_args(entry).err(),
        Kind::Cask => cask_config(entry).err().map(|e| e.to_string()),
        Kind::Extension(_) => None,
    }
}

/// A brew entry's `args:`, each passed to `brew install` as `--<arg>`:
/// whether they ask for a build from source, and the `--with(out)-…`
/// options, which a formula without options ignores; or why not.
fn brew_args(entry: &Entry) -> Result<(bool, Vec<String>), String> {
    let mut build = false;
    let mut ignored = Vec::new();
    let Some(Value::Array(args)) = entry.option("args") else {
        return Ok((build, ignored));
    };
    for arg in args {
        let arg = arg.as_str().map_or_else(|| arg.to_string(), str::to_string);
        match arg.as_str() {
            "build-from-source" => build = true,
            a if a.starts_with("with-") || a.starts_with("without-") => ignored.push(arg),
            _ => return Err(format!("nextbrew does not install with `--{arg}`")),
        }
    }
    Ok((build, ignored))
}

/// The options a cask entry installs with: `$HOMEBREW_CASK_OPTS` with its
/// `args:` (and the Brewfile's `cask_args`) on top, each turned into an
/// option as `Bundle::Cask` does: `true` a switch, `false` nothing, anything
/// else `--key=value`.
fn cask_config(entry: &Entry) -> Result<nb_io::CaskConfig, nb_core::Error> {
    let mut explicit = nb_io::CaskConfig::default();
    if let Some(Value::Hash(args)) = entry.option("args") {
        for (key, value) in args {
            match value {
                Value::Bool(true) => explicit.apply_option(&format!("--{key}"), true)?,
                Value::Bool(false) | Value::Nil => {}
                Value::Str(v) | Value::Sym(v) => {
                    explicit.apply_option(&format!("--{key}={v}"), true)?
                }
                other => explicit.apply_option(&format!("--{key}={other}"), true)?,
            }
        }
    }
    Ok(nb_io::CaskConfig::from_env()?.overlay(&explicit))
}

/// Where an entry stands before anything is installed.
struct Plan {
    entry: Entry,
    install_name: String,
    installed_name: String,
    installed: bool,
    upgradable: bool,
}

impl Plan {
    /// `install_verb`.
    fn verb(&self) -> &'static str {
        if self.installed && self.upgradable {
            "Upgrading"
        } else {
            "Installing"
        }
    }

    /// `preinstall!`: false when it is installed and stays as it is.
    fn actionable(&self) -> bool {
        !self.installed || self.upgradable
    }

    /// `homebrew_batchable?`: no options to act on afterwards.
    fn batchable(&self) -> bool {
        let extra = |allowed: &[&str]| {
            self.entry
                .options
                .iter()
                .any(|(key, _)| !allowed.contains(&key.as_str()))
        };
        match self.entry.kind {
            Kind::Brew => !extra(&["full_name", "trusted"]),
            Kind::Cask => {
                !extra(&["full_name", "trusted", "args"])
                    && !matches!(self.entry.option("args"), Some(Value::Hash(args)) if !args.is_empty())
            }
            _ => false,
        }
    }
}

/// The rest of `homebrew_batchable?`: whether the formula or cask loads
/// before anything is installed. One named in full needs its tap there,
/// trusted, and without the other kind of package of that name; one named
/// shortly must be Homebrew's own.
async fn loadable(installer: &nb_io::Installer, entry: &Entry) -> bool {
    let Some((tap, item)) = entry_tap(entry) else {
        return match entry.kind {
            Kind::Brew => installer
                .formula(&entry.name)
                .await
                .is_ok_and(|formula| formula.third_party_tap().is_none()),
            _ => installer.cask(&entry.name).await.is_ok(),
        };
    };
    let Ok(source) = installer.tap_source() else {
        return false;
    };
    if !source.taps.is_installed(&tap) {
        return false;
    }
    let remote = source.taps.remote(&tap);
    let formula = source.taps.find_formula(&tap, &item).map(|(name, _)| name);
    let cask = source.taps.find_cask(&tap, &item).map(|(token, _)| token);
    match (entry.kind, formula, cask) {
        (Kind::Brew, Some(name), None) => {
            source
                .trust
                .trusted(TrustKind::Formula, &tap, &name, remote.as_deref())
        }
        (Kind::Cask, None, Some(token)) => {
            source
                .trust
                .trusted(TrustKind::Cask, &tap, &token, remote.as_deref())
        }
        _ => false,
    }
}

/// Whether the tap of a `tap` entry is there.
fn tapped(installer: &nb_io::Installer, name: &str) -> bool {
    BUILT_IN_TAPS.contains(&name)
        || TapName::parse(name).is_ok_and(|tap| {
            installer
                .tap_source()
                .is_ok_and(|source| source.taps.is_installed(&tap))
        })
}

/// `brew tap` for a `tap` entry (`clone_target` its remote), saying why it
/// failed as brew's own error would.
async fn tap_entry(installer: &nb_io::Installer, entry: &Entry) -> bool {
    let tapped = async {
        let tap = TapName::parse(&entry.name)?;
        nb_io::tap::Taps::check_tappable(&tap, false)?;
        let remote = entry.option("clone_target").and_then(Value::as_str);
        super::tap::ensure_tapped(installer, &tap, remote).await
    };
    match tapped.await {
        Ok(()) => true,
        Err(e) => {
            eprintln!("{} {e}", style("Error:").red().bold());
            false
        }
    }
}

/// Before `nb install` of a tap's formulae or casks named in full: tap and
/// trust them, as `brew install` does.
async fn prepare_full_names(installer: &nb_io::Installer, names: &[String]) -> bool {
    match super::tap::prepare_full_names(installer, names).await {
        Ok(()) => true,
        Err(e) => {
            eprintln!("{} {e}", style("Error:").red().bold());
            false
        }
    }
}

async fn plan_entry(
    installer: &nb_io::Installer,
    entry: Entry,
    options: &InstallOptions,
) -> Result<Plan, nb_core::Error> {
    let install_name = install_name(&entry);
    let installed_name = installed_name(&entry);
    let installed = installer.is_installed(&installed_name);
    let no_upgrade = options.no_upgrade
        && !(entry.kind == Kind::Brew && options.upgrade_formulae.contains(&entry.name));
    let pinned = installer
        .get_installed(&installed_name)
        .is_some_and(|keg| keg.pinned);
    let greedy = entry.option("greedy").is_some_and(Value::truthy);
    let upgradable = installed
        && !no_upgrade
        && !pinned
        && installer
            .is_outdated_greedy(&installed_name, greedy)
            .await
            .ok()
            .flatten()
            .is_some();
    Ok(Plan {
        entry,
        install_name,
        installed_name,
        installed,
        upgradable,
    })
}

/// The extensions `flags` and `$HOMEBREW_BUNDLE_<STEP>_NO_<TYPE>` select
/// for `step` (`CLEANUP` or `DUMP`), in Homebrew's order.
fn selected_extensions(
    flags: &crate::cli::ExtensionFlags,
    step: &str,
    selected: impl Fn(bool, bool, &str) -> bool,
) -> Vec<Ext> {
    bundle_ext::HANDLED
        .iter()
        .copied()
        .filter(|ext| {
            let (asked, off) = flags.get(ext.name());
            let env = format!("HOMEBREW_BUNDLE_{step}_NO_{}", ext.name().to_uppercase());
            selected(asked, off, &env)
        })
        .collect()
}

async fn install_from_file(
    installer: &mut nb_io::Installer,
    path: &Path,
    options: &InstallOptions,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let entries: Vec<Entry> = read_brewfile(path)?
        .into_iter()
        .filter(|entry| !skipped(entry))
        .collect();
    installer.set_cask_config(nb_io::CaskConfig::from_env()?);
    let mut success = 0;
    let mut failure = 0;

    // What the Brewfile trusts, before anything of a tap is read.
    let trusted = trust_entries(&entries)?;
    if !trusted.is_empty() {
        let store = &installer.tap_source()?.trust;
        for (kind, name) in &trusted {
            store.trust(*kind, name)?;
        }
    }

    // Taps first, then what nextbrew cannot install, then the rest.
    let (taps, others): (Vec<Entry>, Vec<Entry>) = entries
        .into_iter()
        .partition(|entry| entry.kind == Kind::Tap);
    let tap_names: Vec<String> = taps.iter().map(|tap| tap.name.clone()).collect();
    for tap in taps {
        if let Some(reason) = unsupported(&tap) {
            say_verb("Tapping", &tap.name);
            say_failed("Tapping", &tap.name, Some(&reason));
            failure += 1;
            continue;
        }
        let tapped = tapped(installer, &tap.name);
        if tapped {
            if !options.quiet {
                println!("Using {}", tap.name);
            }
            success += 1;
        } else {
            say_verb("Tapping", &tap.name);
            if tap_entry(installer, &tap).await {
                success += 1;
            } else {
                say_failed("Tapping", &tap.name, None);
                failure += 1;
            }
        }
    }
    // `ensure_entry_taps_installed!`: the taps formulae and casks are named
    // in full from, unless a `tap` entry was for one.
    for entry in &others {
        if let Some((tap, _)) = entry_tap(entry)
            && !tap_names.contains(&tap.name())
        {
            super::tap::ensure_tapped(installer, &tap, None).await?;
        }
    }
    // Formulae and casks, and other package managers' entries, in the
    // Brewfile's order.
    enum Pending {
        Homebrew(Plan),
        Extension(Ext, Entry),
    }
    let mut pending = Vec::new();
    for entry in others {
        if let Some(reason) = unsupported(&entry) {
            say_verb("Installing", &entry.name);
            say_failed("Installing", &entry.name, Some(&reason));
            failure += 1;
            continue;
        }
        match Ext::of(&entry) {
            Some(ext) => pending.push(Pending::Extension(ext, entry)),
            None => pending.push(Pending::Homebrew(
                plan_entry(installer, entry, options).await?,
            )),
        }
    }

    // `mark_as_installed_on_request!`: what the Brewfile names was asked for.
    let requested: Vec<String> = pending
        .iter()
        .filter_map(|item| match item {
            Pending::Homebrew(plan) if plan.entry.kind == Kind::Brew => {
                Some(plan.installed_name.clone())
            }
            _ => None,
        })
        .collect();
    // Homebrew's own batch, then each batched package manager's (in the
    // order they first come), then the rest one by one.
    let mut batch = Vec::new();
    let mut extension_batches: Vec<(Ext, Vec<Entry>)> = Vec::new();
    let mut remaining = Vec::new();
    for item in pending {
        match item {
            Pending::Homebrew(plan)
                if plan.batchable() && loadable(installer, &plan.entry).await =>
            {
                batch.push(plan)
            }
            Pending::Extension(ext, entry) if ext.batched(&entry) => {
                match extension_batches.iter_mut().find(|(e, _)| *e == ext) {
                    Some((_, group)) => group.push(entry),
                    None => extension_batches.push((ext, vec![entry])),
                }
            }
            item => remaining.push(item),
        }
    }
    // `batch_install_homebrew!`: everything without options in one install
    // and one upgrade, then each checked.
    let mut to_install = Vec::new();
    let mut to_upgrade = Vec::new();
    for plan in &batch {
        if plan.actionable() {
            say_verb(plan.verb(), &plan.entry.name);
            if plan.installed {
                to_upgrade.push(plan.installed_name.clone());
            } else {
                to_install.push(plan.install_name.clone());
            }
        } else if !options.quiet {
            println!("Using {}", plan.entry.name);
        }
    }
    if !to_install.is_empty() && prepare_full_names(installer, &to_install).await {
        let _ = install::execute(installer, to_install, false, false, ui).await;
    }
    if !to_upgrade.is_empty() {
        let full_names: Vec<String> = batch
            .iter()
            .filter(|plan| plan.actionable() && plan.installed)
            .map(|plan| plan.install_name.clone())
            .collect();
        let _ = super::tap::trust_full_names(installer, &full_names);
        let _ = upgrade::execute(installer, to_upgrade, false, false, false, ui).await;
    }
    for plan in &batch {
        let done = installer.is_installed(&plan.installed_name)
            && (!plan.upgradable
                || installer
                    .is_outdated_greedy(&plan.installed_name, false)
                    .await
                    .ok()
                    .flatten()
                    .is_none());
        if done {
            success += 1;
        } else {
            say_failed(plan.verb(), &plan.entry.name, None);
            failure += 1;
        }
    }

    // `batch_install_package_type!`: each extension's entries at once.
    for (ext, group) in &extension_batches {
        let group: Vec<&Entry> = group.iter().collect();
        let (ok, failed) = install_extension(installer, *ext, &group, options, ui).await;
        success += ok;
        failure += failed;
    }

    for item in remaining {
        let plan = match item {
            Pending::Homebrew(plan) => plan,
            Pending::Extension(ext, entry) => {
                let (ok, failed) = install_extension(installer, ext, &[&entry], options, ui).await;
                success += ok;
                failure += failed;
                continue;
            }
        };
        let actionable = plan.actionable();
        if actionable {
            say_verb(plan.verb(), &plan.entry.name);
        } else if !options.quiet {
            println!("Using {}", plan.entry.name);
        }
        // `Brew#install_change_state!` taps a formula's tap first, and a
        // failure there ends the whole run.
        if actionable
            && plan.entry.kind == Kind::Brew
            && let Some((tap, _)) = entry_tap(&plan.entry)
        {
            super::tap::ensure_tapped(installer, &tap, None).await?;
        }
        let ok = match plan.entry.kind {
            Kind::Brew => install_formula(installer, &plan, actionable, options, ui).await,
            _ => install_cask(installer, &plan, actionable, ui).await,
        };
        if ok {
            success += 1;
        } else {
            say_failed(plan.verb(), &plan.entry.name, None);
            failure += 1;
        }
    }

    let installed: Vec<String> = requested
        .into_iter()
        .filter(|name| installer.is_installed(name))
        .collect();
    installer.mark_on_request(&installed)?;
    if failure > 0 {
        return Err(nb_core::Error::ExecutionError {
            message: format!(
                "`nb bundle` failed! {failure} Brewfile {} failed to install",
                plural(failure)
            ),
        });
    }
    if !options.quiet {
        println!(
            "{}",
            style(format!(
                "`nb bundle` complete! {success} Brewfile {} now installed.",
                plural(success)
            ))
            .green()
        );
    }
    Ok(())
}

/// Install one extension's entries: its tool first if missing, then the
/// entries not installed (or, for apps, outdated) in one go, then one by
/// one if that failed. Returns how many succeeded and failed.
async fn install_extension(
    installer: &mut nb_io::Installer,
    ext: Ext,
    group: &[&Entry],
    options: &InstallOptions,
    ui: &mut StdUi,
) -> (usize, usize) {
    let exe = match (ext.executable(installer.prefix()), ext.provider()) {
        (Some(exe), _) => Some(exe),
        (None, None) => None,
        (None, Some(provider)) => {
            let _ = install::execute(installer, vec![provider.to_string()], false, false, ui).await;
            ext.executable(installer.prefix())
        }
    };
    let Some(exe) = exe else {
        for entry in group {
            say_verb("Installing", &entry.name);
            let reason = ext.tool_missing(&entry.name);
            say_failed("Installing", &entry.name, Some(&format!("Error: {reason}")));
        }
        return (0, group.len());
    };
    let prefix = installer.prefix().to_path_buf();
    let installed = Installed::read(ext, &prefix);
    let needs = |entry: &Entry, installed: &Installed| {
        !installed.present(ext, entry) || (!options.no_upgrade && installed.outdated(ext, entry))
    };
    let verbs: Vec<&str> = group.iter().map(|e| ext.verb(e, &installed)).collect();
    let actionable: Vec<&Entry> = group
        .iter()
        .copied()
        .filter(|e| needs(e, &installed))
        .collect();
    for (entry, verb) in group.iter().zip(&verbs) {
        if actionable.contains(entry) {
            say_verb(verb, &entry.name);
        } else if !options.quiet {
            println!("Using {}", entry.name);
        }
    }
    if actionable.is_empty()
        || bundle_ext::install(ext, &exe, &prefix, &actionable, &installed, options.verbose)
    {
        return (group.len(), 0);
    }
    if !ext.batched(group[0]) {
        say_failed(verbs[0], &group[0].name, None);
        return (0, 1);
    }
    let (mut ok, mut failed) = (group.len() - actionable.len(), 0);
    for entry in actionable {
        let now = Installed::read(ext, &prefix);
        if !needs(entry, &now)
            || bundle_ext::install(ext, &exe, &prefix, &[entry], &now, options.verbose)
        {
            ok += 1;
        } else {
            let verb = verbs[group.iter().position(|e| e == &entry).unwrap_or(0)];
            say_failed(verb, &entry.name, None);
            failed += 1;
        }
    }
    (ok, failed)
}

/// `Bundle::Brew#install!` for an entry with options: conflicts, install or
/// upgrade, then its service, link state, postinstall and version file.
async fn install_formula(
    installer: &mut nb_io::Installer,
    plan: &Plan,
    actionable: bool,
    options: &InstallOptions,
    ui: &mut StdUi,
) -> bool {
    let entry = &plan.entry;
    let name = &entry.name;
    let installed = &plan.installed_name;
    let link = entry.option("link");
    let restart_service = entry.option("restart_service").filter(|v| v.truthy());
    let start_service = entry
        .option("start_service")
        .or(restart_service)
        .filter(|v| v.truthy());
    let mut changed = false;

    if actionable {
        let conflicts: Vec<String> = match entry.option("conflicts_with") {
            Some(Value::Array(names)) => names
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        for conflict in conflicts {
            if !installer.is_installed(&conflict) {
                continue;
            }
            if options.verbose {
                println!(
                    "Unlinking {conflict} formula.\nIt is currently installed and conflicts with {name}."
                );
            }
            if installer.unlink(&conflict).is_err() {
                return false;
            }
            if restart_service.is_some() {
                let _ = services::execute(
                    installer,
                    Some(ServicesCommands::Stop {
                        formulas: vec![conflict.clone()],
                        all: false,
                        keep: false,
                        no_wait: false,
                        max_wait: None,
                    }),
                    false,
                )
                .await;
            }
        }
        let result = if plan.installed {
            let _ =
                super::tap::trust_full_names(installer, std::slice::from_ref(&plan.install_name));
            upgrade::execute(installer, vec![installed.clone()], false, false, false, ui).await
        } else {
            if !prepare_full_names(installer, std::slice::from_ref(&plan.install_name)).await {
                return false;
            }
            let no_link = matches!(link, Some(Value::Bool(false)));
            let (build, ignored) = brew_args(&plan.entry).unwrap_or_default();
            for option in ignored {
                eprintln!(
                    "{} {name}: this formula has no --{option} option so it will be ignored!",
                    style("Warning:").yellow().bold()
                );
            }
            install::execute(
                installer,
                vec![plan.install_name.clone()],
                no_link,
                build,
                ui,
            )
            .await
        };
        if result.is_err() || !installer.is_installed(installed) {
            return false;
        }
        changed = true;
    }
    if !installer.is_installed(installed) {
        return false;
    }

    let mut ok = true;
    // Services: restart when changed (or `:always`), else start if stopped.
    let always = matches!(restart_service, Some(Value::Sym(s) | Value::Str(s)) if s == "always");
    let started = nb_io::services::Launchd::current(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
    )
    .find(&nb_io::services::label(installed))
    .running();
    let service = if restart_service.is_some() && (always || changed) {
        Some(ServicesCommands::Restart {
            formulas: vec![installed.clone()],
            all: false,
            file: None,
        })
    } else if start_service.is_some() && !started {
        Some(ServicesCommands::Start {
            formulas: vec![installed.clone()],
            all: false,
            file: None,
        })
    } else {
        None
    };
    if let Some(command) = service {
        ok &= services::execute(installer, Some(command), false)
            .await
            .is_ok();
    }

    // Link state: `link:` true/`:overwrite`, false, or whatever is usual.
    let keg_only = installer.is_keg_only(installed).await.unwrap_or(false);
    let want_linked = match link {
        Some(Value::Bool(true)) => true,
        Some(Value::Sym(s)) if s == "overwrite" => true,
        Some(Value::Bool(false)) => false,
        _ => !keg_only,
    };
    match installer.is_linked(installed) {
        Ok(linked) if want_linked && !linked => {
            ok &= installer.link(installed, keg_only, false).await.is_ok()
        }
        Ok(linked) if !want_linked && linked => ok &= installer.unlink(installed).is_ok(),
        Ok(_) => {}
        Err(_) => ok = false,
    }

    if changed && ok {
        ok &= run_postinstall(name, entry.option("postinstall"));
    }
    if ok
        && let Some(Value::Str(file)) = entry.option("version_file")
        && let Some(keg) = installer.get_installed(installed)
    {
        let version = keg
            .version
            .split('_')
            .next()
            .unwrap_or(&keg.version)
            .to_string();
        ok &= std::fs::write(file, format!("{version}\n")).is_ok();
        if options.verbose {
            println!("Wrote {name} version {version} to {file}");
        }
    }
    ok
}

async fn install_cask(
    installer: &mut nb_io::Installer,
    plan: &Plan,
    actionable: bool,
    ui: &mut StdUi,
) -> bool {
    if actionable {
        let Ok(config) = cask_config(&plan.entry) else {
            return false;
        };
        installer.set_cask_config(config);
        let result = if plan.installed {
            let _ =
                super::tap::trust_full_names(installer, std::slice::from_ref(&plan.install_name));
            upgrade::execute(
                installer,
                vec![plan.installed_name.clone()],
                false,
                false,
                false,
                ui,
            )
            .await
        } else if prepare_full_names(installer, std::slice::from_ref(&plan.install_name)).await {
            install::execute(installer, vec![plan.install_name.clone()], false, false, ui).await
        } else {
            Err(nb_core::Error::ExecutionError {
                message: format!("cannot install {}", plan.install_name),
            })
        };
        installer.set_cask_config(nb_io::CaskConfig::from_env().unwrap_or_default());
        if result.is_err() {
            return false;
        }
    }
    if !installer.is_installed(&plan.installed_name) {
        return false;
    }
    !actionable || run_postinstall(&plan.entry.name, plan.entry.option("postinstall"))
}

/// A `postinstall:` command, run by the shell after a change. It is
/// arbitrary code, so it is always shown first.
fn run_postinstall(name: &str, command: Option<&Value>) -> bool {
    let Some(Value::Str(command)) = command else {
        return true;
    };
    if command.is_empty() {
        return true;
    }
    println!("Running postinstall for {name}: {command}");
    std::process::Command::new("/bin/sh")
        .args(["-c", command])
        .status()
        .is_ok_and(|status| status.success())
}

/// `brew bundle check`: taps, casks, other package managers' entries,
/// formulae (with their link state) and services, in Homebrew's order.
async fn check(
    installer: &nb_io::Installer,
    path: &Path,
    no_upgrade: bool,
    verbose: bool,
    quiet: bool,
) -> Result<(), nb_core::Error> {
    let entries: Vec<Entry> = read_brewfile(path)?
        .into_iter()
        .filter(|entry| !skipped(entry))
        .collect();
    let needs = if no_upgrade {
        "needs to be installed."
    } else {
        "needs to be installed or updated."
    };
    let options = InstallOptions {
        no_upgrade,
        upgrade_formulae: Vec::new(),
        verbose,
        quiet,
    };
    let of = |kind: Kind| entries.iter().filter(move |entry| entry.kind == kind);
    let mut errors = Vec::new();

    for tap in of(Kind::Tap) {
        let tapped = tapped(installer, &tap.name);
        if !tapped {
            errors.push(format!("Tap {} needs to be tapped.", tap.name));
        }
    }
    for cask in of(Kind::Cask) {
        let plan = plan_entry(installer, cask.clone(), &options).await?;
        if !plan.installed || plan.upgradable {
            errors.push(format!("Cask {} {needs}", cask.name));
        }
    }
    // Homebrew's registered extensions, then apps.
    for kind in bundle_ext::CHECK_ORDER {
        let wanted: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.kind == Kind::Extension(kind))
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let Some(ext) = Ext::named(kind) else {
            continue;
        };
        let installed = Installed::read(ext, installer.prefix());
        for entry in wanted {
            let outdated = !no_upgrade && installed.outdated(ext, entry);
            if !installed.has(ext, entry) || outdated {
                errors.push(ext.failure_reason(entry, &installed, no_upgrade));
            }
        }
    }
    for formula in of(Kind::Brew) {
        let plan = plan_entry(installer, formula.clone(), &options).await?;
        if !plan.installed || plan.upgradable {
            errors.push(format!("Formula {} {needs}", formula.name));
            continue;
        }
        // brew does not look at link states with --no-upgrade.
        if no_upgrade {
            continue;
        }
        // `link_status_to_check`: what `link:` asks for, or else the usual
        // state (keg-only formulae unlinked, the rest linked).
        let installed = installed_name(formula);
        let keg_only = installer.is_keg_only(&installed).await.unwrap_or(false);
        let linked = installer.is_linked(&installed).unwrap_or(false);
        let wanted = match formula.option("link") {
            Some(Value::Bool(true)) => Some(true),
            Some(Value::Sym(s)) if s == "overwrite" => Some(true),
            Some(Value::Bool(false)) => Some(false),
            Some(_) => Some(!keg_only),
            None if keg_only && linked => Some(false),
            None if !keg_only && !linked => Some(true),
            None => None,
        };
        match wanted {
            Some(true) if !linked => {
                errors.push(format!("Formula {} needs to be linked.", formula.name))
            }
            Some(false) if linked => {
                errors.push(format!("Formula {} needs to be unlinked.", formula.name))
            }
            _ => {}
        }
    }
    let launchd = nb_io::services::Launchd::current(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
    );
    for formula in of(Kind::Brew) {
        let wants_start = formula
            .option("start_service")
            .or(formula.option("restart_service"))
            .is_some_and(Value::truthy);
        if wants_start
            && !launchd
                .find(&nb_io::services::label(&installed_name(formula)))
                .running()
        {
            errors.push(format!("Service {} needs to be started.", formula.name));
        }
    }

    if errors.is_empty() {
        if !quiet {
            println!("The Brewfile's dependencies are satisfied.");
        }
        return Ok(());
    }
    eprintln!("nb bundle can't satisfy your Brewfile's dependencies.");
    if verbose {
        for error in &errors {
            eprintln!("→ {error}");
        }
    } else {
        eprintln!("Run `nb bundle check --verbose` to list unmet dependencies.");
    }
    eprintln!("Satisfy missing dependencies with `nb bundle install`.");
    std::process::exit(1);
}

/// What `nb bundle cleanup` cleans up.
struct CleanupTypes {
    formulae: bool,
    casks: bool,
    taps: bool,
    extensions: Vec<Ext>,
}

/// `brew bundle cleanup`: what is installed but neither listed nor needed
/// by what is listed. Without `--force` it is only shown, after asking on a
/// terminal whether to go ahead.
async fn cleanup(
    installer: &mut nb_io::Installer,
    path: &Path,
    force: bool,
    zap: bool,
    types: &CleanupTypes,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let CleanupTypes {
        formulae,
        casks,
        taps,
        ref extensions,
    } = *types;
    let entries = read_brewfile(path)?;
    let kept_casks: Vec<String> = entries
        .iter()
        .filter(|entry| entry.kind == Kind::Cask)
        .map(|entry| entry.name.clone())
        .collect();
    let installed = installer.list_installed()?;
    let casks_to_remove: Vec<String> = if casks {
        let mut tokens: Vec<String> = installed
            .iter()
            .filter_map(|keg| keg.name.strip_prefix("cask:"))
            .filter(|token| !kept_casks.iter().any(|kept| kept == token))
            .map(str::to_string)
            .collect();
        tokens.sort();
        tokens
    } else {
        Vec::new()
    };

    let mut formulae_to_remove = Vec::new();
    // Listed formulae (aliases and old names resolved), the formulae the
    // listed casks depend on, and everything those need.
    let mut kept: Vec<String> = Vec::new();
    if formulae || taps {
        let mut roots: Vec<String> = entries
            .iter()
            .filter(|entry| entry.kind == Kind::Brew)
            .map(|entry| entry.name.clone())
            .collect();
        for token in &kept_casks {
            if let Ok(json) = installer.cask(token).await {
                let deps = &json["depends_on"]["formula"];
                match deps {
                    serde_json::Value::String(one) => roots.push(one.clone()),
                    serde_json::Value::Array(many) => {
                        roots.extend(many.iter().filter_map(|v| v.as_str().map(str::to_string)))
                    }
                    _ => {}
                }
            }
        }
        for root in roots {
            let name = match installer.formula(&root).await {
                Ok(formula) => formula.name,
                Err(_) => root.rsplit('/').next().unwrap_or(&root).to_string(),
            };
            if let Ok(deps) = installer.dependencies(&name, false).await {
                kept.extend(deps);
            }
            kept.push(name);
        }
    }
    if formulae {
        for name in dump_order(installer).await? {
            if !kept.contains(&name) {
                formulae_to_remove.push(name);
            }
        }
    }
    // `taps_to_untap`: taps neither listed, nor named by a listed formula or
    // cask, nor the source of a formula kept.
    let mut taps_to_remove: Vec<String> = Vec::new();
    if taps && let Ok(source) = installer.tap_source() {
        let mut kept_taps: Vec<String> = entries
            .iter()
            .filter_map(|entry| match entry.kind {
                Kind::Tap => Some(entry.name.clone()),
                _ => entry_tap(entry).map(|(tap, _)| tap.name()),
            })
            .collect();
        for name in &kept {
            let tap = match installer.get_installed(name) {
                Some(keg) => keg.tap,
                None => installer
                    .formula(name)
                    .await
                    .ok()
                    .and_then(|formula| formula.third_party_tap().map(str::to_string)),
            };
            kept_taps.extend(tap);
        }
        taps_to_remove = source
            .taps
            .installed()
            .into_iter()
            .map(|tap| tap.name())
            .filter(|tap| !kept_taps.contains(tap) && tap != "homebrew/core")
            .collect();
    }

    let extension_items: Vec<(Ext, Vec<(String, String)>)> = bundle_ext::HANDLED
        .iter()
        .filter(|ext| extensions.contains(ext))
        .map(|ext| {
            let installed = Installed::read(*ext, installer.prefix());
            (*ext, bundle_ext::cleanup_items(*ext, &entries, &installed))
        })
        .filter(|(_, items)| !items.is_empty())
        .collect();

    if force {
        // The Brewfile's trust is all that stays trusted.
        installer
            .tap_source()?
            .trust
            .replace(&trust_entries(&entries)?)?;
        let requested: Vec<String> = entries
            .iter()
            .filter(|entry| entry.kind == Kind::Brew)
            .map(installed_name)
            .filter(|name| installer.is_installed(name))
            .collect();
        installer.mark_on_request(&requested)?;
        if !casks_to_remove.is_empty() {
            let names = casks_to_remove
                .iter()
                .map(|t| format!("cask:{t}"))
                .collect();
            let _ = super::uninstall::execute(installer, names, false, zap, false, true, ui).await;
            let n = casks_to_remove.len();
            println!("Uninstalled {n} cask{}", if n == 1 { "" } else { "s" });
        }
        if !formulae_to_remove.is_empty() {
            let _ = super::uninstall::execute(
                installer,
                formulae_to_remove.clone(),
                false,
                false,
                false,
                true,
                ui,
            )
            .await;
            let n = formulae_to_remove.len();
            println!("Uninstalled {n} formula{}", if n == 1 { "" } else { "e" });
        }
        if !taps_to_remove.is_empty() {
            let _ = super::tap::untap(installer, taps_to_remove, false, ui).await;
        }
        for (ext, items) in &extension_items {
            if let Some(exe) = ext.executable(installer.prefix()) {
                bundle_ext::cleanup(*ext, &exe, items);
            }
        }
        return Ok(());
    }

    let mut would_uninstall = false;
    for (heading, names) in [
        ("Would uninstall casks:", &casks_to_remove),
        ("Would uninstall formulae:", &formulae_to_remove),
        ("Would untap:", &taps_to_remove),
    ] {
        if !names.is_empty() {
            println!("{heading}");
            for name in names {
                println!("{name}");
            }
            would_uninstall = true;
        }
    }
    for (ext, items) in &extension_items {
        println!("Would uninstall {}:", ext.cleanup_heading());
        for (_, shown) in items {
            println!("{shown}");
        }
        would_uninstall = true;
    }
    if !would_uninstall {
        return Ok(());
    }
    if confirm("cleanup") {
        return Box::pin(cleanup(installer, path, true, zap, types, ui)).await;
    }
    println!("Run `nb bundle cleanup --force` to make these changes.");
    std::process::exit(1);
}

/// `Homebrew::Ask.confirm?`: on a terminal, `y` goes ahead; `n`, Escape,
/// Ctrl-C or Ctrl-D end with status 1. Elsewhere, no.
fn confirm(action: &str) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return false;
    }
    println!(
        "{} {}",
        style("==>").blue().bold(),
        style(format!("Do you want to proceed with the {action}? [y/n]")).bold()
    );
    let term = console::Term::stdout();
    loop {
        match term.read_char() {
            Ok('y' | 'Y') => return true,
            Ok('n' | 'N' | '\u{1b}' | '\u{3}' | '\u{4}') | Err(_) => std::process::exit(1),
            Ok(_) => println!("Invalid input. Please press 'y' to proceed, or 'n' to abort."),
        }
    }
}

/// Installed formulae, each after the formulae it depends on.
async fn dump_order(installer: &nb_io::Installer) -> Result<Vec<String>, nb_core::Error> {
    let mut names: Vec<String> = installer
        .list_installed()?
        .into_iter()
        .filter(|keg| !keg.name.starts_with("cask:"))
        .map(|keg| keg.name)
        .collect();
    // By full name, the taps' after Homebrew's own.
    names.sort_by_cached_key(|name| {
        let full = installer.installed_full_name(name);
        (full.contains('/'), full)
    });
    let mut graph = std::collections::HashMap::new();
    for name in &names {
        let mut deps: Vec<String> = installer
            .dependencies(name, false)
            .await
            .map(|deps| deps.into_iter().filter(|dep| names.contains(dep)).collect())
            .unwrap_or_default();
        deps.sort();
        graph.insert(name.clone(), deps);
    }
    fn visit(
        name: &str,
        graph: &std::collections::HashMap<String, Vec<String>>,
        seen: &mut std::collections::HashSet<String>,
        order: &mut Vec<String>,
    ) {
        if !seen.insert(name.to_string()) {
            return;
        }
        for dep in graph.get(name).into_iter().flatten() {
            visit(dep, graph, seen, order);
        }
        order.push(name.to_string());
    }
    let mut order = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in &names {
        visit(name, &graph, &mut seen, &mut order);
    }
    Ok(order)
}

struct DumpOptions {
    taps: bool,
    formulae: bool,
    casks: bool,
    extensions: Vec<Ext>,
    describe: bool,
    no_restart: bool,
}

/// `Dumper.dump_brewfile`.
async fn dump_to_file(
    installer: &mut nb_io::Installer,
    path: &Path,
    force: bool,
    options: &DumpOptions,
) -> Result<(), nb_core::Error> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    if absolute.exists() && !force && absolute != Path::new("/dev/stdout") {
        return Err(nb_core::Error::FileError {
            message: format!("{} already exists", absolute.display()),
        });
    }
    let content = build_brewfile(installer, options).await?;
    std::fs::write(&absolute, content).map_err(|e| nb_core::Error::FileError {
        message: format!("failed to write {}: {e}", absolute.display()),
    })
}

/// `Dumper.build_brewfile`: taps, formulae asked for, then casks, each
/// section separated by a newline.
async fn build_brewfile(
    installer: &nb_io::Installer,
    options: &DumpOptions,
) -> Result<String, nb_core::Error> {
    let mut sections = Vec::new();
    if options.taps {
        let installed = installer.list_installed()?;
        let dumped_formulae: Vec<String> = installed
            .iter()
            .filter(|keg| options.formulae && keg.on_request && !keg.name.starts_with("cask:"))
            .map(|keg| installer.installed_full_name(&keg.name))
            .collect();
        let dumped_casks: Vec<String> = installed
            .iter()
            .filter_map(|keg| keg.name.strip_prefix("cask:").filter(|_| options.casks))
            .map(|token| cask_full_name(installer, token))
            .collect();
        sections.push(dump_taps(installer, &dumped_formulae, &dumped_casks));
    }
    if options.formulae {
        sections.push(dump_formulae(installer, options).await?);
    }
    if options.casks {
        sections.push(dump_casks(installer, options.describe).await?);
    }
    for ext in &options.extensions {
        sections.push(bundle_ext::dump(
            *ext,
            &Installed::read(*ext, installer.prefix()),
        ));
    }
    let sections: Vec<String> = sections.into_iter().filter(|s| !s.is_empty()).collect();
    Ok(format!("{}\n", sections.join("\n")))
}

/// `Cask#full_name`: `user/repo/token` for a cask from a third-party tap.
fn cask_full_name(installer: &nb_io::Installer, token: &str) -> String {
    match installer
        .get_installed(&format!("cask:{token}"))
        .and_then(|keg| keg.tap)
    {
        Some(tap) => format!("{tap}/{token}"),
        None => token.to_string(),
    }
}

/// `Bundle::Tap.dump`: each tap, with its remote when that is not its
/// GitHub repository, and what of it is trusted: the tap itself, or the
/// formulae and casks trusted that are not dumped with `trusted: true`.
fn dump_taps(
    installer: &nb_io::Installer,
    dumped_formulae: &[String],
    dumped_casks: &[String],
) -> String {
    let Ok(source) = installer.tap_source() else {
        return String::new();
    };
    let mut lines = Vec::new();
    for tap in source.taps.installed() {
        let remote = source.taps.remote(&tap).filter(|remote| !remote.is_empty());
        let mut line = format!("tap \"{tap}\"");
        if let Some(remote) = remote.as_deref()
            && remote != tap.default_remote()
        {
            line += &format!(", \"{remote}\"");
        }
        let explicitly = !tap.is_official()
            && source
                .trust
                .entries(TrustKind::Tap)
                .iter()
                .any(|reference| tap.matches_reference(reference, remote.as_deref()));
        if explicitly {
            line += ", trusted: true";
        } else {
            let mut kinds = Vec::new();
            for (kind, key, dumped) in [
                (TrustKind::Formula, "formulae", dumped_formulae),
                (TrustKind::Cask, "casks", dumped_casks),
                (TrustKind::Command, "commands", &[]),
            ] {
                let mut items: Vec<String> = source
                    .trust
                    .entries(kind)
                    .iter()
                    .filter_map(|entry| {
                        let (reference, item) = entry.rsplit_once('/')?;
                        if reference.is_empty() || item.is_empty() {
                            return None;
                        }
                        if reference != tap.name()
                            && !tap.matches_reference(reference, remote.as_deref())
                        {
                            return None;
                        }
                        if dumped.contains(&format!("{tap}/{item}")) {
                            return None;
                        }
                        Some(item.to_string())
                    })
                    .collect();
                items.sort();
                items.dedup();
                if !items.is_empty() {
                    let items: Vec<String> = items.iter().map(|item| format!("{item:?}")).collect();
                    kinds.push(format!("{key}: [{}]", items.join(", ")));
                }
            }
            if !kinds.is_empty() {
                line += &format!(", trusted: {{ {} }}", kinds.join(", "));
            }
        }
        lines.push(line);
    }
    lines.sort();
    lines.dedup();
    lines.join("\n")
}

fn describe_lines(desc: Option<&str>) -> String {
    desc.filter(|desc| !desc.is_empty())
        .map(|desc| desc.lines().map(|line| format!("# {line}\n")).collect())
        .unwrap_or_default()
}

/// `Bundle::Brew.dump`: formulae installed on request, each after the
/// formulae it depends on (Homebrew's topological sort).
async fn dump_formulae(
    installer: &nb_io::Installer,
    options: &DumpOptions,
) -> Result<String, nb_core::Error> {
    let order = dump_order(installer).await?;

    let launchd = nb_io::services::Launchd::current(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
    );
    let trusted = installer
        .tap_source()
        .map(|source| source.trust.entries(TrustKind::Formula))
        .unwrap_or_default();
    let mut lines = Vec::new();
    for name in order {
        let Some(keg) = installer.get_installed(&name) else {
            continue;
        };
        if !keg.on_request {
            continue;
        }
        let formula = installer.formula(&name).await.ok();
        let mut line = if options.describe {
            describe_lines(formula.as_ref().and_then(|f| f.desc.as_deref()))
        } else {
            String::new()
        };
        let full_name = installer.installed_full_name(&name);
        line += &format!("brew \"{full_name}\"");
        if !options.no_restart
            && cfg!(target_os = "macos")
            && launchd.find(&nb_io::services::label(&name)).running()
        {
            line += ", restart_service: :changed";
        }
        let keg_only = formula.as_ref().is_some_and(|f| f.is_keg_only());
        match installer.is_linked(&name) {
            Ok(true) if keg_only => line += ", link: true",
            Ok(false) if !keg_only => line += ", link: false",
            _ => {}
        }
        if trusted.contains(&full_name) {
            line += ", trusted: true";
        }
        lines.push(line);
    }
    Ok(lines.join("\n"))
}

/// `Bundle::Cask.dump`: every installed cask, by token, named in full.
async fn dump_casks(
    installer: &nb_io::Installer,
    describe: bool,
) -> Result<String, nb_core::Error> {
    let mut tokens: Vec<String> = installer
        .list_installed()?
        .into_iter()
        .filter_map(|keg| keg.name.strip_prefix("cask:").map(str::to_string))
        .collect();
    tokens.sort();
    let trusted = installer
        .tap_source()
        .map(|source| source.trust.entries(TrustKind::Cask))
        .unwrap_or_default();
    let mut lines = Vec::new();
    for token in tokens {
        let mut line = String::new();
        if describe {
            let json = installer.cask(&token).await.ok();
            line += &describe_lines(json.as_ref().and_then(|j| j["desc"].as_str()));
        }
        let full_name = cask_full_name(installer, &token);
        line += &format!("cask \"{full_name}\"");
        // `explicit_s`: the directories it was installed with, home as `~`.
        let name = format!("cask:{token}");
        if let Some(keg) = installer.get_installed(&name) {
            let config = nb_io::CaskConfig::load(&installer.keg_path(&name, &keg.version));
            if !config.dirs.is_empty() {
                let home = std::env::var("HOME").unwrap_or_default();
                let args: Vec<String> = config
                    .dirs
                    .iter()
                    .map(|(key, dir)| {
                        let dir = dir.to_string_lossy();
                        let dir = match dir.strip_prefix(&home) {
                            Some(rest) if !home.is_empty() => format!("~{rest}"),
                            _ => dir.into_owned(),
                        };
                        format!("{key}: \"{dir}\"")
                    })
                    .collect();
                line += &format!(", args: {{ {} }}", args.join(", "));
            }
        }
        if trusted.contains(&full_name) {
            line += ", trusted: true";
        }
        lines.push(line);
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source: &str) -> Entry {
        brewfile::parse(source).unwrap().remove(0)
    }

    #[test]
    fn says_why_an_entry_cannot_be_installed() {
        for source in [
            "brew \"jq\"",
            "tap \"homebrew/cask\"",
            "tap \"hashicorp/tap\"",
            "brew \"hashicorp/tap/terraform\"",
            "cask \"microsoft/git/microsoft-git\"",
            "flatpak \"org.gimp.GIMP\"",
            "winget \"Git.Git\"",
            "brew \"x\", args: [\"build-from-source\", \"with-y\"]",
            "cask \"firefox\", args: { language: \"ja\" }",
        ] {
            assert!(unsupported(&entry(source)).is_none(), "{source}");
        }
        for source in ["tap \"homebrew/bundle\"", "brew \"x\", args: [\"HEAD\"]"] {
            assert!(unsupported(&entry(source)).is_some(), "{source}");
        }
    }

    #[test]
    fn names_a_taps_packages_in_full_to_install_them() {
        let names = |source: &str| {
            let entry = entry(source);
            (install_name(&entry), installed_name(&entry))
        };
        assert_eq!(names("brew \"jq\""), ("jq".into(), "jq".into()));
        assert_eq!(
            names("brew \"hashicorp/tap/terraform\""),
            ("hashicorp/tap/terraform".into(), "terraform".into())
        );
        assert_eq!(
            names("cask \"anthropics/tap/ant\""),
            ("cask:anthropics/tap/ant".into(), "cask:ant".into())
        );
        assert_eq!(
            names("cask \"firefox\""),
            ("cask:firefox".into(), "cask:firefox".into())
        );
    }

    #[test]
    fn trusts_what_the_brewfile_marks_trusted() {
        let entries = brewfile::parse(
            "tap \"a/one\", trusted: true\n\
             tap \"b/two\", \"https://example.com/two.git\", trusted: true\n\
             tap \"c/three\", trusted: { formulae: [\"x\", \"c/three/y\"], cask: \"z\", commands: [\"cmd\"] }\n\
             tap \"d/four\", \"https://example.com/four.git\"\n\
             brew \"d/four/w\", trusted: true\n\
             cask \"e/five/v\", trusted: true\n\
             brew \"jq\", trusted: true\n\
             brew \"f/six/u\"\n",
        )
        .unwrap();
        assert_eq!(
            trust_entries(&entries).unwrap(),
            vec![
                (TrustKind::Tap, "a/one".to_string()),
                (TrustKind::Tap, "https://example.com/two.git".to_string()),
                (TrustKind::Formula, "c/three/x".to_string()),
                (TrustKind::Formula, "c/three/y".to_string()),
                (TrustKind::Cask, "c/three/z".to_string()),
                (TrustKind::Command, "c/three/cmd".to_string()),
                (
                    TrustKind::Formula,
                    "https://example.com/four.git/w".to_string()
                ),
                (TrustKind::Cask, "e/five/v".to_string()),
            ]
        );
        let bad = brewfile::parse("tap \"a/one\", trusted: { apps: [\"x\"] }\n").unwrap();
        assert!(trust_entries(&bad).is_err());
    }

    #[test]
    fn only_entries_without_options_are_batched() {
        let plan = |source: &str| Plan {
            entry: entry(source),
            install_name: String::new(),
            installed_name: String::new(),
            installed: false,
            upgradable: false,
        };
        assert!(plan("brew \"jq\"").batchable());
        assert!(plan("cask \"firefox\"").batchable());
        assert!(!plan("brew \"jq\", link: false").batchable());
        assert!(!plan("cask \"firefox\", greedy: true").batchable());
    }
}
