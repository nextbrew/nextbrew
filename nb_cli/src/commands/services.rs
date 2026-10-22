//! `nb services`: `brew services` for launchd and systemd, with its messages. Jobs are
//! labelled `dev.nextbrew.<formula>`, so Homebrew's own services are never
//! touched.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use console::style;
use nb_io::ServiceFormula;
use nb_io::services::{JobStatus, LABEL_PREFIX, Launchd, label, launchctl, launchctl_status};
use serde::Serialize;

use crate::cli::ServicesCommands;
use crate::utils::normalize_formula_name;

const BIN: &str = "nb services";
/// `launchctl bootout`'s "no such process" and "not supported in domain".
const ESRCH: i32 = 3;
const DOMAIN_ACTION_NOT_SUPPORTED: i32 = 125;
const EINPROGRESS: i32 = 36;

fn ohai(message: impl AsRef<str>) {
    println!(
        "{} {}",
        style("==>").blue().bold(),
        style(message.as_ref()).bold()
    );
}

fn opoo(message: impl AsRef<str>) {
    eprintln!("{} {}", style("Warning:").yellow().bold(), message.as_ref());
}

fn usage(message: impl Into<String>) -> nb_core::Error {
    nb_core::Error::InvalidArgument {
        message: message.into(),
    }
}

/// A formula's service and its launchd job.
struct Service<'a> {
    formula: ServiceFormula,
    label: String,
    launchd: &'a Launchd,
}

impl Service<'_> {
    fn name(&self) -> &str {
        &self.formula.name
    }

    fn status(&self) -> JobStatus {
        self.launchd.find(&self.label)
    }

    fn file_in(&self, dir: &Path) -> PathBuf {
        let ext = if self.launchd.systemd {
            "service"
        } else {
            "plist"
        };
        dir.join(format!("{}.{ext}", self.label))
    }

    fn timer_dest(&self) -> PathBuf {
        self.dest().with_extension("timer")
    }

    fn dest(&self) -> PathBuf {
        self.file_in(&self.launchd.dest_dir())
    }

    fn file_present(&self) -> bool {
        self.file_in(&self.launchd.boot_path()).exists()
            || self.file_in(&self.launchd.user_path()).exists()
    }

    /// The registered file, or where it would be registered.
    fn registered_destination(&self) -> PathBuf {
        [
            self.dest(),
            self.file_in(&self.launchd.boot_path()),
            self.file_in(&self.launchd.user_path()),
        ]
        .into_iter()
        .find(|file| file.exists())
        .unwrap_or_else(|| self.dest())
    }

    fn owner(&self) -> Option<String> {
        let registered = self.registered_destination();
        if registered.exists() {
            if let Some(user) = nb_io::services::plist_user_name(&registered) {
                return Some(user);
            }
            let dir = registered.parent().map(Path::to_path_buf);
            if dir == Some(self.launchd.boot_path()) {
                return Some("root".to_string());
            }
            if dir == Some(self.launchd.user_path()) {
                return Some(current_user());
            }
        }
        if self.file_in(&self.launchd.boot_path()).exists() {
            return Some("root".to_string());
        }
        if self.file_in(&self.launchd.user_path()).exists() {
            return Some(current_user());
        }
        None
    }

    fn keep_alive(&self) -> bool {
        self.formula
            .spec
            .as_ref()
            .is_some_and(|spec| spec.keep_alive())
    }

    fn timed(&self) -> bool {
        self.formula.spec.as_ref().is_some_and(|spec| spec.timed())
    }

    fn requires_root(&self) -> bool {
        self.formula
            .spec
            .as_ref()
            .is_some_and(|spec| spec.requires_root())
    }

    /// The plist to register: generated from the `service` block, or the
    /// package's own file under nextbrew's label.
    fn contents(&self) -> Result<String, nb_core::Error> {
        if let Some(spec) = &self.formula.spec {
            return Ok(if self.launchd.systemd {
                spec.to_systemd_unit(self.name())
            } else {
                spec.to_plist(&self.label)
            });
        }
        self.formula
            .keg_file
            .as_ref()
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(|contents| {
                if self.launchd.systemd {
                    Some(contents)
                } else {
                    nb_io::services::relabel_plist(&contents, &self.label)
                }
            })
            .ok_or_else(|| {
                usage(format!(
                    "Formula `{}` has not implemented #plist, #service or provided a locatable service file.",
                    self.name()
                ))
            })
    }

    fn status_symbol(&self, status: &JobStatus) -> &'static str {
        if status.running() {
            "started"
        } else if !status.found {
            "none"
        } else if status.exit_code() == Some(0) {
            if self.timed() { "scheduled" } else { "stopped" }
        } else if status.exit_code().is_some() {
            "error"
        } else if status.output.is_empty() {
            "unknown"
        } else {
            "other"
        }
    }
}

