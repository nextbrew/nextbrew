//! `nb bundle`: `brew bundle` over a Brewfile read without Ruby (see
//! [`crate::brewfile`]). Entries nextbrew cannot install (other taps,
//! `mas`, `vscode`, cask `args`, build `args`) count as failures and say
//! why, as a failed install does in Homebrew; none are skipped silently.

use std::path::{Path, PathBuf};

use console::style;

use super::{install, services, upgrade};
use crate::brewfile::{self, Entry, Kind, Value};
use crate::cli::{BundleCommands, ServicesCommands};
use crate::ui::StdUi;

/// The taps whose packages nextbrew reads from the API.
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
        } => {
            let no_type_args = !(formula || cask || tap);
            let selected = |asked: bool, off: bool, env: &str| {
                !off && std::env::var_os(env).is_none() && (asked || all || no_type_args)
            };
            let formulae = selected(formula, no_formula, "HOMEBREW_BUNDLE_CLEANUP_NO_BREW");
            let casks = selected(cask, no_cask, "HOMEBREW_BUNDLE_CLEANUP_NO_CASK");
            // nextbrew has no taps to untap.
            let _ = selected(tap, no_tap, "HOMEBREW_BUNDLE_CLEANUP_NO_TAP");
            let path = brewfile_path(file.as_deref(), global)?;
            cleanup(installer, &path, force, zap, formulae, casks, ui).await
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
        } => {
            // `type_selected?`: the types asked for, or all of them, minus
            // those turned off by flag or environment.
            let no_type_args = !(formula || cask || tap);
            let selected = |asked: bool, off: bool, env: &str| {
                !off && std::env::var_os(env).is_none() && (asked || no_type_args)
            };
            let options = DumpOptions {
                formulae: selected(formula, no_formula, "HOMEBREW_BUNDLE_DUMP_NO_BREW"),
                casks: selected(cask, no_cask, "HOMEBREW_BUNDLE_DUMP_NO_CASK"),
                describe: !no_describe && std::env::var_os("HOMEBREW_BUNDLE_NO_DESCRIBE").is_none(),
                no_restart,
            };
            // nextbrew has no taps to dump.
            let _ = selected(tap, no_tap, "HOMEBREW_BUNDLE_DUMP_NO_TAP");
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

/// The tap a `user/repo/name` entry is from, unless it is a built-in one.
fn foreign_tap(name: &str) -> Option<String> {
    let (tap, _) = name.rsplit_once('/')?;
    (!BUILT_IN_TAPS.contains(&tap)).then(|| tap.to_string())
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
        Kind::Tap if !BUILT_IN_TAPS.contains(&entry.name.as_str()) => Some(format!(
            "nextbrew installs from homebrew/core and homebrew/cask only; tap `{}` with brew",
            entry.name
        )),
        Kind::Tap => None,
        Kind::Brew => {
            if let Some(tap) = foreign_tap(&entry.name) {
                return Some(format!(
                    "formula from the '{tap}' tap; install it with brew"
                ));
            }
            match entry.option("args") {
                Some(Value::Array(args)) if !args.is_empty() => Some(format!(
                    "build options ({}) need a build from source, which nextbrew does not do",
                    args.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                _ => None,
            }
        }
        Kind::Cask => {
            if let Some(tap) = full_name(entry).and_then(|name| foreign_tap(&name)) {
                return Some(format!("cask from the '{tap}' tap; install it with brew"));
            }
            match entry.option("args") {
                Some(Value::Hash(args)) if !args.is_empty() => Some(format!(
                    "cask args ({}) are not supported by nextbrew",
                    args.iter()
                        .map(|(k, v)| format!("{k}: {v}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                _ => None,
            }
        }
        Kind::Extension(kind) => Some(format!("nextbrew does not install `{kind}` entries")),
    }
}

/// Where an entry stands before anything is installed.
struct Plan {
    entry: Entry,
    install_name: String,
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
            Kind::Cask => !extra(&["full_name", "trusted", "args"]),
            _ => false,
        }
    }
}

async fn plan_entry(
    installer: &nb_io::Installer,
    entry: Entry,
    options: &InstallOptions,
) -> Result<Plan, nb_core::Error> {
    let install_name = match entry.kind {
        Kind::Cask => format!("cask:{}", entry.name),
        _ => entry.name.clone(),
    };
    let installed = installer.is_installed(&install_name);
    let no_upgrade = options.no_upgrade
        && !(entry.kind == Kind::Brew && options.upgrade_formulae.contains(&entry.name));
    let pinned = installer
        .get_installed(&install_name)
        .is_some_and(|keg| keg.pinned);
    let greedy = entry.option("greedy").is_some_and(Value::truthy);
    let upgradable = installed
        && !no_upgrade
        && !pinned
        && installer
            .is_outdated_greedy(&install_name, greedy)
            .await
            .ok()
            .flatten()
            .is_some();
    Ok(Plan {
        entry,
        install_name,
        installed,
        upgradable,
    })
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
    let mut success = 0;
    let mut failure = 0;

    // Taps first, then what nextbrew cannot install, then the rest.
    let (taps, others): (Vec<Entry>, Vec<Entry>) = entries
        .into_iter()
        .partition(|entry| entry.kind == Kind::Tap);
    for tap in taps {
        match unsupported(&tap) {
            None => {
                if !options.quiet {
                    println!("Using {}", tap.name);
                }
                success += 1;
            }
            Some(reason) => {
                say_verb("Tapping", &tap.name);
                say_failed("Tapping", &tap.name, Some(&reason));
                failure += 1;
            }
        }
    }
    let mut plans = Vec::new();
    for entry in others {
        if let Some(reason) = unsupported(&entry) {
            say_verb("Installing", &entry.name);
            say_failed("Installing", &entry.name, Some(&reason));
            failure += 1;
            continue;
        }
        plans.push(plan_entry(installer, entry, options).await?);
    }

    // `mark_as_installed_on_request!`: what the Brewfile names was asked for.
    let requested: Vec<String> = plans
        .iter()
        .filter(|plan| plan.entry.kind == Kind::Brew)
        .map(|plan| plan.install_name.clone())
        .collect();
    let (batch, single): (Vec<Plan>, Vec<Plan>) = plans.into_iter().partition(Plan::batchable);
    // `batch_install_homebrew!`: everything without options in one install
    // and one upgrade, then each checked.
    let mut to_install = Vec::new();
    let mut to_upgrade = Vec::new();
    for plan in &batch {
        if plan.actionable() {
            say_verb(plan.verb(), &plan.entry.name);
            if plan.installed {
                to_upgrade.push(plan.install_name.clone());
            } else {
                to_install.push(plan.install_name.clone());
            }
        } else if !options.quiet {
            println!("Using {}", plan.entry.name);
        }
    }
    if !to_install.is_empty() {
        let _ = install::execute(installer, to_install, false, false, ui).await;
    }
    if !to_upgrade.is_empty() {
        let _ = upgrade::execute(installer, to_upgrade, false, false, ui).await;
    }
    for plan in &batch {
        let done = installer.is_installed(&plan.install_name)
            && (!plan.upgradable
                || installer
                    .is_outdated_greedy(&plan.install_name, false)
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

    for plan in single {
        let actionable = plan.actionable();
        if actionable {
            say_verb(plan.verb(), &plan.entry.name);
        } else if !options.quiet {
            println!("Using {}", plan.entry.name);
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
            upgrade::execute(installer, vec![name.clone()], false, false, ui).await
        } else {
            let no_link = matches!(link, Some(Value::Bool(false)));
            install::execute(installer, vec![name.clone()], no_link, false, ui).await
        };
        if result.is_err() || !installer.is_installed(name) {
            return false;
        }
        changed = true;
    }
    if !installer.is_installed(name) {
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
    .find(&nb_io::services::label(name))
    .running();
    let service = if restart_service.is_some() && (always || changed) {
        Some(ServicesCommands::Restart {
            formulas: vec![name.clone()],
            all: false,
            file: None,
        })
    } else if start_service.is_some() && !started {
        Some(ServicesCommands::Start {
            formulas: vec![name.clone()],
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
    let keg_only = installer.is_keg_only(name).await.unwrap_or(false);
    let want_linked = match link {
        Some(Value::Bool(true)) => true,
        Some(Value::Sym(s)) if s == "overwrite" => true,
        Some(Value::Bool(false)) => false,
        _ => !keg_only,
    };
    match installer.is_linked(name) {
        Ok(linked) if want_linked && !linked => ok &= installer.link(name, keg_only).await.is_ok(),
        Ok(linked) if !want_linked && linked => ok &= installer.unlink(name).is_ok(),
        Ok(_) => {}
        Err(_) => ok = false,
    }

    if changed && ok {
        ok &= run_postinstall(name, entry.option("postinstall"));
    }
    if ok
        && let Some(Value::Str(file)) = entry.option("version_file")
        && let Some(keg) = installer.get_installed(name)
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
        let result = if plan.installed {
            upgrade::execute(installer, vec![plan.install_name.clone()], false, false, ui).await
        } else {
            install::execute(installer, vec![plan.install_name.clone()], false, false, ui).await
        };
        if result.is_err() {
            return false;
        }
    }
    if !installer.is_installed(&plan.install_name) {
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
        if !BUILT_IN_TAPS.contains(&tap.name.as_str()) {
            errors.push(format!("Tap {} needs to be tapped.", tap.name));
        }
    }
    for cask in of(Kind::Cask) {
        let plan = plan_entry(installer, cask.clone(), &options).await?;
        if !plan.installed || plan.upgradable {
            errors.push(format!("Cask {} {needs}", cask.name));
        }
    }
    for entry in entries
        .iter()
        .filter(|entry| matches!(entry.kind, Kind::Extension(_)))
    {
        if let Kind::Extension(kind) = entry.kind {
            errors.push(format!(
                "{} {} cannot be checked: nextbrew does not handle `{kind}` entries.",
                capitalize(kind),
                entry.name
            ));
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
        let keg_only = installer.is_keg_only(&formula.name).await.unwrap_or(false);
        let linked = installer.is_linked(&formula.name).unwrap_or(false);
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
                .find(&nb_io::services::label(&formula.name))
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

/// `brew bundle cleanup`: what is installed but neither listed nor needed
/// by what is listed. Without `--force` it is only shown, after asking on a
/// terminal whether to go ahead.
async fn cleanup(
    installer: &mut nb_io::Installer,
    path: &Path,
    force: bool,
    zap: bool,
    formulae: bool,
    casks: bool,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
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
    if formulae {
        // Listed formulae (aliases and old names resolved), the formulae the
        // listed casks depend on, and everything those need.
        let mut kept: Vec<String> = Vec::new();
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
            let name = installer
                .formula(&root)
                .await
                .map(|f| f.name)
                .unwrap_or(root);
            if let Ok(deps) = installer.dependencies(&name, false).await {
                kept.extend(deps);
            }
            kept.push(name);
        }
        for name in dump_order(installer).await? {
            if !kept.contains(&name) {
                formulae_to_remove.push(name);
            }
        }
    }

    if force {
        let requested: Vec<String> = entries
            .iter()
            .filter(|entry| entry.kind == Kind::Brew && installer.is_installed(&entry.name))
            .map(|entry| entry.name.clone())
            .collect();
        installer.mark_on_request(&requested)?;
        if !casks_to_remove.is_empty() {
            let names = casks_to_remove
                .iter()
                .map(|t| format!("cask:{t}"))
                .collect();
            let _ = super::uninstall::execute(installer, names, false, zap, ui);
            let n = casks_to_remove.len();
            println!("Uninstalled {n} cask{}", if n == 1 { "" } else { "s" });
        }
        if !formulae_to_remove.is_empty() {
            let _ =
                super::uninstall::execute(installer, formulae_to_remove.clone(), false, false, ui);
            let n = formulae_to_remove.len();
            println!("Uninstalled {n} formula{}", if n == 1 { "" } else { "e" });
        }
        return Ok(());
    }

    let mut would_uninstall = false;
    for (heading, names) in [
        ("Would uninstall casks:", &casks_to_remove),
        ("Would uninstall formulae:", &formulae_to_remove),
    ] {
        if !names.is_empty() {
            println!("{heading}");
            for name in names {
                println!("{name}");
            }
            would_uninstall = true;
        }
    }
    if !would_uninstall {
        return Ok(());
    }
    if confirm("cleanup") {
        return Box::pin(cleanup(installer, path, true, zap, formulae, casks, ui)).await;
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
    names.sort_by(|a, b| (a.contains('/'), a).cmp(&(b.contains('/'), b)));
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

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

struct DumpOptions {
    formulae: bool,
    casks: bool,
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

/// `Dumper.build_brewfile`: formulae asked for, then casks, each section
/// separated by a newline.
async fn build_brewfile(
    installer: &nb_io::Installer,
    options: &DumpOptions,
) -> Result<String, nb_core::Error> {
    let mut sections = Vec::new();
    if options.formulae {
        sections.push(dump_formulae(installer, options).await?);
    }
    if options.casks {
        sections.push(dump_casks(installer, options.describe).await?);
    }
    let sections: Vec<String> = sections.into_iter().filter(|s| !s.is_empty()).collect();
    Ok(format!("{}\n", sections.join("\n")))
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
        line += &format!("brew \"{name}\"");
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
        lines.push(line);
    }
    Ok(lines.join("\n"))
}

/// `Bundle::Cask.dump`: every installed cask, by token.
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
    let mut lines = Vec::new();
    for token in tokens {
        let mut line = String::new();
        if describe {
            let json = installer.cask(&token).await.ok();
            line += &describe_lines(json.as_ref().and_then(|j| j["desc"].as_str()));
        }
        line += &format!("cask \"{token}\"");
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
        assert!(unsupported(&entry("brew \"jq\"")).is_none());
        assert!(unsupported(&entry("tap \"homebrew/cask\"")).is_none());
        for source in [
            "tap \"hashicorp/tap\"",
            "brew \"hashicorp/tap/terraform\"",
            "brew \"x\", args: [\"HEAD\"]",
            "cask \"microsoft/git/microsoft-git\"",
            "cask_args appdir: \"~/Apps\"\ncask \"firefox\"",
            "mas \"Xcode\", id: 497799835",
        ] {
            assert!(unsupported(&entry(source)).is_some(), "{source}");
        }
    }

    #[test]
    fn only_entries_without_options_are_batched() {
        let plan = |source: &str| Plan {
            entry: entry(source),
            install_name: String::new(),
            installed: false,
            upgradable: false,
        };
        assert!(plan("brew \"jq\"").batchable());
        assert!(plan("cask \"firefox\"").batchable());
        assert!(!plan("brew \"jq\", link: false").batchable());
        assert!(!plan("cask \"firefox\", greedy: true").batchable());
    }
}
