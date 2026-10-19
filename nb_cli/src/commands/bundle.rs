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
        BundleCommands::Dump { file, force } => dump_to_file(installer, &file, force),
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

fn dump_to_file(
    installer: &mut nb_io::Installer,
    file_path: &Path,
    force: bool,
) -> Result<(), nb_core::Error> {
    if file_path.exists() && !force {
        return Err(nb_core::Error::FileError {
            message: format!(
                "file {} already exists (use --force to overwrite)",
                file_path.display()
            ),
        });
    }

    let installed = installer.list_installed()?;
    let mut content = String::new();
    for keg in &installed {
        content.push_str(&format!("brew \"{}\"\n", keg.name));
    }

    std::fs::write(file_path, content).map_err(|e| nb_core::Error::FileError {
        message: format!("failed to write {}: {}", file_path.display(), e),
    })?;

    println!(
        "{} Dumped {} packages to {}",
        style("==>").cyan().bold(),
        style(installed.len()).green().bold(),
        file_path.display()
    );

    Ok(())
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