fn current_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| {
            std::process::Command::new("/usr/bin/whoami")
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .unwrap_or_default()
        })
}

/// `System.user_exists?`.
fn user_exists(name: &str) -> bool {
    if name == current_user() {
        return true;
    }
    let Ok(name) = std::ffi::CString::new(name) else {
        return false;
    };
    // SAFETY: `name` is a valid C string; only the pointer's nullness is read.
    !unsafe { libc::getpwnam(name.as_ptr()) }.is_null()
}

fn home() -> Result<PathBuf, nb_core::Error> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| usage("HOME is not set"))
}

pub async fn execute(
    installer: &nb_io::Installer,
    command: Option<ServicesCommands>,
    verbose: bool,
) -> Result<(), nb_core::Error> {
    execute_as(installer, command, verbose, None).await
}

pub async fn execute_as(
    installer: &nb_io::Installer,
    command: Option<ServicesCommands>,
    verbose: bool,
    sudo_service_user: Option<String>,
) -> Result<(), nb_core::Error> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err(usage("`nb services` is supported only on macOS and Linux."));
    }
    let mut launchd = Launchd::current(home()?);
    if sudo_service_user.is_some() {
        if !launchd.root() {
            return Err(usage(format!(
                "`{BIN} --sudo-service-user` is supported only when running as root!"
            )));
        }
        if launchd.systemd {
            return Err(usage(format!(
                "`{BIN} --sudo-service-user` is currently supported only on macOS!"
            )));
        }
    }
    launchd.service_user = sudo_service_user;
    let launchd = launchd;
    let command = command.unwrap_or(ServicesCommands::List { json: false });
    let (subcommand, formulas, all) = match &command {
        ServicesCommands::List { .. } | ServicesCommands::Cleanup => ("list", Vec::new(), false),
        ServicesCommands::Info { formulas, all, .. } => ("info", formulas.clone(), *all),
        ServicesCommands::Run { formulas, all, .. } => ("run", formulas.clone(), *all),
        ServicesCommands::Start { formulas, all, .. } => ("start", formulas.clone(), *all),
        ServicesCommands::Stop { formulas, all, .. } => ("stop", formulas.clone(), *all),
        ServicesCommands::Kill { formulas, all } => ("kill", formulas.clone(), *all),
        ServicesCommands::Restart { formulas, all, .. } => ("restart", formulas.clone(), *all),
    };
    if !formulas.is_empty() && all {
        opoo("The `--all` argument overrides provided formula argument!");
    }

    let mut targets = Vec::new();
    if all {
        for formula in installer.service_formulae().await? {
            let service = wrap(formula, &launchd);
            let keep = match subcommand {
                "start" => {
                    !service.status().found
                        && (launchd.root() || service.owner().as_deref() != Some("root"))
                }
                "stop" => {
                    service.status().found
                        && (launchd.root() || service.owner().as_deref() != Some("root"))
                }
                _ => true,
            };
            if keep {
                targets.push(service);
            }
        }
        if targets.is_empty() {
            return Ok(());
        }
    } else {
        for formula in &formulas {
            let name = normalize_formula_name(formula)?;
            targets.push(wrap(installer.service_formula(&name).await?, &launchd));
        }
    }
    let check = |targets: &Vec<Service>| {
        if targets.is_empty() {
            Err(usage(
                "Formula(e) missing, please provide a formula name or use `--all`.",
            ))
        } else {
            Ok(())
        }
    };

    match command {
        ServicesCommands::List { json } => list(installer, &launchd, json).await,
        ServicesCommands::Cleanup => cleanup(installer, &launchd).await,
        ServicesCommands::Info { json, .. } => {
            check(&targets)?;
            info(&targets, json, verbose)
        }
        ServicesCommands::Run { file, .. } => {
            check(&targets)?;
            run(&targets, file.as_deref(), &launchd)
        }
        ServicesCommands::Start { file, .. } => {
            check(&targets)?;
            start(&targets, file.as_deref(), &launchd, verbose)
        }
        ServicesCommands::Stop {
            keep,
            no_wait,
            max_wait,
            ..
        } => {
            check(&targets)?;
            stop(&targets, &launchd, no_wait, max_wait.unwrap_or(60.0), keep)
        }
        ServicesCommands::Kill { .. } => {
            check(&targets)?;
            kill(&targets);
            Ok(())
        }
        ServicesCommands::Restart { file, .. } => {
            check(&targets)?;
            restart(&targets, file.as_deref(), &launchd, verbose)
        }
    }
}

