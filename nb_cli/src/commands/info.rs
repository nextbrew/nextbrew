use std::path::Path;

use chrono::{DateTime, Local};
use console::style;
use serde_json::Value;

use crate::caveats::{Places, cask_caveats, formula_caveats};

/// `nb info`: a formula as `brew info` shows it, or a cask as `brew info
/// --cask` does. `cask` (`--cask`) asks for the cask, `formula`
/// (`--formula`) for the formula; otherwise the formula is shown, unless
/// only a cask of that name is installed or there is no such formula.
pub async fn execute(
    installer: &mut nb_io::Installer,
    name: String,
    formula: bool,
    cask: bool,
) -> Result<(), nb_core::Error> {
    let token = name.strip_prefix("cask:").map(str::to_string);
    let cask = cask || token.is_some();
    let name = token.unwrap_or(name);
    if !cask {
        let only_cask_installed = !formula
            && installer.get_installed_by_full_name(&name).is_none()
            && installer
                .get_installed_by_full_name(&format!("cask:{name}"))
                .is_some();
        if !only_cask_installed {
            match installer.formula_json(&name).await {
                Ok(json) => {
                    print!("{}", formula_info(installer, &json).await);
                    return Ok(());
                }
                Err(nb_core::Error::MissingFormula { .. }) => {
                    // Installed, but gone from the API: what nextbrew knows.
                    if let Some(keg) = installer.get_installed_by_full_name(&name) {
                        print_field("Name:", style(&keg.name).bold());
                        print_field("Version:", &keg.version);
                        print_field("Installed:", format_timestamp(keg.installed_at));
                        return Ok(());
                    }
                    if formula {
                        missing(installer, &name, true).await;
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
    match installer.cask(&name).await {
        Ok(json) => {
            print!("{}", cask_info(installer, &json).await);
            Ok(())
        }
        Err(nb_core::Error::MissingFormula { .. }) if !cask => {
            missing(installer, &name, false).await
        }
        Err(e) => Err(e),
    }
}

/// `ofail` with `FormulaUnavailableError`, then `MissingFormula.reason`:
/// the cask of that name when only formulae were asked for. All on
/// standard error, status 1.
async fn missing(installer: &nb_io::Installer, name: &str, formula_only: bool) -> ! {
    let names = installer.formula_names().await.unwrap_or_default();
    let suggestions = nb_io::network::suggest::did_you_mean(name, &names);
    let mut message = format!("No available formula with the name \"{name}\".");
    if !suggestions.is_empty() {
        message += &format!(" Did you mean {}?", to_sentence(&suggestions));
    }
    eprintln!("{} {message}", style("Error:").red().bold());
    if formula_only && let Ok(json) = installer.cask(name).await {
        eprint!(
            "Found a cask named \"{name}\" instead.\n\n{}\n",
            cask_info(installer, &json).await
        );
    }
    std::process::exit(1);
}

/// `Utils::Text.to_sentence` with `or`: `a`, `a or b`, `a, b or c`.
fn to_sentence(words: &[String]) -> String {
    match words {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// What `brew info` says of an installed formula.
struct InstalledFormula {
    on_request: bool,
    /// Poured from a bottle, as opposed to built from source.
    poured: bool,
    pinned: bool,
    version: String,
}

/// A keg in `==> Installed Versions`.
struct KegRow {
    full_name: String,
    version: String,
    /// The newer version, when the formula is outdated.
    latest: Option<String>,
    files: usize,
    size: u64,
    linked: bool,
}

struct FormulaFacts {
    /// The formula API's JSON for this system.
    json: Value,
    full_name: String,
    /// A bottle for this system, so it pours rather than builds.
    bottled: bool,
    keg_only: bool,
    /// Marks and metadata lines are only for a terminal.
    tty: bool,
    installed: Option<InstalledFormula>,
    /// The installed version, when a newer one is out.
    outdated_from: Option<String>,
    deprecation: Option<String>,
    conflicts: Vec<String>,
    from: String,
    /// The third-party tap it is from.
    tap: Option<String>,
    rows: Vec<KegRow>,
    /// Each kind (`Build`, `Required`, …) with each dependency and whether
    /// it is installed.
    dependencies: Vec<(&'static str, Vec<(String, bool)>)>,
    /// Of the installed keg's runtime dependencies: installed and all.
    recursive: Option<(usize, usize)>,
    dependents: Option<usize>,
    /// Each kind (`Build`, `Required`) with each requirement and whether
    /// it is met.
    requirements: Vec<(&'static str, Vec<(String, bool)>)>,
    installs_from_source: bool,
    caveats: String,
}

/// `Homebrew::Cmd::Info#info_formula`.
async fn formula_info(installer: &nb_io::Installer, json: &Value) -> String {
    use std::io::IsTerminal;

    let json = nb_io::installer::for_current_platform(json);
    let name = json["name"].as_str().unwrap_or_default().to_string();
    let tap = json["tap"]
        .as_str()
        .filter(|tap| *tap != "homebrew/core")
        .map(str::to_string);
    let full_name = match &tap {
        Some(tap) => format!("{tap}/{name}"),
        None => name.clone(),
    };
    let formula: Option<nb_core::Formula> = serde_json::from_value(json.clone()).ok();
    let bottled = formula
        .as_ref()
        .is_some_and(|formula| nb_core::formula::select_bottle(formula).is_ok());
    let keg_only = formula.as_ref().is_some_and(nb_core::Formula::is_keg_only);
    let keg = installer.get_installed(&name).filter(|keg| keg.tap == tap);
    let outdated_from = match &keg {
        Some(_) => installer
            .is_outdated(&name)
            .await
            .ok()
            .flatten()
            .map(|outdated| outdated.installed_version),
        None => None,
    };
    let installed = keg.as_ref().map(|keg| InstalledFormula {
        on_request: keg.on_request,
        poured: !keg.store_key.starts_with("source:"),
        pinned: keg.pinned,
        version: keg.version.clone(),
    });
    let places = Places::new(installer, None);
    let tty = std::io::stdout().is_terminal();
    let is_installed = |name: &str| installer.is_installed(name.rsplit('/').next().unwrap_or(name));

    let mut dependencies = Vec::new();
    for (kind, deps) in [
        ("Build", strings(&json["build_dependencies"])),
        ("Required", required_dependencies(&json)),
        ("Recommended", strings(&json["recommended_dependencies"])),
        ("Optional", strings(&json["optional_dependencies"])),
    ] {
        if !deps.is_empty() {
            let deps = deps
                .into_iter()
                .map(|dep| {
                    let installed = is_installed(&dep);
                    (dep, installed)
                })
                .collect();
            dependencies.push((kind, deps));
        }
    }
    let recursive = match &keg {
        Some(_) => installer.dependencies(&name, false).await.ok().map(|deps| {
            let installed = deps.iter().filter(|dep| is_installed(dep)).count();
            (installed, deps.len())
        }),
        None => None,
    };
    let dependents = if tty && keg.is_some() {
        let mut count = 0;
        for other in installer.list_installed().unwrap_or_default() {
            if !other.name.starts_with("cask:")
                && installer
                    .dependencies(&other.name, false)
                    .await
                    .is_ok_and(|deps| deps.contains(&name))
            {
                count += 1;
            }
        }
        Some(count)
    } else {
        None
    };

    // Homebrew reads no requirements from the API; a tap's file has them.
    let system = current_system();
    let mut requirements: Vec<(&'static str, Vec<(String, bool)>)> = Vec::new();
    if tap.is_some() {
        for requirement in json["requirements"].as_array().into_iter().flatten() {
            let contexts: Vec<&str> = requirement["contexts"]
                .as_array()
                .map(|c| c.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let kind = if contexts.contains(&"build") {
                "Build"
            } else {
                "Required"
            };
            let shown = (
                nb_core::formula::requirements::display(requirement),
                nb_core::formula::requirements::satisfied(requirement, &system),
            );
            match requirements.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, list)) => list.push(shown),
                None => requirements.push((kind, vec![shown])),
            }
        }
        requirements.sort_by_key(|(kind, _)| *kind != "Build");
    }
    // `formula_installs_from_source?`.
    let for_other_os = json["requirements"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|req| matches!(req["name"].as_str(), Some("linux" | "macos")) && tap.is_some());
    let has_stable = json["versions"]["stable"].as_str().is_some();
    let installs_from_source =
        !for_other_os && ((!has_stable && has_head(&json)) || (has_stable && !bottled));

    let mut rows = Vec::new();
    let base = name.split('@').next().unwrap_or(&name).to_string();
    for other in installer.list_installed().unwrap_or_default() {
        let related =
            other.name == name || other.name == base || other.name.starts_with(&format!("{base}@"));
        if other.name.starts_with("cask:") || !related {
            continue;
        }
        let path = installer.keg_path(&other.name, &other.version);
        let latest = installer
            .is_outdated(&other.name)
            .await
            .ok()
            .flatten()
            .map(|outdated| outdated.current_version);
        rows.push(KegRow {
            full_name: installer.installed_full_name(&other.name),
            version: other.version.clone(),
            latest,
            files: file_count(&path),
            size: disk_usage(&path),
            linked: installer.is_linked(&other.name).unwrap_or(false),
        });
    }
    rows.sort_by(|a, b| compare_versions(&b.version, &a.version));

    let mut conflicts = Vec::new();
    let reasons = json["conflicts_with_reasons"].as_array();
    for (i, conflict) in strings(&json["conflicts_with"]).into_iter().enumerate() {
        let resolved = match installer.formula(&conflict).await {
            Ok(other) => match other.third_party_tap() {
                Some(tap) => format!("{tap}/{}", other.name),
                None => other.name,
            },
            Err(_) => conflict,
        };
        if resolved == full_name {
            continue;
        }
        match reasons.and_then(|r| r.get(i)).and_then(Value::as_str) {
            Some(reason) => conflicts.push(format!("{resolved} (because {reason})")),
            None => conflicts.push(resolved),
        }
    }
    conflicts.sort();

    let caveats =
        formula_caveats(installer, &json, &name, &full_name, keg_only, &places, true).await;
    let facts = FormulaFacts {
        deprecation: deprecation(&json),
        from: formula_from_url(installer, &json, tap.as_deref()),
        full_name,
        bottled,
        keg_only,
        tty,
        installed,
        outdated_from,
        conflicts,
        tap,
        rows,
        dependencies,
        recursive,
        dependents,
        requirements,
        installs_from_source,
        caveats,
        json,
    };
    render_formula(&facts)
}

fn render_formula(facts: &FormulaFacts) -> String {
    let json = &facts.json;
    let text = |key: &str| json[key].as_str().filter(|s| !s.is_empty());
    let tty = facts.tty;
    let installed = facts.installed.is_some();
    let outdated = facts.outdated_from.is_some();

    let mut specs = Vec::new();
    if let Some(stable) = json["versions"]["stable"].as_str() {
        let version = match json["revision"].as_u64() {
            Some(revision) if outdated && revision > 0 => format!("{stable}_{revision}"),
            _ => stable.to_string(),
        };
        let mut spec = format!("stable {version}");
        if facts.bottled {
            spec += " (bottled)";
        }
        specs.push(spec);
    }
    if has_head(json) {
        specs.push("HEAD".to_string());
    }
    if let Some(from) = &facts.outdated_from
        && let Some(first) = specs.first_mut()
    {
        *first = format!("{from} → {first}");
    }
    let lifecycle = if json["disabled"].as_bool() == Some(true) {
        Some(" (disabled)")
    } else if json["deprecated"].as_bool() == Some(true) {
        Some(" (deprecated)")
    } else {
        None
    };
    let mut name_status = mark(&facts.full_name, tty, installed, outdated, false);
    if tty
        && !installed
        && let Some(lifecycle) = lifecycle
    {
        name_status = format!(
            "{} {}",
            style(&facts.full_name).bold(),
            style(&lifecycle[1..]).yellow()
        );
    }
    let mut out = format!(
        "{} {}: {}",
        style("==>").green(),
        style(name_status).bold(),
        specs.join(", ")
    );
    if facts.keg_only {
        out += " [keg-only]";
    }
    out += "\n";
    for key in ["desc", "homepage"] {
        if let Some(value) = text(key) {
            out += &format!("{value}\n");
        }
    }
    for (label, key) in [("Aliases", "aliases"), ("Old Names", "oldnames")] {
        let names = strings(&json[key]);
        if !names.is_empty() {
            out += &format!("{label}: {}\n", names.join(", "));
        }
    }
    if let Some(message) = &facts.deprecation {
        out += &format!("{message}\n");
    }
    if !facts.conflicts.is_empty() {
        out += &format!("Conflicts with:\n  {}\n", facts.conflicts.join("\n  "));
    }
    match &facts.installed {
        None => out += "Not installed\n",
        Some(installed) if installed.on_request => out += "Installed (on request)\n",
        Some(_) => out += "Installed (as dependency)\n",
    }
    out += &format!("From: {}\n", facts.from);
    if let Some(tap) = &facts.tap
        && !tap.starts_with("homebrew/")
    {
        out += &format!("Tap: {tap}\n");
    }
    if let Some(license) = text("license") {
        out += &format!("License: {license}\n");
    }
    if tty {
        if let Some(installed) = facts.installed.as_ref().filter(|i| i.pinned) {
            out += &format!("Pinned: {}\n", installed.version);
        }
        if !installed && facts.installs_from_source {
            out += "Installs from source: yes\n";
        }
    }
    let rows = installed_lines(&facts.rows, tty);
    if !rows.is_empty() {
        out += &format!("{}\n{}\n", ohai("Installed Versions"), rows.join("\n"));
    }

    // `==> Dependencies`: build ones only when it was or would be built.
    let built = facts.installed.as_ref().is_some_and(|i| !i.poured);
    let mut lines = Vec::new();
    for (kind, deps) in &facts.dependencies {
        let would_build = facts.installs_from_source && !installed;
        if *kind == "Build" && !built && !would_build {
            continue;
        }
        let optional = matches!(*kind, "Recommended" | "Optional");
        let shown: Vec<String> = deps
            .iter()
            .map(|(dep, dep_installed)| {
                mark(dep, tty, *dep_installed, false, installed && !optional)
            })
            .collect();
        lines.push(format!("{kind} ({}): {}", deps.len(), shown.join(", ")));
    }
    let recursive = facts.recursive.filter(|(_, all)| *all > 0);
    let dependents = facts.dependents.filter(|count| *count > 0);
    if !lines.is_empty() || recursive.is_some() || dependents.is_some() {
        out += &format!("{}\n", ohai("Dependencies"));
        for line in lines {
            out += &format!("{line}\n");
        }
        if let Some((installed, all)) = recursive {
            let missing = all - installed;
            let status = if missing == 0 {
                format!("all installed {}", style("✔").green())
            } else {
                format!(
                    "{installed} installed {}, {missing} missing {}",
                    style("✔").green(),
                    style("✘").red()
                )
            };
            out += &format!("Recursive Runtime ({all}): {status}\n");
        }
        if let Some(count) = dependents {
            out += &format!("Dependents: {count}\n");
        }
    }
    if !facts.requirements.is_empty() {
        out += &format!("{}\n", ohai("Requirements"));
        for (kind, requirements) in &facts.requirements {
            let shown: Vec<String> = requirements
                .iter()
                .map(|(requirement, met)| mark(requirement, tty, *met, false, true))
                .collect();
            out += &format!("{kind}: {}\n", shown.join(", "));
        }
    }

    // `Options.dump_for_formula`.
    let mut options: Vec<(String, String)> = json["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| {
            Some((
                option["option"].as_str()?.to_string(),
                option["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ))
        })
        .collect();
    options.sort();
    if !options.is_empty() || has_head(json) {
        out += &format!("{}\n", ohai("Options"));
        for (flag, description) in options {
            out += &format!("{flag}\n\t{description}\n");
        }
        if has_head(json) {
            out += "--HEAD\n\tInstall HEAD version\n";
        }
    }
    if !facts.caveats.is_empty() {
        out += &format!("{}\n{}", ohai("Caveats"), facts.caveats);
        if !facts.caveats.ends_with('\n') {
            out += "\n";
        }
    }
    out
}

/// `pretty_install_status` as `brew info` uses it: plain unless on a
/// terminal, where it is bold and marked installed (✔), upgradable (↑)
/// or, when that matters, missing (✘).
fn mark(name: &str, tty: bool, installed: bool, outdated: bool, mark_missing: bool) -> String {
    if !tty {
        return name.to_string();
    }
    let name = style(name).bold();
    if installed && outdated {
        format!("{name} {}", style("↑").green())
    } else if installed {
        format!("{name} {}", style("✔").green())
    } else if mark_missing {
        format!("{name} {}", style("✘").red())
    } else {
        name.to_string()
    }
}

/// `installed_section_lines`: each related formula's keg in a column, the
/// newest first, with its size and whether it is linked.
fn installed_lines(rows: &[KegRow], tty: bool) -> Vec<String> {
    let rows: Vec<(String, String, String, bool)> = rows
        .iter()
        .map(|row| {
            let version = match &row.latest {
                Some(latest) if *latest != row.version => format!("{} → {latest}", row.version),
                _ => row.version.clone(),
            };
            (
                mark(&row.full_name, tty, true, row.latest.is_some(), false),
                version,
                format!("({})", abv(row.files, row.size)),
                row.linked,
            )
        })
        .collect();
    let width = |text: &str| console::measure_text_width(text);
    let name_width = rows.iter().map(|r| width(&r.0)).max().unwrap_or(0);
    let version_width = rows.iter().map(|r| width(&r.1)).max().unwrap_or(0);
    let size_width = rows.iter().map(|r| width(&r.2)).max().unwrap_or(0);
    rows.iter()
        .map(|(name, version, size, linked)| {
            let name = format!("{name}{}", " ".repeat(name_width - width(name)));
            let version = format!("{version}{}", " ".repeat(version_width - width(version)));
            if *linked {
                let size = format!("{size}{}", " ".repeat(size_width - width(size)));
                format!("{name} {version} {size} [Linked]")
            } else {
                format!("{name} {version} {size}")
            }
        })
        .collect()
}

/// `Pathname#abv`: the number of files (when more than one) and the size.
fn abv(files: usize, size: u64) -> String {
    if files > 1 {
        format!(
            "{} files, {}",
            number_readable(files),
            disk_usage_readable(size)
        )
    } else {
        disk_usage_readable(size)
    }
}

/// `Formatter.number_readable`: thousands separated by commas.
fn number_readable(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The files under `path` (`find -type f`), `.DS_Store` aside.
fn file_count(path: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let Ok(kind) = entry.file_type() else {
                return 0;
            };
            if kind.is_dir() {
                file_count(&entry.path())
            } else {
                usize::from(kind.is_file() && entry.file_name() != ".DS_Store")
            }
        })
        .sum()
}

/// Versions compared part by part, numbers as numbers.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<String> {
        v.split(|c: char| !c.is_ascii_alphanumeric())
            .map(str::to_string)
            .collect()
    };
    let (a, b) = (parts(a), parts(b));
    for (x, y) in a.iter().zip(&b) {
        let order = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.cmp(y),
        };
        if order.is_ne() {
            return order;
        }
    }
    a.len().cmp(&b.len())
}

/// The strings of a JSON array, each once.
fn strings(value: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in value.as_array().into_iter().flatten() {
        if let Some(item) = item.as_str()
            && !out.iter().any(|seen| seen == item)
        {
            out.push(item.to_string());
        }
    }
    out
}

/// `deps.required`: the dependencies, and on a macOS older than its bound
/// each `uses_from_macos` one the system does not provide yet.
fn required_dependencies(json: &Value) -> Vec<String> {
    let mut deps = strings(&json["dependencies"]);
    let current = nb_core::formula::macos_major_version();
    let bounds = json["uses_from_macos_bounds"].as_array();
    for (i, dep) in json["uses_from_macos"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(name) = dep.as_str() else {
            continue;
        };
        let since = bounds
            .and_then(|b| b.get(i))
            .and_then(|b| b["since"].as_str())
            .and_then(macos_major_of);
        if let (Some(current), Some(since)) = (current, since)
            && current < since
            && !deps.iter().any(|d| d == name)
        {
            deps.push(name.to_string());
        }
    }
    deps
}

/// The major version of a macOS codename (`catalina` as 10).
fn macos_major_of(codename: &str) -> Option<u32> {
    Some(match codename {
        "golden_gate" => 27,
        "tahoe" => 26,
        "sequoia" => 15,
        "sonoma" => 14,
        "ventura" => 13,
        "monterey" => 12,
        "big_sur" => 11,
        "catalina" | "mojave" | "high_sierra" | "sierra" | "el_capitan" => 10,
        _ => return None,
    })
}

fn has_head(json: &Value) -> bool {
    !json["urls"]["head"].is_null() || !json["versions"]["head"].is_null()
}

/// What requirements are checked against here.
fn current_system() -> nb_core::formula::requirements::System {
    nb_core::formula::requirements::System {
        macos: nb_core::formula::macos_major_version().map(|major| (major, 0)),
        arm: cfg!(target_arch = "aarch64"),
        xcode: None,
    }
}

/// `github_info` for a formula: its file on GitHub, by the tap's remote
/// for a third-party tap.
fn formula_from_url(installer: &nb_io::Installer, json: &Value, tap: Option<&str>) -> String {
    let name = json["name"].as_str().unwrap_or_default();
    let Some(tap) = tap else {
        let path = json["ruby_source_path"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| {
                let first = name.chars().next().unwrap_or('_');
                format!("Formula/{first}/{name}.rb")
            });
        return format!("https://github.com/Homebrew/homebrew-core/blob/HEAD/{path}");
    };
    let found = (|| {
        let tap = nb_io::tap::TapName::parse(tap).ok()?;
        let source = installer.tap_source().ok()?;
        let (_, file) = source.taps.find_formula(&tap, name)?;
        let relative = file
            .strip_prefix(source.taps.path(&tap))
            .ok()?
            .display()
            .to_string();
        let remote = source
            .taps
            .remote(&tap)
            .filter(|remote| !remote.is_empty())
            .unwrap_or_else(|| tap.default_remote());
        Some(github_remote_path(&remote, &relative))
    })();
    found.unwrap_or_default()
}

/// `github_remote_path`: the file's page on GitHub, or the remote and the
/// path elsewhere.
fn github_remote_path(remote: &str, path: &str) -> String {
    let github = ["https://", "http://", "git@", "git://"]
        .iter()
        .find_map(|scheme| remote.strip_prefix(scheme))
        .and_then(|rest| {
            rest.strip_prefix("github.com/")
                .or_else(|| rest.strip_prefix("github.com:"))
        });
    if let Some(repo) = github {
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if let Some((user, name)) = repo.split_once('/')
            && !name.is_empty()
        {
            return format!("https://github.com/{user}/{name}/blob/HEAD/{path}");
        }
    }
    format!("{remote}/{path}")
}

/// `Cask::Info.get_info`.
async fn cask_info(installer: &nb_io::Installer, json: &Value) -> String {
    let json = nb_io::installer::for_current_platform(json);
    let token = json["token"].as_str().unwrap_or_default().to_string();
    let keg = installer.get_installed(&format!("cask:{token}"));
    let outdated_from = match &keg {
        Some(_) => installer
            .is_outdated_greedy(&format!("cask:{token}"), false)
            .await
            .ok()
            .flatten()
            .map(|outdated| outdated.installed_version),
        None => None,
    };
    let installed = keg.map(|keg| {
        let path = installer.keg_path(&format!("cask:{token}"), &keg.version);
        let size = path.exists().then(|| disk_usage(&path));
        InstalledCask {
            on_request: keg.on_request,
            path: path.display().to_string(),
            size,
            installed_at: keg.installed_at,
            from_api: keg.tap.is_none(),
        }
    });
    let config = installed
        .as_ref()
        .map(|installed| nb_io::CaskConfig::load(Path::new(&installed.path)))
        .unwrap_or_default();
    let facts = CaskFacts {
        deprecation: deprecation(&json),
        from: from_url(installer, &json),
        dependencies: dependencies(installer, &json).await,
        installed,
        outdated_from,
        places: Places::new(installer, config.appdir()),
        json,
    };
    render(&facts)
}

struct InstalledCask {
    on_request: bool,
    path: String,
    /// `None` when the path is gone.
    size: Option<u64>,
    installed_at: i64,
    /// From homebrew/cask's API, as opposed to a tap's file.
    from_api: bool,
}

/// `Required (n): …` and the recursive count, installed and all.
struct Dependencies {
    required: Vec<String>,
    recursive_installed: usize,
    recursive: usize,
}

struct CaskFacts {
    /// The cask API's JSON for this system.
    json: Value,
    installed: Option<InstalledCask>,
    /// The installed version, when a newer one is out.
    outdated_from: Option<String>,
    deprecation: Option<String>,
    from: Option<String>,
    dependencies: Option<Dependencies>,
    places: Places,
}

fn render(facts: &CaskFacts) -> String {
    let json = &facts.json;
    let text = |key: &str| json[key].as_str().filter(|s| !s.is_empty());
    let token = text("token").unwrap_or_default();

    let mut title = format!("{} {}", style("==>").green(), style(token).bold());
    let names: Vec<&str> = json["name"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !names.is_empty() {
        title += &format!(" ({})", names.join(", "));
    }
    let version = text("version").unwrap_or_default();
    match &facts.outdated_from {
        Some(installed) => title += &format!(": {installed} → {version}"),
        None => title += &format!(": {version}"),
    }
    if json["auto_updates"].as_bool() == Some(true) {
        title += " (auto_updates)";
    }

    let mut out = format!("{title}\n");
    for key in ["desc", "homepage"] {
        if let Some(value) = text(key) {
            out += &format!("{value}\n");
        }
    }
    if let Some(message) = &facts.deprecation {
        out += &format!("{message}\n");
    }
    out += &installation_info(facts.installed.as_ref());
    out += "\n";
    if let Some(from) = &facts.from {
        out += &format!("From: {from}\n");
    }
    if let Some(deps) = &facts.dependencies {
        out += &format!(
            "{}\nRequired ({}): {}\n",
            ohai("Dependencies"),
            deps.required.len(),
            deps.required.join(", ")
        );
        if deps.recursive > 0 {
            let missing = deps.recursive - deps.recursive_installed;
            let status = if missing == 0 {
                "all installed ✔".to_string()
            } else {
                format!(
                    "{} installed ✔, {missing} missing ✘",
                    deps.recursive_installed
                )
            };
            out += &format!("Recursive Runtime ({}): {status}\n", deps.recursive);
        }
    }
    let requirements = requirements(&json["depends_on"]);
    if !requirements.is_empty() {
        out += &format!(
            "{}\nRequired: {}\n",
            ohai("Requirements"),
            requirements.join(", ")
        );
    }
    let languages: Vec<&str> = json["languages"]
        .as_array()
        .map(|languages| languages.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !languages.is_empty() {
        out += &format!("{}\n{}\n", ohai("Languages"), languages.join(", "));
    }
    out += &ohai("Artifacts");
    let mut artifacts: Vec<Value> = json["artifacts"].as_array().cloned().unwrap_or_default();
    // Homebrew sorts the API's list as it lists it; a tap's file was
    // sorted so once already as it was read.
    if json["tap"] == "homebrew/cask" {
        nb_io::tap::ruby::sort_artifacts(&mut artifacts);
    }
    for artifact in &artifacts {
        if let Some(line) = artifact_line(artifact, token, &facts.places) {
            out += &format!("\n{line}");
        }
    }
    out += "\n";
    if let Some(caveats) = cask_caveats(json, token, &facts.places) {
        out += &format!("{}\n{caveats}\n", ohai("Caveats"));
    }
    out
}

fn ohai(title: &str) -> String {
    format!("{} {}", style("==>").blue().bold(), style(title).bold())
}

/// `Cask::Info.installation_info`, with the receipt's line.
fn installation_info(installed: Option<&InstalledCask>) -> String {
    let Some(installed) = installed else {
        return "Not installed".to_string();
    };
    let status = if installed.on_request {
        "Installed (on request)"
    } else {
        "Installed (as dependency)"
    };
    let Some(size) = installed.size else {
        return format!(
            "{status}\n{} ({})\n",
            installed.path,
            style("does not exist").red()
        );
    };
    let mut receipt = "Installed".to_string();
    if installed.from_api {
        receipt += " using the formulae.brew.sh API";
    }
    if let Some(time) = DateTime::from_timestamp(installed.installed_at, 0) {
        receipt += &time
            .with_timezone(&Local)
            .format(" on %Y-%m-%d at %H:%M:%S")
            .to_string();
    }
    format!(
        "{status}\n{} ({})\n  {receipt}",
        installed.path,
        disk_usage_readable(size)
    )
}

/// `DeprecateDisable.message`, without the token (or formula name),
/// capitalised.
fn deprecation(json: &Value) -> Option<String> {
    let token = json["token"]
        .as_str()
        .or(json["name"].as_str())
        .unwrap_or_default();
    let message = match nb_io::installer::deprecate_disable(json, Local::now().date_naive())? {
        nb_io::installer::DeprecateDisable::Deprecated(message)
        | nb_io::installer::DeprecateDisable::Disabled(message) => message,
    };
    let message = message
        .strip_prefix(&format!("{token} has been "))
        .unwrap_or(&message)
        .trim_end()
        .to_string();
    let mut chars = message.chars();
    Some(
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default(),
    )
}

/// `Cask::Info.repo_info`: the cask's file on GitHub, or the tap's own
/// remote when it has one.
fn from_url(installer: &nb_io::Installer, json: &Value) -> Option<String> {
    let tap = json["tap"].as_str()?;
    if tap == "homebrew/cask" {
        let path = json["ruby_source_path"].as_str()?;
        return Some(format!(
            "https://github.com/Homebrew/homebrew-cask/blob/HEAD/{path}"
        ));
    }
    let tap = nb_io::tap::TapName::parse(tap).ok()?;
    let source = installer.tap_source().ok()?;
    if let Some(remote) = source.taps.remote(&tap)
        && tap.uses_custom_remote(Some(&remote))
    {
        return Some(remote);
    }
    let (_, file) = source.taps.find_cask(&tap, json["token"].as_str()?)?;
    let relative = file.strip_prefix(source.taps.path(&tap)).ok()?;
    Some(format!(
        "{}/blob/HEAD/{}",
        tap.default_remote(),
        relative.display()
    ))
}

/// The names in a `depends_on` entry, one or many.
fn names(value: &Value) -> Vec<String> {
    match value {
        Value::String(name) => vec![name.clone()],
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// `Cask::Info.deps_info`: the formulae and casks it depends on, and how
/// many of everything they need in turn are installed.
async fn dependencies(installer: &nb_io::Installer, json: &Value) -> Option<Dependencies> {
    let formulae = names(&json["depends_on"]["formula"]);
    let casks = names(&json["depends_on"]["cask"]);
    if formulae.is_empty() && casks.is_empty() {
        return None;
    }
    let short = |name: &str| name.rsplit('/').next().unwrap_or(name).to_string();
    let mut required: Vec<String> = formulae.clone();
    required.extend(casks.iter().map(|token| format!("{token} (cask)")));

    // `collect_cask_dependency_names`: the formulae with what installed
    // ones need, and the casks with theirs.
    let mut all_formulae: Vec<String> = Vec::new();
    for name in &formulae {
        all_formulae.push(name.clone());
        if installer.is_installed(&short(name))
            && let Ok(deps) = installer.dependencies(&short(name), false).await
        {
            all_formulae.extend(deps);
        }
    }
    let mut all_casks: Vec<String> = Vec::new();
    let mut pending = casks;
    while let Some(token) = pending.pop() {
        if all_casks.contains(&token) || Some(token.as_str()) == json["token"].as_str() {
            continue;
        }
        if let Ok(dep) = installer.cask(&token).await {
            let dep = nb_io::installer::for_current_platform(&dep);
            all_formulae.extend(names(&dep["depends_on"]["formula"]));
            pending.extend(names(&dep["depends_on"]["cask"]));
        }
        all_casks.push(token);
    }
    all_formulae.sort();
    all_formulae.dedup();
    let installed = all_formulae
        .iter()
        .filter(|name| installer.is_installed(&short(name)))
        .count()
        + all_casks
            .iter()
            .filter(|token| installer.is_installed(&format!("cask:{}", short(token))))
            .count();
    Some(Dependencies {
        required,
        recursive_installed: installed,
        recursive: all_formulae.len() + all_casks.len(),
    })
}

/// `CaskDependent#requirements` as `display_s` shows them: architecture,
/// then macOS.
fn requirements(depends_on: &Value) -> Vec<String> {
    let mut requirements = Vec::new();
    for arch in depends_on["arch"].as_array().into_iter().flatten() {
        let kind = arch["type"].as_str().unwrap_or_default();
        let name = match (kind, arch["bits"].as_i64()) {
            ("intel", Some(64)) => "x86_64".to_string(),
            (kind, Some(64)) => format!("{kind}64"),
            ("intel", Some(32)) => "i386".to_string(),
            (kind, _) => kind.to_string(),
        };
        requirements.push(format!("{name} architecture"));
    }
    for key in ["macos", "maximum_macos"] {
        let Some(macos) = depends_on[key].as_object() else {
            continue;
        };
        match macos.iter().next() {
            Some((comparator, versions)) => {
                let versions: Vec<String> = match versions {
                    Value::Array(versions) => versions
                        .iter()
                        .map(|v| v.as_str().map_or(v.to_string(), str::to_string))
                        .collect(),
                    other => vec![other.as_str().map_or(other.to_string(), str::to_string)],
                };
                requirements.push(format!("macOS {comparator} {}", versions.join(" / ")));
            }
            None => requirements.push("macOS".to_string()),
        }
    }
    requirements
}

/// An ordinary artifact as `AbstractArtifact#to_s` shows it:
/// `<summary> (<English name>)`. Uninstall, zap and flight steps have none.
fn artifact_line(artifact: &Value, token: &str, places: &Places) -> Option<String> {
    let (kind, args) = artifact
        .as_object()?
        .iter()
        .find(|(key, _)| key.as_str() != "target")?;
    let english = english_name(kind)?;
    let args: Vec<Value> = match args {
        Value::Array(args) => args.clone(),
        other => vec![other.clone()],
    };
    let first = args.first().and_then(Value::as_str).unwrap_or_default();
    let summary = match kind.as_str() {
        "command_wrapper" => format!(".homebrew-command-wrappers/{first} -> {first}"),
        "stage_only" => "true".to_string(),
        "installer" => {
            let options = args.first().cloned().unwrap_or_default();
            options["manual"]
                .as_str()
                .or_else(|| options["script"]["executable"].as_str())
                .or_else(|| options["script"].as_str())
                .map(|path| places.expand(path))
                .unwrap_or_default()
        }
        "generate_completions_from_executable" => {
            let commands: Vec<&str> = args.iter().filter_map(Value::as_str).collect();
            let options = args.iter().find(|arg| arg.is_object());
            let base_name = options
                .and_then(|o| o["base_name"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    Path::new(commands.first().copied().unwrap_or(token))
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                });
            let shells: Vec<&str> = options
                .and_then(|o| o["shells"].as_array())
                .map(|shells| shells.iter().filter_map(Value::as_str).collect())
                .unwrap_or_else(|| vec!["bash", "zsh", "fish"]);
            format!(
                "{} (base_name: {base_name}, shells: {})",
                commands.join(" "),
                shells.join(", ")
            )
        }
        "pkg" | "generated_script" => places.expand(first),
        // `Relocated#summarize`: the source, and the target if one is given.
        _ => {
            let target = args
                .iter()
                .find_map(|arg| arg["target"].as_str())
                .filter(|target| !target.is_empty());
            match target {
                Some(target) => format!("{} -> {}", places.expand(first), places.expand(target)),
                None => places.expand(first),
            }
        }
    };
    Some(format!("{summary} ({english})"))
}

/// `english_name` of each of `DSL::ORDINARY_ARTIFACT_CLASSES`, by DSL key.
fn english_name(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "installer" => "Installer",
        "app" => "App",
        "app_image" => "App Image",
        "artifact" => "Generic Artifact",
        "audio_unit_plugin" => "Audio Unit Plugin",
        "binary" => "Binary",
        "command_wrapper" => "Command Wrapper",
        "colorpicker" => "Colorpicker",
        "dictionary" => "Dictionary",
        "font" => "Font",
        "generated_script" => "Generated Script",
        "input_method" => "Input Method",
        "internet_plugin" => "Internet Plugin",
        "keyboard_layout" => "Keyboard Layout",
        "manpage" => "Manpage",
        "pkg" => "Pkg",
        "prefpane" => "Preference Pane",
        "qlplugin" => "Quick Look Plugin",
        "mdimporter" => "Spotlight metadata importer",
        "screen_saver" => "Screen Saver",
        "service" => "Service",
        "stage_only" => "Stage Only",
        "suite" => "App Suite",
        "vst_plugin" => "VST Plugin",
        "vst3_plugin" => "VST3 Plugin",
        "bash_completion" => "Bash Completion",
        "zsh_completion" => "Zsh Completion",
        "fish_completion" => "Fish Completion",
        "pwsh_completion" => "Pwsh Completion",
        "generate_completions_from_executable" => "Generated Completion",
        _ => return None,
    })
}

/// `Pathname#disk_usage`: every file and directory by its own size, each
/// hard-linked file once.
fn disk_usage(path: &Path) -> u64 {
    fn walk(path: &Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return 0;
        };
        if meta.is_dir() {
            let children: u64 = std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| walk(&entry.path(), seen))
                .sum();
            return meta.size() + children;
        }
        if seen.insert((meta.dev(), meta.ino())) {
            meta.size()
        } else {
            0
        }
    }
    let mut seen = std::collections::HashSet::new();
    // `children.sum(&:disk_usage)`: the directory itself is not counted.
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| walk(&entry.path(), &mut seen))
        .sum()
}

/// `Formatter.disk_usage_readable`: in thousands, one decimal unless whole.
fn disk_usage_readable(bytes: u64) -> String {
    let mut size = bytes as f64;
    let mut unit = "B";
    for next in ["KB", "MB", "GB"] {
        if (size * 10.0).round() / 10.0 < 1000.0 {
            break;
        }
        size /= 1000.0;
        unit = next;
    }
    let size = (size * 10.0).round() / 10.0;
    if size.fract() == 0.0 {
        format!("{}{unit}", size as u64)
    } else {
        format!("{size:.1}{unit}")
    }
}

fn print_field(label: &str, value: impl std::fmt::Display) {
    println!("{:<10}  {}", style(label).dim(), value);
}

fn format_timestamp(timestamp: i64) -> String {
    match DateTime::from_timestamp(timestamp, 0) {
        Some(dt) => {
            let local_dt = dt.with_timezone(&Local);
            let now = Local::now();
            let duration = now.signed_duration_since(local_dt);

            if duration.num_days() > 0 {
                format!(
                    "{} ({} days ago)",
                    local_dt.format("%Y-%m-%d"),
                    duration.num_days()
                )
            } else if duration.num_hours() > 0 {
                format!(
                    "{} ({} hours ago)",
                    local_dt.format("%Y-%m-%d %H:%M"),
                    duration.num_hours()
                )
            } else {
                format!(
                    "{} ({} minutes ago)",
                    local_dt.format("%H:%M"),
                    duration.num_minutes()
                )
            }
        }
        None => "invalid timestamp".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn places() -> Places {
        Places {
            prefix: "/p".into(),
            appdir: "/Applications".into(),
            home: "/Users/me".into(),
        }
    }

    fn formula_facts(json: Value) -> FormulaFacts {
        FormulaFacts {
            json,
            full_name: "jq".into(),
            bottled: true,
            keg_only: false,
            tty: false,
            installed: None,
            outdated_from: None,
            deprecation: None,
            conflicts: Vec::new(),
            from: "https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/j/jq.rb".into(),
            tap: None,
            rows: Vec::new(),
            dependencies: Vec::new(),
            recursive: None,
            dependents: None,
            requirements: Vec::new(),
            installs_from_source: false,
            caveats: String::new(),
        }
    }

    #[test]
    fn shows_a_formula_as_brew_info_does() {
        let mut facts = formula_facts(json!({
            "name": "jq",
            "desc": "Lightweight and flexible command-line JSON processor",
            "homepage": "https://jqlang.github.io/jq/",
            "license": "MIT",
            "versions": {"stable": "1.8.2", "head": "HEAD"},
            "urls": {"head": {"url": "https://github.com/jqlang/jq.git"}},
            "aliases": ["jq@1"],
        }));
        facts.dependencies = vec![("Required", vec![("oniguruma".into(), false)])];
        assert_eq!(
            console::strip_ansi_codes(&render_formula(&facts)),
            "==> jq: stable 1.8.2 (bottled), HEAD\n\
             Lightweight and flexible command-line JSON processor\n\
             https://jqlang.github.io/jq/\n\
             Aliases: jq@1\n\
             Not installed\n\
             From: https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/j/jq.rb\n\
             License: MIT\n\
             ==> Dependencies\n\
             Required (1): oniguruma\n\
             ==> Options\n\
             --HEAD\n\
             \tInstall HEAD version\n"
        );
    }

    #[test]
    fn shows_an_installed_outdated_keg_only_formula() {
        let mut facts = formula_facts(json!({
            "name": "openssl@3",
            "versions": {"stable": "3.5.9"},
            "revision": 1,
            "conflicts_with": ["x"],
        }));
        facts.full_name = "openssl@3".into();
        facts.keg_only = true;
        facts.installed = Some(InstalledFormula {
            on_request: false,
            poured: false,
            pinned: false,
            version: "3.5.8".into(),
        });
        facts.outdated_from = Some("3.5.8".into());
        facts.conflicts = vec!["x (because both install `x`)".into()];
        facts.rows = vec![
            KegRow {
                full_name: "openssl@3".into(),
                version: "3.5.8".into(),
                latest: Some("3.5.9_1".into()),
                files: 7596,
                size: 37_300_000,
                linked: false,
            },
            KegRow {
                full_name: "openssl@1.1".into(),
                version: "1.1.1w".into(),
                latest: None,
                files: 1,
                size: 1_000,
                linked: true,
            },
        ];
        facts.dependencies = vec![
            ("Build", vec![("perl".into(), false)]),
            ("Required", vec![("ca-certificates".into(), true)]),
        ];
        facts.recursive = Some((1, 1));
        facts.caveats = "openssl@3 is keg-only.\n".into();
        assert_eq!(
            console::strip_ansi_codes(&render_formula(&facts)),
            "==> openssl@3: 3.5.8 → stable 3.5.9_1 (bottled) [keg-only]\n\
             Conflicts with:\n  x (because both install `x`)\n\
             Installed (as dependency)\n\
             From: https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/j/jq.rb\n\
             ==> Installed Versions\n\
             openssl@3   3.5.8 → 3.5.9_1 (7,596 files, 37.3MB)\n\
             openssl@1.1 1.1.1w          (1KB)                 [Linked]\n\
             ==> Dependencies\n\
             Build (1): perl\n\
             Required (1): ca-certificates\n\
             Recursive Runtime (1): all installed ✔\n\
             ==> Caveats\n\
             openssl@3 is keg-only.\n"
        );
    }

    #[test]
    fn formula_helpers_follow_homebrew() {
        assert_eq!(to_sentence(&["a".into()]), "a");
        assert_eq!(
            to_sentence(&["a".into(), "b".into(), "c".into()]),
            "a, b or c"
        );
        assert_eq!(
            github_remote_path(
                "https://github.com/hashicorp/homebrew-tap.git",
                "Formula/x.rb"
            ),
            "https://github.com/hashicorp/homebrew-tap/blob/HEAD/Formula/x.rb"
        );
        assert_eq!(
            github_remote_path("git@github.com:me/homebrew-x", "a.rb"),
            "https://github.com/me/homebrew-x/blob/HEAD/a.rb"
        );
        assert_eq!(github_remote_path("/srv/tap", "a.rb"), "/srv/tap/a.rb");
        assert_eq!(number_readable(1234567), "1,234,567");
        assert_eq!(compare_versions("3.10", "3.9"), std::cmp::Ordering::Greater);
        assert_eq!(
            required_dependencies(&json!({
                "dependencies": ["a"],
                "uses_from_macos": ["b", "c"],
                "uses_from_macos_bounds": [{}, {"since": "golden_gate_plus"}],
            })),
            vec!["a".to_string()]
        );
    }

    fn facts(json: Value) -> CaskFacts {
        CaskFacts {
            json,
            installed: None,
            outdated_from: None,
            deprecation: None,
            from: None,
            dependencies: None,
            places: places(),
        }
    }

    #[test]
    fn shows_a_cask_as_brew_info_does() {
        let mut facts = facts(json!({
            "token": "alacritty",
            "tap": "homebrew/cask",
            "name": ["Alacritty"],
            "version": "0.17.0",
            "desc": "GPU-accelerated terminal emulator",
            "homepage": "https://github.com/alacritty/alacritty/",
            "depends_on": {"macos": {}},
            "artifacts": [
                {"app": ["Alacritty.app"], "target": "/Applications/Alacritty.app"},
                {"binary": ["$APPDIR/Alacritty.app/Contents/MacOS/alacritty"], "target": "$HOMEBREW_PREFIX/bin/alacritty"},
                {"binary": ["$APPDIR/Alacritty.app/Contents/Resources/61/alacritty", {"target": "~/.terminfo/61/alacritty"}], "target": "/$HOME/.terminfo/61/alacritty"},
                {"manpage": ["$APPDIR/Alacritty.app/Contents/Resources/alacritty.1.gz"], "target": "$HOMEBREW_PREFIX/share/man/man1/alacritty.1.gz"},
                {"zap": [{"trash": "~/x"}]},
            ],
        }));
        facts.from =
            Some("https://github.com/Homebrew/homebrew-cask/blob/HEAD/Casks/a/alacritty.rb".into());
        assert_eq!(
            console::strip_ansi_codes(&render(&facts)),
            "==> alacritty (Alacritty): 0.17.0\n\
             GPU-accelerated terminal emulator\n\
             https://github.com/alacritty/alacritty/\n\
             Not installed\n\
             From: https://github.com/Homebrew/homebrew-cask/blob/HEAD/Casks/a/alacritty.rb\n\
             ==> Requirements\n\
             Required: macOS\n\
             ==> Artifacts\n\
             Alacritty.app (App)\n\
             /Applications/Alacritty.app/Contents/MacOS/alacritty (Binary)\n\
             /Applications/Alacritty.app/Contents/Resources/61/alacritty -> ~/.terminfo/61/alacritty (Binary)\n\
             /Applications/Alacritty.app/Contents/Resources/alacritty.1.gz (Manpage)\n"
        );
    }

    #[test]
    fn shows_wrappers_completions_caveats_and_requirements() {
        let mut facts = facts(json!({
            "token": "op",
            "tap": "someone/tools",
            "name": [],
            "version": "2.41.0",
            "auto_updates": true,
            "depends_on": {"macos": {">=": ["14"]}, "arch": [{"type": "arm", "bits": 64}]},
            "languages": ["en", "ja"],
            "caveats": "Read $HOMEBREW_PREFIX/x.\n",
            "artifacts": [
                {"command_wrapper": ["keka", {"executable": "$APPDIR/Keka.app/Contents/MacOS/Keka"}], "target": "$HOMEBREW_PREFIX/bin/keka"},
                {"generate_completions_from_executable": ["op", "completion", {"base_name": null, "shells": ["bash", "zsh", "fish"]}]},
                {"pkg": ["Install X.pkg"]},
            ],
        }));
        facts.outdated_from = Some("2.40.0".into());
        facts.dependencies = Some(Dependencies {
            required: vec!["jq".into(), "other (cask)".into()],
            recursive_installed: 1,
            recursive: 3,
        });
        assert_eq!(
            console::strip_ansi_codes(&render(&facts)),
            "==> op: 2.40.0 → 2.41.0 (auto_updates)\n\
             Not installed\n\
             ==> Dependencies\n\
             Required (2): jq, other (cask)\n\
             Recursive Runtime (3): 1 installed ✔, 2 missing ✘\n\
             ==> Requirements\n\
             Required: arm64 architecture, macOS >= 14\n\
             ==> Languages\n\
             en, ja\n\
             ==> Artifacts\n\
             .homebrew-command-wrappers/keka -> keka (Command Wrapper)\n\
             op completion (base_name: op, shells: bash, zsh, fish) (Generated Completion)\n\
             Install X.pkg (Pkg)\n\
             ==> Caveats\n\
             Read /p/x.\n\n"
        );
    }

    #[test]
    fn says_where_and_how_big_an_installed_cask_is() {
        let installed = InstalledCask {
            on_request: true,
            path: "/p/Caskroom/x/1.0".into(),
            size: Some(1_234_567),
            installed_at: 0,
            from_api: false,
        };
        let info = installation_info(Some(&installed));
        assert!(
            info.starts_with("Installed (on request)\n/p/Caskroom/x/1.0 (1.2MB)\n  Installed on ")
        );
        assert_eq!(disk_usage_readable(999), "999B");
        assert_eq!(disk_usage_readable(1_000), "1KB");
        assert_eq!(disk_usage_readable(999_960), "1MB");
        assert_eq!(disk_usage_readable(2_500_000_000), "2.5GB");
    }
}