fn wrap(formula: ServiceFormula, launchd: &Launchd) -> Service<'_> {
    Service {
        label: label(&formula.name),
        formula,
        launchd,
    }
}

/// `report_service_running_or_loaded?`: a running service is left alone.
fn already_running(service: &Service, status_word: &str) -> bool {
    if !service.status().running() {
        return false;
    }
    println!(
        "Service `{}` already {status_word}, use `{BIN} restart {}` to restart.",
        service.name(),
        service.name()
    );
    true
}

fn run(targets: &[Service], file: Option<&Path>, launchd: &Launchd) -> Result<(), nb_core::Error> {
    if let Some(file) = file
        && !file.exists()
    {
        return Err(usage("Provided service file does not exist."));
    }
    for service in targets {
        if already_running(service, "running") {
            continue;
        }
        if launchd.root() {
            println!(
                "Service `{}` cannot be run (but can be started) as root.",
                service.name()
            );
            continue;
        }
        load(service, file, false, launchd)?;
    }
    Ok(())
}

fn start(
    targets: &[Service],
    file: Option<&Path>,
    launchd: &Launchd,
    verbose: bool,
) -> Result<(), nb_core::Error> {
    if let Some(file) = file
        && !file.exists()
    {
        return Err(usage("Provided service file does not exist."));
    }
    for service in targets {
        if already_running(service, "started") {
            continue;
        }
        if !service.formula.installed() {
            return Err(usage(format!(
                "Formula `{}` is not installed.",
                service.name()
            )));
        }
        install_service_file(service, file)?;
        if file.is_none() && verbose {
            ohai(format!("Generated service file for {}:", service.name()));
            let contents = std::fs::read_to_string(service.dest()).unwrap_or_default();
            println!("   {}", contents.replace('\n', "\n   "));
            println!();
        }
        if launchd.root() && launchd.service_user.is_none() {
            take_root_ownership(service)?;
        }
        load(service, None, true, launchd)?;
    }
    Ok(())
}

/// `install_service_file`: write the plist where launchd loads it at login
/// (or boot, as root).
fn install_service_file(service: &Service, file: Option<&Path>) -> Result<(), nb_core::Error> {
    let contents = match file {
        Some(file) => std::fs::read_to_string(file)
            .map_err(nb_core::Error::file("failed to read the service file"))?,
        None => match &service.launchd.service_user {
            Some(user) if !service.launchd.systemd => {
                ohai(format!("Setting username in {} to: {user}", service.label));
                let contents = service.contents()?;
                nb_io::services::set_plist_user_name(&contents, user).unwrap_or(contents)
            }
            _ => service.contents()?,
        },
    };
    remove_service_files(service);
    let dest = service.dest();
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(nb_core::Error::file(
            "failed to create the service directory",
        ))?;
    }
    std::fs::write(&dest, contents)
        .map_err(nb_core::Error::file("failed to write the service file"))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o644)).map_err(
        nb_core::Error::file("failed to set the service file's permissions"),
    )?;
    if !service.launchd.systemd {
        return Ok(());
    }
    if let Some(spec) = service.formula.spec.as_ref().filter(|spec| spec.timed()) {
        let timer = service.timer_dest();
        std::fs::write(
            &timer,
            spec.to_systemd_timer(service.name(), &service.label),
        )
        .map_err(nb_core::Error::file("failed to write the timer file"))?;
        std::fs::set_permissions(&timer, std::fs::Permissions::from_mode(0o644)).map_err(
            nb_core::Error::file("failed to set the timer file's permissions"),
        )?;
    }
    systemctl(service.launchd, &["daemon-reload"])
}

/// `System::Systemctl.run`: fail like `safe_system` does.
fn systemctl(launchd: &Launchd, args: &[&str]) -> Result<(), nb_core::Error> {
    if launchd.systemctl(args).1 {
        return Ok(());
    }
    Err(usage(format!(
        "Failure while executing; `systemctl {}` failed",
        args.join(" ")
    )))
}

/// Remove the unit files, disabling the units that are still enabled.
fn remove_service_files(service: &Service) {
    for file in [service.dest(), service.timer_dest()] {
        if service.launchd.systemd
            && file.exists()
            && let Some(unit) = file.file_name().and_then(|name| name.to_str())
        {
            service.launchd.systemctl(&["disable", unit]);
        }
        let _ = std::fs::remove_file(file);
    }
}

/// `take_root_ownership?`: a service started as root runs from paths only
/// root may change.
fn take_root_ownership(service: &Service) -> Result<(), nb_core::Error> {
    let dest = service.dest();
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(program) = std::fs::read_to_string(&dest)
        .ok()
        .and_then(|plist| first_program_argument(&plist))
    {
        match std::fs::canonicalize(&program) {
            Ok(real) => {
                if let Some(parent) = real.parent().and_then(|p| std::fs::canonicalize(p).ok()) {
                    paths.push(parent);
                }
                paths.push(real);
            }
            Err(_) => opoo(format!(
                "{}: the first ProgramArguments value does not exist:\n  {program}",
                service.name()
            )),
        }
    }
    if let Some(keg) = &service.formula.keg {
        paths.push(keg.join("bin"));
        paths.push(keg.join("sbin"));
        if let Some(rack) = keg.parent().and_then(Path::parent).and_then(Path::parent) {
            paths.push(rack.join("opt").join(service.name()));
        }
    }
    paths.sort();
    paths.dedup();
    paths.retain(|path| path.exists());
    let listed: Vec<String> = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let owner = if service.launchd.systemd {
        "root"
    } else {
        "admin"
    };
    opoo(format!(
        "Taking root:{owner} ownership of some {} paths:\n  {}\nThis will require manual removal of these paths using `sudo rm` on\nnb upgrade/reinstall/uninstall.",
        service.name(),
        listed.join("\n  ")
    ));
    let owner = if service.launchd.systemd {
        "root:root"
    } else {
        "root:admin"
    };
    let status = std::process::Command::new("chown")
        .arg(owner)
        .arg(&dest)
        .args(&paths)
        .status()
        .map_err(nb_core::Error::file("failed to run chown"))?;
    if !status.success() {
        return Err(usage("failed to give root ownership of the service paths"));
    }
    if !paths.is_empty() {
        let _ = std::process::Command::new("/bin/chmod")
            .arg("+t")
            .args(&paths)
            .status();
    }
    Ok(())
}

fn first_program_argument(plist: &str) -> Option<String> {
    if let Some(exec) = plist.split("\nExecStart=\"").nth(1) {
        return exec.split('"').next().map(str::to_string);
    }
    let key = plist
        .find("<key>ProgramArguments</key>")
        .or_else(|| plist.find("<key>Program</key>"))?;
    let start = key + plist[key..].find("<string>")? + "<string>".len();
    let end = start + plist[start..].find("</string>")?;
    Some(plist[start..end].to_string())
}

/// `service_load`: bootstrap the plist into the domain, enabling it to run
/// at login (or boot) unless it is only being run.
fn load(
    service: &Service,
    file: Option<&Path>,
    enable: bool,
    launchd: &Launchd,
) -> Result<(), nb_core::Error> {
    if launchd.root() && !service.requires_root() && launchd.service_user.is_none() {
        opoo(format!(
            "`{}` must be run as non-root to start at user login!",
            service.name()
        ));
    } else if !launchd.root() && service.requires_root() {
        opoo(format!(
            "`{}` must be run as root to start at system startup!",
            service.name()
        ));
    }
    if launchd.systemd {
        // systemd loads units by location, so `run` registers one too.
        if !service.dest().exists() {
            install_service_file(service, file)?;
        }
    }
    if let Some(user) = &launchd.service_user
        && !user_exists(user)
    {
        let function = if enable { "start" } else { "run" };
        return Err(usage(format!(
            "Cannot {function} `{}` as `{user}` is not a user!",
            service.name()
        )));
    }
    if let Some(spec) = &service.formula.spec {
        for dir in spec.path_dirs() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
    if launchd.systemd {
        let timer = format!("{}.timer", service.label);
        systemctl(launchd, &["start", &service.label])?;
        if service.timed() {
            systemctl(launchd, &["start", &timer])?;
            if enable {
                systemctl(launchd, &["enable", &timer])?;
            }
        } else if enable {
            systemctl(launchd, &["enable", &service.label])?;
        }
        let done = if enable { "started" } else { "ran" };
        ohai(format!(
            "Successfully {done} `{}` (label: {})",
            service.name(),
            service.label
        ));
        return Ok(());
    }
    let (domain, warning) = launchd.domain_target();
    if let Some(warning) = warning
        && std::env::var_os("NEXTBREW_SERVICES_NO_DOMAIN_WARNING").is_none()
    {
        opoo(warning);
        eprintln!("Hide this warning by setting `NEXTBREW_SERVICES_NO_DOMAIN_WARNING=1`.");
    }

    // Running without registering loads a copy that is removed afterwards.
    let temp;
    let file: PathBuf = match file {
        Some(file) => file.to_path_buf(),
        None if enable => service.dest(),
        None => {
            temp = tempfile::Builder::new()
                .prefix(&service.label)
                .suffix(".plist")
                .tempfile()
                .map_err(nb_core::Error::file(
                    "failed to create a temporary service file",
                ))?;
            std::fs::write(temp.path(), service.contents()?).map_err(nb_core::Error::file(
                "failed to write a temporary service file",
            ))?;
            temp.path().to_path_buf()
        }
    };
    let loaded_label = nb_io::services::plist_label(&file).unwrap_or_else(|| service.label.clone());
    if enable {
        let target = format!("{domain}/{loaded_label}");
        if !launchctl(&["enable", &target]).1 {
            return Err(usage(format!(
                "Failure while executing; `launchctl enable {target}` failed"
            )));
        }
    }
    let file_arg = file.to_string_lossy().into_owned();
    let (_, ok) = launchctl(&["bootstrap", &domain, &file_arg]);
    if !ok {
        return Err(usage(format!(
            "Failure while executing; `launchctl bootstrap {domain} {file_arg}` failed"
        )));
    }
    let done = if enable { "started" } else { "ran" };
    ohai(format!(
        "Successfully {done} `{}` (label: {loaded_label})",
        service.name()
    ));
    Ok(())
}

fn stop(
    targets: &[Service],
    launchd: &Launchd,
    no_wait: bool,
    max_wait: f64,
    keep: bool,
) -> Result<(), nb_core::Error> {
    for service in targets {
        let status = service.status();
        if !status.found {
            if !keep {
                remove_service_files(service);
            }
            if service.file_present() {
                let owner = service.owner().unwrap_or_default();
                let sudo = if launchd.root() { "" } else { "sudo " };
                return Err(usage(format!(
                    "Service `{}` is started as `{owner}`. Try:\n  {sudo}{BIN} stop {}",
                    service.name(),
                    service.name()
                )));
            }
            let target = format!("{}/{}", launchd.domain_target().0, service.label);
            if !launchd.systemd && launchctl(&["bootout", &target]).1 {
                ohai(format!(
                    "Successfully stopped `{}` (label: {})",
                    service.name(),
                    service.label
                ));
            } else {
                opoo(format!("Service `{}` is not started.", service.name()));
            }
            continue;
        }

        if no_wait {
            println!("Stopping `{}`...", service.name());
        } else {
            println!("Stopping `{}`... (might take a while)", service.name());
        }
        let label = &service.label;
        if launchd.systemd {
            let no_block: &[&str] = if no_wait { &["--no-block"] } else { &[] };
            let mut units = Vec::new();
            if service.timed() {
                units.push(format!("{label}.timer"));
            }
            units.push(label.clone());
            for unit in &units {
                let action: &[&str] = if keep {
                    &["stop"]
                } else {
                    &["disable", "--now"]
                };
                launchd.systemctl(&[no_block, action, &[unit.as_str()]].concat());
            }
        }
        for domain in launchd.candidate_domains() {
            if launchd.systemd {
                break;
            }
            if !launchd.find(label).found {
                break;
            }
            let target = format!("{domain}/{label}");
            if no_wait {
                launchctl(&["bootout", &target]);
            } else {
                let mut slept = 0.0;
                let mut exit = launchctl_status(&["bootout", &target]);
                while ![Some(ESRCH), Some(DOMAIN_ACTION_NOT_SUPPORTED)].contains(&exit)
                    && (exit == Some(EINPROGRESS) || launchd.find(label).found)
                    && (max_wait == 0.0 || slept < max_wait)
                {
                    std::thread::sleep(Duration::from_secs(1));
                    slept += 1.0;
                    exit = launchctl_status(&["bootout", &target]);
                }
            }
            if launchd.find(label).found {
                launchctl(&["stop", label]);
            }
        }

        let still_loaded = launchd.find(label).found;
        if !keep && !still_loaded {
            remove_service_files(service);
            if launchd.systemd {
                launchd.systemctl(&["daemon-reload"]);
            }
        }
        if still_loaded {
            opoo(format!(
                "Unable to stop `{}` (label: {label})",
                service.name()
            ));
        } else {
            ohai(format!(
                "Successfully stopped `{}` (label: {label})",
                service.name()
            ));
        }
    }
    Ok(())
}

fn kill(targets: &[Service]) {
    for service in targets {
        if !service.status().running() {
            println!("Service `{}` is not started.", service.name());
        } else if service.keep_alive() {
            println!(
                "Service `{}` is set to automatically restart and can't be killed.",
                service.name()
            );
        } else {
            println!("Killing `{}`... (might take a while)", service.name());
            if service.launchd.systemd {
                service.launchd.systemctl(&["stop", &service.label]);
            } else {
                launchctl(&["stop", &service.label]);
            }
            if service.status().running() {
                opoo(format!(
                    "Unable to kill `{}` (label: {})",
                    service.name(),
                    service.label
                ));
            } else {
                ohai(format!(
                    "Successfully killed `{}` (label: {})",
                    service.name(),
                    service.label
                ));
            }
        }
    }
}

/// `restart`: stop what is loaded, then run again what was only running and
/// start the rest. (Homebrew runs every target again in the first group;
/// only the ones that were running are meant.)
fn restart(
    targets: &[Service],
    file: Option<&Path>,
    launchd: &Launchd,
    verbose: bool,
) -> Result<(), nb_core::Error> {
    let mut ran = Vec::new();
    let mut started = Vec::new();
    for (index, service) in targets.iter().enumerate() {
        let loaded = service.status().found;
        if loaded && !service.file_present() {
            ran.push(index);
        } else {
            started.push(index);
        }
        if loaded {
            stop(std::slice::from_ref(service), launchd, false, 60.0, false)?;
        }
    }
    let pick =
        |indices: &[usize]| -> Vec<&Service> { indices.iter().map(|&i| &targets[i]).collect() };
    for service in pick(&ran) {
        run(std::slice::from_ref(service), file, launchd)?;
    }
    for service in pick(&started) {
        start(std::slice::from_ref(service), file, launchd, verbose)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct ServiceInfo {
    name: String,
    service_name: String,
    running: bool,
    loaded: bool,
    schedulable: bool,
    pid: Option<i64>,
    exit_code: Option<i64>,
    user: Option<String>,
    status: &'static str,
    file: String,
    registered: bool,
    loaded_file: Option<String>,
    #[serde(flatten)]
    command: Option<CommandInfo>,
}

#[derive(Serialize)]
struct CommandInfo {
    command: String,
    working_dir: Option<String>,
    root_dir: Option<String>,
    log_path: Option<String>,
    error_log_path: Option<String>,
    interval: Option<i64>,
    cron: Option<String>,
}

/// `FormulaWrapper#to_hash`.
fn describe(service: &Service) -> ServiceInfo {
    let status = service.status();
    let file = if service.file_present() {
        service.registered_destination()
    } else {
        service.formula.source_file.clone()
    };
    ServiceInfo {
        name: service.name().to_string(),
        service_name: service.label.clone(),
        running: status.running(),
        loaded: status.found,
        schedulable: service.timed(),
        pid: status.pid(),
        exit_code: status.exit_code(),
        user: service.owner(),
        status: service.status_symbol(&status),
        file: file.display().to_string(),
        registered: service.file_present(),
        loaded_file: status.loaded_file(),
        command: service.formula.spec.as_ref().map(|spec| CommandInfo {
            command: spec.manual_command(),
            working_dir: spec.working_dir().map(str::to_string),
            root_dir: spec.root_dir().map(str::to_string),
            log_path: spec.log_path().map(str::to_string),
            error_log_path: spec.error_log_path().map(str::to_string),
            interval: spec.interval(),
            cron: spec.cron(),
        }),
    }
}

fn pretty_bool(value: bool) -> String {
    if !std::io::stdout().is_terminal() {
        return value.to_string();
    }
    if value {
        style("✔").green().bold().to_string()
    } else {
        style("✘").red().bold().to_string()
    }
}

fn info(targets: &[Service], json: bool, verbose: bool) -> Result<(), nb_core::Error> {
    let infos: Vec<ServiceInfo> = targets.iter().map(describe).collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&infos).unwrap_or_default()
        );
        return Ok(());
    }
    for info in infos {
        let mut out = format!("{} ({})\n", style(&info.name).bold(), info.service_name);
        out += &format!("Running: {}\n", pretty_bool(info.running));
        out += &format!("Loaded: {}\n", pretty_bool(info.loaded));
        out += &format!("Schedulable: {}\n", pretty_bool(info.schedulable));
        if let Some(pid) = info.pid {
            out += &format!("User: {}\n", info.user.clone().unwrap_or_default());
            out += &format!("PID: {pid}\n");
        }
        if verbose {
            out += &format!(
                "File: {} {}\n",
                info.file,
                pretty_bool(!info.file.is_empty())
            );
            out += &format!("Registered at login: {}\n", pretty_bool(info.registered));
            if let Some(command) = &info.command {
                out += &format!("Command: {}\n", command.command);
                let mut line = |label: &str, value: &Option<String>| {
                    if let Some(value) = value {
                        out += &format!("{label}: {value}\n");
                    }
                };
                line("Working directory", &command.working_dir);
                line("Root directory", &command.root_dir);
                line("Log", &command.log_path);
                line("Error log", &command.error_log_path);
                if let Some(interval) = command.interval {
                    out += &format!("Interval: {interval}s\n");
                }
                if let Some(cron) = &command.cron {
                    out += &format!("Cron: {cron}\n");
                }
            }
        }
        // `puts` of a string ending in a newline prints no extra line.
        print!("{out}");
    }
    Ok(())
}

#[derive(Serialize)]
struct ListEntry {
    name: String,
    status: &'static str,
    user: Option<String>,
    file: String,
    exit_code: Option<i64>,
}

async fn list(
    installer: &nb_io::Installer,
    launchd: &Launchd,
    json: bool,
) -> Result<(), nb_core::Error> {
    let services: Vec<Service> = installer
        .service_formulae()
        .await?
        .into_iter()
        .map(|formula| wrap(formula, launchd))
        .collect();
    if services.is_empty() {
        if std::io::stderr().is_terminal() {
            opoo(format!("No services available to control with `{BIN}`"));
        }
        if json {
            println!("[]");
        }
        return Ok(());
    }
    let infos: Vec<ServiceInfo> = services.iter().map(describe).collect();
    if json {
        let entries: Vec<ListEntry> = infos
            .into_iter()
            .map(|info| ListEntry {
                name: info.name,
                status: info.status,
                user: info.user,
                file: info.file,
                exit_code: info.exit_code,
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_default()
        );
        return Ok(());
    }

    let tty = std::io::stdout().is_terminal();
    let color = |code: u8, text: &str| {
        if tty {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let rows: Vec<(String, String, String, String)> = infos
        .iter()
        .map(|info| {
            let status = match info.status {
                "started" | "scheduled" => color(32, info.status),
                "stopped" | "none" => color(39, info.status),
                "error" => format!(
                    "{}{}",
                    color(31, "error  "),
                    info.exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_default()
                ),
                other => color(33, other),
            };
            let file = if info.loaded && !home.is_empty() {
                info.file.replace(&home, "~")
            } else if info.loaded {
                info.file.clone()
            } else {
                String::new()
            };
            (
                info.name.clone(),
                status,
                info.user.clone().unwrap_or_default(),
                file,
            )
        })
        .collect();
    let longest_name = rows
        .iter()
        .map(|row| row.0.chars().count())
        .chain([4])
        .max()
        .unwrap_or(4);
    let longest_status = rows
        .iter()
        .map(|row| row.1.chars().count())
        .chain([15])
        .max()
        .unwrap_or(15);
    let longest_user = rows
        .iter()
        .map(|row| row.2.chars().count())
        .chain([4])
        .max()
        .unwrap_or(4);
    // Homebrew sizes the status column for its colour codes even when it
    // prints none, and narrows only the header by them.
    let header = format!(
        "{} {} {} File",
        fit("Name", longest_name),
        fit("Status", longest_status - 9),
        fit("User", longest_user)
    );
    if tty {
        println!("{}", style(header).bold());
    } else {
        println!("{header}");
    }
    for (name, status, user, file) in rows {
        println!(
            "{} {} {} {file}",
            fit(&name, longest_name),
            fit(&status, longest_status),
            fit(&user, longest_user)
        );
    }
    Ok(())
}

/// Ruby's `%-N.Ns`: cut to `width` characters, then pad.
fn fit(text: &str, width: usize) -> String {
    let cut: String = text.chars().take(width).collect();
    format!("{cut:<width$}")
}

/// `cleanup`: stop jobs whose plist is gone, and remove plists of jobs that
/// are not running.
async fn cleanup(installer: &nb_io::Installer, launchd: &Launchd) -> Result<(), nb_core::Error> {
    let mut cleaned = Vec::new();
    let running = launchd.listed_labels();

    let mut orphans = Vec::new();
    for job in &running {
        let formula = job.strip_prefix(LABEL_PREFIX).unwrap_or(job);
        match installer.service_formula(formula).await {
            Ok(formula) => {
                let service = wrap(formula, launchd);
                if !service.dest().is_file() {
                    cleaned.push(job.clone());
                    orphans.push(service);
                }
            }
            Err(_) => opoo(format!("Service {job} not managed by `{BIN}` => skipping")),
        }
    }
    kill(&orphans);

    if let Ok(entries) = std::fs::read_dir(launchd.dest_dir()) {
        for entry in entries.filter_map(Result::ok) {
            let file = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let suffixes: &[&str] = if launchd.systemd {
                &[".service", ".timer"]
            } else {
                &[".plist"]
            };
            let Some(stem) = suffixes.iter().find_map(|suffix| name.strip_suffix(suffix)) else {
                continue;
            };
            if !stem.starts_with(LABEL_PREFIX) {
                continue;
            }
            let job = if launchd.systemd {
                stem.to_string()
            } else {
                nb_io::services::plist_label(&file).unwrap_or_else(|| stem.to_string())
            };
            if running.contains(&job) || launchd.find(&job).found {
                continue;
            }
            println!("Removing unused service file: {}", file.display());
            if std::fs::remove_file(&file).is_ok() {
                cleaned.push(file.display().to_string());
            }
        }
    }

    if cleaned.is_empty() {
        let kind = if launchd.root() { "root" } else { "user-space" };
        println!("All {kind} services OK, nothing cleaned...");
    }
    Ok(())
}
