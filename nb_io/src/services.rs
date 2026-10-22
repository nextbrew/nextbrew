//! `brew services` on launchd: a formula's `service` block from the API
//! turned into a launchd job, and what launchd says about that job.
//!
//! Jobs are labelled `dev.nextbrew.<formula>` rather than Homebrew's
//! `sh.brew.<formula>`, so the two tools never load, overwrite or stop each
//! other's services.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use nb_core::Error;
use serde_json::Value;

/// The prefix of every launchd label nextbrew registers.
pub const LABEL_PREFIX: &str = "dev.nextbrew.";

/// The launchd label of `formula`'s service.
pub fn label(formula: &str) -> String {
    format!("{LABEL_PREFIX}{formula}")
}

/// What the API's `$HOMEBREW_PREFIX`, `$HOMEBREW_CELLAR` and `$HOME`
/// placeholders stand for here.
#[derive(Debug, Clone, PartialEq)]
pub struct Places {
    pub prefix: PathBuf,
    pub home: PathBuf,
}

impl Places {
    /// `Homebrew::Service.replace_placeholders`.
    fn fill(&self, value: &str) -> String {
        value
            .replace("$HOMEBREW_PREFIX", &self.prefix.to_string_lossy())
            .replace(
                "$HOMEBREW_CELLAR",
                &self.prefix.join("Cellar").to_string_lossy(),
            )
            .replace("$HOME", &self.home.to_string_lossy())
    }

    /// Ruby's `File.expand_path`: `~` is the home directory, a relative
    /// path is taken from the working directory, and `.`, `..` and repeated
    /// slashes are resolved.
    fn expand(&self, path: &str) -> String {
        let path = match path.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                format!("{}{rest}", self.home.display())
            }
            _ => path.to_string(),
        };
        let mut full = PathBuf::from(&path);
        if full.is_relative() {
            full = std::env::current_dir().unwrap_or_default().join(full);
        }
        let mut out = PathBuf::from("/");
        for part in full.components() {
            match part {
                Component::ParentDir => {
                    out.pop();
                }
                Component::Normal(name) => out.push(name),
                _ => {}
            }
        }
        out.to_string_lossy().into_owned()
    }
}

/// A `service` block with a command, as `Homebrew::Service` holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceSpec {
    command: Vec<String>,
    run_type: String,
    interval: Option<i64>,
    /// `parse_cron`'s fields in its order, `None` for `*`.
    cron: Vec<(&'static str, Option<i64>)>,
    keep_alive: serde_json::Map<String, Value>,
    launch_only_once: bool,
    require_root: bool,
    environment_variables: Vec<(String, String)>,
    /// The user's `services/<formula>.env`, read when the environment is.
    env_file: Option<PathBuf>,
    working_dir: Option<String>,
    root_dir: Option<String>,
    input_path: Option<String>,
    log_path: Option<String>,
    error_log_path: Option<String>,
    restart_delay: Option<i64>,
    throttle_interval: Option<i64>,
    stop_timeout: Option<i64>,
    nice: Option<i64>,
    process_type: Option<String>,
    macos_legacy_timers: bool,
    sockets: Vec<(String, Socket)>,
    places: Places,
}

#[derive(Debug, Clone, PartialEq)]
struct Socket {
    kind: String,
    host: String,
    port: String,
}

impl ServiceSpec {
    /// The API's `service` hash (`Homebrew::Service.from_hash`), or `None`
    /// when it runs nothing on this OS and only names a service file the
    /// package ships.
    pub fn from_api(service: &Value, places: &Places) -> Result<Option<Self>, Error> {
        let invalid = |what: &str| Error::InvalidArgument {
            message: format!("unsupported service definition: {what}"),
        };
        let os_key = if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        };
        let run = match &service["run"] {
            Value::Object(by_os) => by_os.get(os_key).cloned().unwrap_or(Value::Null),
            other => other.clone(),
        };
        let run: Vec<String> = match run {
            Value::Null => return Ok(None),
            Value::String(one) => vec![one],
            Value::Array(args) => args
                .iter()
                .map(|arg| {
                    arg.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| invalid("run"))
                })
                .collect::<Result<_, _>>()?,
            _ => return Err(invalid("run")),
        };
        if run.is_empty() {
            return Ok(None);
        }
        let command = run
            .iter()
            .map(|arg| {
                let arg = places.fill(arg);
                if arg.starts_with('~') {
                    places.expand(&arg)
                } else {
                    arg
                }
            })
            .collect();

        let string = |key: &str| service[key].as_str().map(|value| places.fill(value));
        let int = |key: &str| service[key].as_i64();
        let flag = |key: &str| service[key].as_bool() == Some(true);
        let environment_variables = service["environment_variables"]
            .as_object()
            .map(|vars| {
                vars.iter()
                    .map(|(key, value)| {
                        (key.clone(), places.fill(value.as_str().unwrap_or_default()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cron = match service["cron"].as_str() {
            Some(statement) => parse_cron(statement).ok_or_else(|| invalid("cron"))?,
            None => Vec::new(),
        };
        let sockets = match &service["sockets"] {
            Value::Null => Vec::new(),
            Value::String(one) => vec![("listeners".to_string(), parse_socket(one)?)],
            Value::Object(named) => named
                .iter()
                .map(|(name, value)| {
                    Ok((
                        name.clone(),
                        parse_socket(value.as_str().unwrap_or_default())?,
                    ))
                })
                .collect::<Result<_, Error>>()?,
            _ => return Err(invalid("sockets")),
        };

        Ok(Some(Self {
            command,
            run_type: string("run_type").unwrap_or_else(|| "immediate".to_string()),
            interval: int("interval"),
            cron,
            keep_alive: service["keep_alive"]
                .as_object()
                .cloned()
                .unwrap_or_default(),
            launch_only_once: flag("launch_only_once"),
            require_root: flag("require_root"),
            environment_variables,
            env_file: None,
            working_dir: string("working_dir"),
            root_dir: string("root_dir"),
            input_path: string("input_path"),
            log_path: string("log_path"),
            error_log_path: string("error_log_path"),
            restart_delay: int("restart_delay"),
            throttle_interval: int("throttle_interval"),
            stop_timeout: int("stop_timeout"),
            nice: int("nice"),
            process_type: string("process_type"),
            macos_legacy_timers: flag("macos_legacy_timers"),
            sockets,
            places: places.clone(),
        }))
    }

    /// `requires_root?`: the service is meant to start at boot.
    pub fn requires_root(&self) -> bool {
        self.require_root
    }

    /// `keep_alive?`: launchd restarts the process when it exits.
    pub fn keep_alive(&self) -> bool {
        !self.keep_alive.is_empty() && self.keep_alive.get("always") != Some(&Value::Bool(false))
    }

    /// `timed?`: run on an interval or a schedule, not kept running.
    pub fn timed(&self) -> bool {
        self.run_type == "cron" || self.run_type == "interval"
    }

    /// The directories the service writes into, created before it starts.
    pub fn path_dirs(&self) -> Vec<PathBuf> {
        let dir = |path: &Option<String>| {
            path.as_deref()
                .filter(|path| path.starts_with('/') || path.starts_with('~'))
                .map(|path| PathBuf::from(self.places.expand(path)))
        };
        let parent =
            |path: &Option<String>| dir(path).and_then(|path| path.parent().map(Path::to_path_buf));
        let dirs: Vec<PathBuf> = [
            dir(&self.working_dir),
            dir(&self.root_dir),
            parent(&self.input_path),
            parent(&self.log_path),
            parent(&self.error_log_path),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut unique = Vec::new();
        for dir in dirs {
            if !unique.contains(&dir) {
                unique.push(dir);
            }
        }
        unique
    }

    /// Read the user's env `file` (`services/<formula>.env`) whenever the
    /// service's environment is used.
    pub fn set_env_file(&mut self, file: PathBuf) {
        self.env_file = Some(file);
    }

    /// `effective_environment_variables`: the user's env file overrides
    /// the service's variables. As Homebrew's, it is read (and warned
    /// about) each time the environment is used, and only then.
    fn effective_environment_variables(&self) -> Vec<(String, String)> {
        let mut vars = self.environment_variables.clone();
        let Some(file) = &self.env_file else {
            return vars;
        };
        let warn = |why: &str| eprintln!("Warning: Skipping {}: {why}", file.display());
        let Some(meta) = std::fs::metadata(file).ok().filter(|meta| meta.is_file()) else {
            return vars;
        };
        // SAFETY: geteuid cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            warn("user env overrides are not supported for root services.");
            return vars;
        }
        let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions());
        if mode & 0o002 != 0 {
            warn("file is world-writable.");
            return vars;
        }
        if mode & 0o020 != 0 {
            warn("file is group-writable.");
            return vars;
        }
        let Ok(text) = std::fs::read_to_string(file) else {
            return vars;
        };
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line
                .split_once('=')
                .filter(|(key, _)| !key.trim().is_empty())
            else {
                eprintln!(
                    "Warning: Skipping invalid line in {}: {line}",
                    file.display()
                );
                continue;
            };
            let (key, value) = (key.trim().to_string(), value.trim().to_string());
            match vars.iter_mut().find(|(k, _)| *k == key) {
                Some(var) => var.1 = value,
                None => vars.push((key, value)),
            }
        }
        vars
    }

    /// `manual_command`: the environment (without `PATH`) and the command,
    /// quoted for a shell.
    pub fn manual_command(&self) -> String {
        self.effective_environment_variables()
            .iter()
            .filter(|(key, _)| key != "PATH")
            .map(|(key, value)| format!("{key}=\"{value}\""))
            .chain(self.command.iter().map(|arg| sh_quote(arg)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn working_dir(&self) -> Option<&str> {
        self.working_dir.as_deref()
    }

    pub fn root_dir(&self) -> Option<&str> {
        self.root_dir.as_deref()
    }

    pub fn log_path(&self) -> Option<&str> {
        self.log_path.as_deref()
    }

    pub fn error_log_path(&self) -> Option<&str> {
        self.error_log_path.as_deref()
    }

    pub fn interval(&self) -> Option<i64> {
        self.interval
    }

    /// The parsed cron schedule as `brew services info` prints it (Ruby's
    /// `Hash#inspect`).
    pub fn cron(&self) -> Option<String> {
        if self.cron.is_empty() {
            return None;
        }
        let fields: Vec<String> = self
            .cron
            .iter()
            .map(|(key, value)| match value {
                Some(number) => format!("{key}: {number}"),
                None => format!("{key}: \"*\""),
            })
            .collect();
        Some(format!("{{{}}}", fields.join(", ")))
    }

    /// `Homebrew::Service#to_plist` under `label`.
    pub fn to_plist(&self, label: &str) -> String {
        use Plist::*;
        let path =
            |path: &Option<String>| path.as_deref().map(|path| Str(self.places.expand(path)));
        let mut dict: Vec<(String, Plist)> = vec![
            ("Label".into(), Str(label.to_string())),
            (
                "ProgramArguments".into(),
                Array(self.command.iter().cloned().map(Str).collect()),
            ),
            ("RunAtLoad".into(), Bool(true)),
        ];
        let mut add = |key: &str, value: Option<Plist>| {
            if let Some(value) = value {
                dict.push((key.to_string(), value));
            }
        };
        add(
            "LaunchOnlyOnce",
            self.launch_only_once.then_some(Bool(true)),
        );
        add(
            "LegacyTimers",
            self.macos_legacy_timers.then_some(Bool(true)),
        );
        add("ExitTimeOut", self.stop_timeout.map(Int));
        add("TimeOut", self.restart_delay.map(Int));
        add("ThrottleInterval", self.throttle_interval.map(Int));
        add(
            "ProcessType",
            self.process_type
                .as_deref()
                .map(|kind| Str(capitalize(kind))),
        );
        add("Nice", self.nice.map(Int));
        add(
            "StartInterval",
            self.interval
                .filter(|_| self.run_type == "interval")
                .map(Int),
        );
        add("WorkingDirectory", path(&self.working_dir));
        add("RootDirectory", path(&self.root_dir));
        add("StandardInPath", path(&self.input_path));
        add("StandardOutPath", path(&self.log_path));
        add("StandardErrorPath", path(&self.error_log_path));
        let environment_variables = self.effective_environment_variables();
        if !environment_variables.is_empty() {
            add(
                "EnvironmentVariables",
                Some(Dict(
                    environment_variables
                        .iter()
                        .map(|(key, value)| (key.clone(), Str(value.clone())))
                        .collect(),
                )),
            );
        }
        if self.keep_alive() {
            let keep_alive = &self.keep_alive;
            let flag = |key: &str| keep_alive.get(key).and_then(Value::as_bool);
            let value = if keep_alive.get("always").is_some_and(truthy) {
                Some(Bool(true))
            } else if let Some(value) = flag("successful_exit") {
                Some(Dict(vec![("SuccessfulExit".into(), Bool(value))]))
            } else if let Some(value) = flag("crashed") {
                Some(Dict(vec![("Crashed".into(), Bool(value))]))
            } else {
                keep_alive
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|path| !path.is_empty())
                    .map(|path| Dict(vec![("PathState".into(), Str(path.to_string()))]))
            };
            add("KeepAlive", value);
        }
        if !self.sockets.is_empty() {
            add(
                "Sockets",
                Some(Dict(
                    self.sockets
                        .iter()
                        .map(|(name, socket)| {
                            let entry = Dict(vec![
                                ("SockNodeName".into(), Str(socket.host.clone())),
                                ("SockServiceName".into(), Str(socket.port.clone())),
                                ("SockProtocol".into(), Str(socket.kind.to_uppercase())),
                            ]);
                            (name.clone(), entry)
                        })
                        .collect(),
                )),
            );
        }
        if !self.cron.is_empty() && self.run_type == "cron" {
            add(
                "StartCalendarInterval",
                Some(Dict(
                    self.cron
                        .iter()
                        .filter_map(|(key, value)| Some((key.to_string(), Int((*value)?))))
                        .collect(),
                )),
            );
        }
        add(
            "LimitLoadToSessionType",
            Some(Array(
                ["Aqua", "Background", "LoginWindow", "StandardIO", "System"]
                    .into_iter()
                    .map(|kind| Str(kind.to_string()))
                    .collect(),
            )),
        );
        to_xml(&Dict(dict))
    }

    /// `Homebrew::Service#to_systemd_unit` for `formula`.
    pub fn to_systemd_unit(&self, formula: &str) -> String {
        let present = |path: &Option<String>| {
            path.as_deref()
                .filter(|path| !path.is_empty())
                .map(|path| self.places.expand(path))
        };
        let command: Vec<String> = self.command.iter().map(|arg| systemd_quote(arg)).collect();
        let mut options = vec![
            format!(
                "Type={}",
                if self.launch_only_once {
                    "oneshot"
                } else {
                    "simple"
                }
            ),
            format!("ExecStart={}", command.join(" ")),
        ];
        let keep_alive = |key: &str| self.keep_alive.get(key).is_some_and(truthy);
        if keep_alive("always") || keep_alive("crashed") {
            // Homebrew's on-failure, so `systemctl stop` stays stopped.
            options.push("Restart=on-failure".into());
        } else if keep_alive("successful_exit") {
            options.push("Restart=on-success".into());
        }
        let mut add = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                options.push(format!("{key}={value}"));
            }
        };
        add("RestartSec", self.restart_delay.map(|n| n.to_string()));
        add("TimeoutStopSec", self.stop_timeout.map(|n| n.to_string()));
        add("Nice", self.nice.map(|n| n.to_string()));
        add("WorkingDirectory", present(&self.working_dir));
        add("RootDirectory", present(&self.root_dir));
        add(
            "StandardInput",
            present(&self.input_path).map(|p| format!("file:{p}")),
        );
        add(
            "StandardOutput",
            present(&self.log_path).map(|p| format!("append:{p}")),
        );
        add(
            "StandardError",
            present(&self.error_log_path).map(|p| format!("append:{p}")),
        );
        for (key, value) in &self.effective_environment_variables() {
            options.push(format!("Environment=\"{key}={value}\""));
        }
        format!(
            "[Unit]\nDescription=Homebrew generated unit for {formula}\n\n\
             [Install]\nWantedBy=default.target\n\n[Service]\n{}\n",
            options.join("\n")
        )
    }

    /// `Homebrew::Service#to_systemd_timer` for `formula`, whose service
    /// unit is `unit`.
    pub fn to_systemd_timer(&self, formula: &str, unit: &str) -> String {
        let mut options = Vec::new();
        if self.run_type == "cron" {
            options.push("Persistent=true".to_string());
        }
        if self.run_type == "interval" {
            options.push(format!(
                "OnUnitActiveSec={}",
                self.interval.map(|n| n.to_string()).unwrap_or_default()
            ));
        }
        if self.run_type == "cron" {
            let field = |key: &str| self.cron.iter().find(|(name, _)| *name == key)?.1;
            let two = |key: &str| field(key).map_or("*".to_string(), |n| format!("{n:02}"));
            let any = |key: &str| field(key).map_or("*".to_string(), |n| n.to_string());
            const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            let weekday = field("Weekday")
                .filter(|n| (0..=7).contains(n))
                .map_or(String::new(), |n| format!("{} ", DAYS[(n % 7) as usize]));
            options.push(format!(
                "OnCalendar={weekday}*-{}-{} {}:{}:00",
                any("Month"),
                any("Day"),
                two("Hour"),
                two("Minute")
            ));
        }
        format!(
            "[Unit]\nDescription=Homebrew generated timer for {formula}\n\n\
             [Install]\nWantedBy=timers.target\n\n[Timer]\nUnit={unit}.service\n{}\n",
            options.join("\n")
        )
    }
}

/// `Utils::Service.systemd_quote`.
fn systemd_quote(arg: &str) -> String {
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Ruby truthiness after `presence`: `false`, `nil` and empty values don't count.
fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
        && value.as_str().is_none_or(|s| !s.is_empty())
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// `Service#parse_cron`: Month, Day, Weekday, Hour and Minute, `None`
/// standing for `*`.
fn parse_cron(statement: &str) -> Option<Vec<(&'static str, Option<i64>)>> {
    let mut fields: Vec<(&'static str, Option<i64>)> =
        ["Month", "Day", "Weekday", "Hour", "Minute"]
            .into_iter()
            .map(|key| (key, None))
            .collect();
    let mut set = |key: &str, value: i64| {
        if let Some(field) = fields.iter_mut().find(|(name, _)| *name == key) {
            field.1 = Some(value);
        }
    };
    match statement {
        "@hourly" => set("Minute", 0),
        "@daily" => {
            set("Minute", 0);
            set("Hour", 0);
        }
        "@weekly" => {
            set("Minute", 0);
            set("Hour", 0);
            set("Weekday", 0);
        }
        "@monthly" => {
            set("Minute", 0);
            set("Hour", 0);
            set("Day", 1);
        }
        "@yearly" | "@annually" => {
            set("Minute", 0);
            set("Hour", 0);
            set("Day", 1);
            set("Month", 1);
        }
        _ => {
            let parts: Vec<&str> = statement.split_whitespace().collect();
            if parts.len() != 5 {
                return None;
            }
            for (key, part) in ["Minute", "Hour", "Day", "Month", "Weekday"]
                .into_iter()
                .zip(parts)
            {
                if part != "*" {
                    set(key, part.parse().ok()?);
                }
            }
        }
    }
    Some(fields)
}

/// `<type>://<host>:<port>`, as `Service#sockets` accepts it.
fn parse_socket(spec: &str) -> Result<Socket, Error> {
    let invalid = || Error::InvalidArgument {
        message: format!("unsupported service socket: {spec}"),
    };
    let (kind, rest) = spec.split_once("://").ok_or_else(invalid)?;
    let (host, port) = rest.rsplit_once(':').ok_or_else(invalid)?;
    if kind.is_empty()
        || !kind.chars().all(|c| c.is_ascii_alphabetic())
        || host.is_empty()
        || port.is_empty()
        || !port.chars().all(|c| c.is_ascii_digit())
    {
        return Err(invalid());
    }
    Ok(Socket {
        kind: kind.to_string(),
        host: host.to_string(),
        port: port.to_string(),
    })
}

/// `Utils::Shell.sh_quote`.
fn sh_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    let mut out = String::new();
    for c in arg.chars() {
        if c == '\n' {
            out.push_str("'\n'");
        } else if c.is_ascii_alphanumeric() || "_-.,:/@~+".contains(c) {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out
}

/// A property list value, written as the `plist` gem writes it: keys
/// sorted, one tab per level.
enum Plist {
    Str(String),
    Int(i64),
    Bool(bool),
    Array(Vec<Plist>),
    Dict(Vec<(String, Plist)>),
}

fn to_xml(root: &Plist) -> String {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn write(value: &Plist, depth: usize, out: &mut String) {
        let indent = "\t".repeat(depth);
        match value {
            Plist::Str(text) => {
                out.push_str(&format!("{indent}<string>{}</string>\n", escape(text)))
            }
            Plist::Int(number) => out.push_str(&format!("{indent}<integer>{number}</integer>\n")),
            Plist::Bool(flag) => out.push_str(&format!("{indent}<{flag}/>\n")),
            Plist::Array(items) => {
                out.push_str(&format!("{indent}<array>\n"));
                for item in items {
                    write(item, depth + 1, out);
                }
                out.push_str(&format!("{indent}</array>\n"));
            }
            Plist::Dict(entries) => {
                let mut sorted: Vec<&(String, Plist)> = entries.iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                out.push_str(&format!("{indent}<dict>\n"));
                for (key, item) in sorted {
                    out.push_str(&format!("{indent}\t<key>{}</key>\n", escape(key)));
                    write(item, depth + 1, out);
                }
                out.push_str(&format!("{indent}</dict>\n"));
            }
        }
    }
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n",
    );
    write(root, 0, &mut out);
    out.push_str("</plist>\n");
    out
}

/// Replace the `Label` of a package-provided plist, so it is registered
/// under nextbrew's label like a generated one.
pub fn relabel_plist(contents: &str, label: &str) -> Option<String> {
    let key = contents.find("<key>Label</key>")?;
    let start = key + contents[key..].find("<string>")? + "<string>".len();
    let end = start + contents[start..].find("</string>")?;
    Some(format!("{}{label}{}", &contents[..start], &contents[end..]))
}

/// Homebrew's `$HOMEBREW_USER_CONFIG_HOME/services/<formula>.env`:
/// `$XDG_CONFIG_HOME/nextbrew`, or `~/.nextbrew`.
pub fn user_env_file(home: &Path, formula: &str) -> PathBuf {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(config) => PathBuf::from(config).join("nextbrew"),
        None => home.join(".nextbrew"),
    };
    config.join(format!("services/{formula}.env"))
}

/// The `Label` of a plist file, if it is an XML plist with one.
pub fn plist_label(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let key = contents.find("<key>Label</key>")?;
    let start = key + contents[key..].find("<string>")? + "<string>".len();
    let end = start + contents[start..].find("</string>")?;
    Some(contents[start..end].to_string())
}

/// The plist with its job run as `user` (`--sudo-service-user`). Homebrew
/// re-emits the plist with sorted keys; the key goes in that place among
/// one-tab keys (as `to_plist` writes them), else at the end of the dict.
pub fn set_plist_user_name(contents: &str, user: &str) -> Option<String> {
    if let Some(key) = contents.find("<key>UserName</key>") {
        let start = key + contents[key..].find("<string>")? + "<string>".len();
        let end = start + contents[start..].find("</string>")?;
        return Some(format!("{}{user}{}", &contents[..start], &contents[end..]));
    }
    let at = contents
        .match_indices("\n\t<key>")
        .find(|(i, _)| {
            let key = &contents[i + "\n\t<key>".len()..];
            key.split("</key>")
                .next()
                .is_some_and(|key| key > "UserName")
        })
        .map(|(i, _)| i + 1)
        .or_else(|| contents.rfind("</dict>"))?;
    Some(format!(
        "{}\t<key>UserName</key>\n\t<string>{user}</string>\n{}",
        &contents[..at],
        &contents[at..]
    ))
}

/// The `UserName` a plist runs its job as, if it names one.
pub fn plist_user_name(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let key = contents.find("<key>UserName</key>")?;
    let start = key + contents[key..].find("<string>")? + "<string>".len();
    let end = start + contents[start..].find("</string>")?;
    Some(contents[start..end].to_string())
}

/// What `launchctl` reports about one label.
#[derive(Debug, Clone, Default)]
pub struct JobStatus {
    pub output: String,
    pub found: bool,
    /// `launchctl print` output rather than `launchctl list`'s.
    printed: bool,
    /// `systemctl status` output.
    systemd: bool,
}

impl JobStatus {
    fn capture(&self, print: &str, list: &str) -> Option<String> {
        let pattern = if self.printed { print } else { list };
        regex::Regex::new(pattern)
            .ok()?
            .captures(&self.output)?
            .get(1)
            .map(|m| m.as_str().to_string())
    }

    pub fn pid(&self) -> Option<i64> {
        if self.systemd {
            // Homebrew's `\((?!code=)`, without lookahead.
            let found = regex::Regex::new(r"Main PID: ([0-9]*) \((code=)?")
                .ok()?
                .captures(&self.output)?;
            return match found.get(2) {
                Some(_) => None,
                None => Some(found[1].parse().unwrap_or(0)),
            };
        }
        self.capture(r"pid = ([0-9]+)", r#""PID" = ([0-9]*);"#)?
            .parse()
            .ok()
    }

    pub fn exit_code(&self) -> Option<i64> {
        if self.systemd {
            let found = regex::Regex::new(r"\(code=exited, status=([0-9]*)\)|\(dead\)")
                .ok()?
                .captures(&self.output)?;
            // `(dead)` is Ruby's `nil.to_i`.
            return Some(found.get(1).map_or(0, |n| n.as_str().parse().unwrap_or(0)));
        }
        self.capture(
            r"last exit code = ([0-9]+)",
            r#""LastExitStatus" = ([0-9]*);"#,
        )?
        .parse()
        .ok()
    }

    pub fn loaded_file(&self) -> Option<String> {
        if self.systemd {
            // Homebrew's greedy `\((.*);` also takes `; enabled`; only the
            // path is meant.
            let loaded = r"Loaded: .*? \(([^;]*);";
            return self.capture(loaded, loaded);
        }
        if !self.printed {
            return None;
        }
        self.capture(r"path = (.*)", "")
    }

    pub fn running(&self) -> bool {
        self.pid().is_some_and(|pid| pid > 0)
    }
}

/// The launchd domain and paths `brew services` uses for this user.
#[derive(Debug, Clone)]
pub struct Launchd {
    uid: u32,
    root: bool,
    home: PathBuf,
    /// systemd instead of launchd, as on Linux.
    pub systemd: bool,
    /// `--sudo-service-user`: the user root runs the jobs as.
    pub service_user: Option<String>,
}

impl Launchd {
    pub fn current(home: PathBuf) -> Self {
        // SAFETY: getuid and geteuid cannot fail.
        let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
        Self {
            uid,
            root: euid == 0,
            home,
            systemd: cfg!(target_os = "linux"),
            service_user: None,
        }
    }

    pub fn root(&self) -> bool {
        self.root
    }

    /// `/Library/LaunchDaemons`, where services that start at boot live.
    pub fn boot_path(&self) -> PathBuf {
        if self.systemd {
            return PathBuf::from("/usr/lib/systemd/system");
        }
        PathBuf::from("/Library/LaunchDaemons")
    }

    /// `~/Library/LaunchAgents`, where services that start at login live.
    pub fn user_path(&self) -> PathBuf {
        if self.systemd {
            return self.home.join(".config/systemd/user");
        }
        self.home.join("Library/LaunchAgents")
    }

    /// The boot path as root, the user path otherwise.
    pub fn dest_dir(&self) -> PathBuf {
        if self.root {
            self.boot_path()
        } else {
            self.user_path()
        }
    }

    /// `System.domain_target`, with the warning Homebrew prints when it
    /// falls back to the `user/` domain.
    pub fn domain_target(&self) -> (String, Option<&'static str>) {
        if self.root {
            return ("system".to_string(), None);
        }
        let console_owner = std::fs::metadata("/dev/console")
            .map(|meta| std::os::unix::fs::MetadataExt::uid(&meta))
            .ok();
        let ssh = std::env::var_os("SSH_TTY").is_some_and(|tty| !tty.is_empty())
            && console_owner != Some(self.uid);
        let sudo = std::env::var_os("SUDO_USER").is_some_and(|user| !user.is_empty());
        if ssh {
            (
                format!("user/{}", self.uid),
                Some(
                    "running over SSH without /dev/console ownership, using user/* instead of gui/* domain!",
                ),
            )
        } else if sudo {
            (
                format!("user/{}", self.uid),
                Some("running through sudo, using user/* instead of gui/* domain!"),
            )
        } else {
            (format!("gui/{}", self.uid), None)
        }
    }

    /// `System.candidate_domain_targets`.
    pub fn candidate_domains(&self) -> Vec<String> {
        let mut domains = vec![self.domain_target().0];
        if !self.root {
            for domain in [format!("user/{}", self.uid), format!("gui/{}", self.uid)] {
                if !domains.contains(&domain) {
                    domains.push(domain);
                }
            }
        }
        domains
    }

    /// `System.launchctl_find_service`: `launchctl print` in each candidate
    /// domain, then `launchctl list`.
    pub fn find(&self, label: &str) -> JobStatus {
        if self.systemd {
            // Homebrew's loaded_service_names and status_output_success_type.
            let (output, ok) = self.systemctl(&["status", &format!("{label}.service")]);
            let found = (ok && !output.is_empty())
                || self.systemctl(&["status", &format!("{label}.timer")]).1;
            return JobStatus {
                output,
                found,
                printed: false,
                systemd: true,
            };
        }
        for domain in self.candidate_domains() {
            let (output, ok) = launchctl(&["print", &format!("{domain}/{label}")]);
            if ok && !output.is_empty() {
                return JobStatus {
                    output,
                    found: true,
                    printed: true,
                    systemd: false,
                };
            }
        }
        let (output, ok) = launchctl(&["list", label]);
        JobStatus {
            found: ok && !output.is_empty(),
            output,
            printed: false,
            systemd: false,
        }
    }

    /// Every job label `launchctl list` shows that nextbrew registered.
    pub fn listed_labels(&self) -> Vec<String> {
        if self.systemd {
            let args = ["list-units", "--type=service", "--state=running"];
            return self
                .systemctl(&[&args[..], &["--no-pager", "--no-legend"]].concat())
                .0
                .split_whitespace()
                .filter_map(|unit| unit.strip_suffix(".service"))
                .filter(|label| label.starts_with(LABEL_PREFIX))
                .map(str::to_string)
                .collect();
        }
        launchctl(&["list"])
            .0
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .filter(|label| label.starts_with(LABEL_PREFIX))
            .map(str::to_string)
            .collect()
    }
}

impl Launchd {
    /// Run `systemctl --user` (`--system` as root), returning its trimmed
    /// standard output and success.
    pub fn systemctl(&self, args: &[&str]) -> (String, bool) {
        let scope = if self.root { "--system" } else { "--user" };
        match Command::new("systemctl").arg(scope).args(args).output() {
            Ok(output) => (
                String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_string(),
                output.status.success(),
            ),
            Err(_) => (String::new(), false),
        }
    }
}

/// Run `launchctl`, returning its trimmed standard output and success.
pub fn launchctl(args: &[&str]) -> (String, bool) {
    match Command::new("/bin/launchctl").args(args).output() {
        Ok(output) => (
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string(),
            output.status.success(),
        ),
        Err(_) => (String::new(), false),
    }
}

/// Run `launchctl`, returning its exit code (`None` if it didn't run).
pub fn launchctl_status(args: &[&str]) -> Option<i32> {
    Command::new("/bin/launchctl")
        .args(args)
        .output()
        .ok()
        .and_then(|output| output.status.code())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> Places {
        Places {
            prefix: PathBuf::from("/opt/homebrew"),
            home: PathBuf::from("/Users/admin"),
        }
    }

    fn plist_of(service: &str, name: &str) -> String {
        let service: Value = serde_json::from_str(service).unwrap();
        ServiceSpec::from_api(&service, &places())
            .unwrap()
            .unwrap()
            .to_plist(&format!("sh.brew.{name}"))
    }

    // Expected files are Homebrew's own `Formula[name].service.to_plist`.
    #[test]
    fn plists_match_homebrew() {
        let cases = [
            (
                "redis",
                r#"{"run": ["$HOMEBREW_PREFIX/opt/redis/bin/redis-server", "$HOMEBREW_PREFIX/etc/redis.conf"], "run_type": "immediate", "keep_alive": {"always": true}, "working_dir": "$HOMEBREW_PREFIX/var", "log_path": "$HOMEBREW_PREFIX/var/log/redis.log", "error_log_path": "$HOMEBREW_PREFIX/var/log/redis.log"}"#,
                include_str!("../tests/fixtures/services/redis.plist"),
            ),
            (
                "asimov",
                r#"{"run": "$HOMEBREW_PREFIX/opt/asimov/bin/asimov", "run_type": "interval", "interval": 86400}"#,
                include_str!("../tests/fixtures/services/asimov.plist"),
            ),
            (
                "automysqlbackup",
                r#"{"run": "$HOMEBREW_PREFIX/opt/automysqlbackup/sbin/automysqlbackup", "run_type": "cron", "cron": "0 4 * * *", "working_dir": "$HOMEBREW_PREFIX", "log_path": "$HOMEBREW_PREFIX/var/log/automysqlbackup.log", "error_log_path": "$HOMEBREW_PREFIX/var/log/automysqlbackup.log"}"#,
                include_str!("../tests/fixtures/services/automysqlbackup.plist"),
            ),
            (
                "bitlbee",
                r#"{"run": "$HOMEBREW_PREFIX/opt/bitlbee/sbin/bitlbee", "run_type": "immediate", "sockets": "tcp://127.0.0.1:6667"}"#,
                include_str!("../tests/fixtures/services/bitlbee.plist"),
            ),
        ];
        for (name, service, expected) in cases {
            assert_eq!(plist_of(service, name), expected, "{name}");
        }
    }

    #[test]
    fn an_env_file_overrides_the_service_environment() {
        let home = tempfile::TempDir::new().unwrap();
        let places = Places {
            prefix: PathBuf::from("/opt/homebrew"),
            home: home.path().to_path_buf(),
        };
        let service: Value = serde_json::from_str(
            r#"{"run": "/bin/x", "environment_variables": {"A": "1", "B": "2"}}"#,
        )
        .unwrap();
        let mut spec = ServiceSpec::from_api(&service, &places).unwrap().unwrap();
        let env = home.path().join(".nextbrew/services/x.env");
        std::fs::create_dir_all(env.parent().unwrap()).unwrap();
        std::fs::write(&env, "# c\n B = 3 \nbad\nC=4=5\n").unwrap();
        std::fs::set_permissions(&env, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        spec.set_env_file(env);
        assert_eq!(spec.manual_command(), r#"A="1" B="3" C="4=5" /bin/x"#);
    }

    #[test]
    fn systemctl_status_is_read_like_homebrew() {
        let status = |output: &str| JobStatus {
            output: output.to_string(),
            found: true,
            printed: false,
            systemd: true,
        };
        let running = status(
            "Loaded: loaded (/home/a/.config/systemd/user/dev.nextbrew.redis.service; enabled; preset: enabled)\n     Main PID: 42 (redis-server)",
        );
        assert_eq!(running.pid(), Some(42));
        assert_eq!(
            running.loaded_file().as_deref(),
            Some("/home/a/.config/systemd/user/dev.nextbrew.redis.service")
        );
        let exited = status(
            "Process: 7 ExecStart=x (code=exited, status=3)\n Main PID: 7 (code=exited, status=3)",
        );
        assert_eq!((exited.pid(), exited.exit_code()), (None, Some(3)));
        assert_eq!(status("Active: inactive (dead)").exit_code(), Some(0));
    }

    // Expected text follows Homebrew's to_systemd_unit/to_systemd_timer templates.
    #[test]
    fn systemd_units_match_homebrew() {
        let spec = |json: &str| {
            ServiceSpec::from_api(&serde_json::from_str(json).unwrap(), &places())
                .unwrap()
                .unwrap()
        };
        let redis = spec(
            r#"{"run": ["$HOMEBREW_PREFIX/opt/redis/bin/redis-server", "a \"b\""], "run_type": "immediate", "keep_alive": {"always": true}, "working_dir": "$HOMEBREW_PREFIX/var", "log_path": "$HOMEBREW_PREFIX/var/log/redis.log", "environment_variables": {"LANG": "C"}}"#,
        );
        assert_eq!(
            redis.to_systemd_unit("redis"),
            "[Unit]\nDescription=Homebrew generated unit for redis\n\n\
             [Install]\nWantedBy=default.target\n\n[Service]\nType=simple\n\
             ExecStart=\"/opt/homebrew/opt/redis/bin/redis-server\" \"a \\\"b\\\"\"\n\
             Restart=on-failure\nWorkingDirectory=/opt/homebrew/var\n\
             StandardOutput=append:/opt/homebrew/var/log/redis.log\nEnvironment=\"LANG=C\"\n"
        );
        let backup = spec(r#"{"run": "/bin/backup", "run_type": "cron", "cron": "5 4 * * 1"}"#);
        assert_eq!(
            backup.to_systemd_timer("backup", "dev.nextbrew.backup"),
            "[Unit]\nDescription=Homebrew generated timer for backup\n\n\
             [Install]\nWantedBy=timers.target\n\n[Timer]\n\
             Unit=dev.nextbrew.backup.service\nPersistent=true\nOnCalendar=Mon *-*-* 04:05:00\n"
        );
    }

    #[test]
    fn the_user_name_goes_where_homebrew_sorts_it() {
        let plist = plist_of(r#"{"run": "/bin/x", "working_dir": "/tmp"}"#, "x");
        let with_user = set_plist_user_name(&plist, "admin").unwrap();
        assert!(with_user.contains(
            "\t<key>UserName</key>\n\t<string>admin</string>\n\t<key>WorkingDirectory</key>"
        ));
        let replaced = set_plist_user_name(&with_user, "other").unwrap();
        assert!(replaced.contains("<string>other</string>") && !replaced.contains("admin"));
        let last = set_plist_user_name("<dict>\n</dict>\n", "a").unwrap();
        assert_eq!(
            last,
            "<dict>\n\t<key>UserName</key>\n\t<string>a</string>\n</dict>\n"
        );
    }

    #[test]
    fn a_service_without_a_command_here_names_a_provided_file() {
        let service: Value =
            serde_json::from_str(r#"{"name": {"macos": "org.freedesktop.dbus-session"}}"#).unwrap();
        assert!(
            ServiceSpec::from_api(&service, &places())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn keep_alive_follows_homebrew() {
        let spec = |keep_alive: &str| {
            let service: Value = serde_json::from_str(&format!(
                r#"{{"run": "/bin/true", "keep_alive": {keep_alive}}}"#
            ))
            .unwrap();
            ServiceSpec::from_api(&service, &places()).unwrap().unwrap()
        };
        assert!(spec(r#"{"always": true}"#).keep_alive());
        assert!(!spec(r#"{"always": false}"#).keep_alive());
        let crashed = spec(r#"{"successful_exit": false, "crashed": true}"#);
        assert!(crashed.keep_alive());
        assert!(
            crashed
                .to_plist("x")
                .contains("<key>SuccessfulExit</key>\n\t\t<false/>")
        );
    }

    #[test]
    fn relabels_a_provided_plist() {
        let plist = "<dict>\n\t<key>Label</key>\n\t<string>org.example</string>\n</dict>";
        assert_eq!(
            relabel_plist(plist, "dev.nextbrew.x").unwrap(),
            "<dict>\n\t<key>Label</key>\n\t<string>dev.nextbrew.x</string>\n</dict>"
        );
    }

    #[test]
    fn expands_paths_like_ruby() {
        assert_eq!(places().expand("//Users/admin/./x/../y"), "/Users/admin/y");
        assert_eq!(places().expand("~/Library"), "/Users/admin/Library");
    }

    #[test]
    fn quotes_like_homebrew() {
        assert_eq!(sh_quote("daemon off;"), "daemon\\ off\\;");
        assert_eq!(sh_quote("--config=/a/b.json"), "--config\\=/a/b.json");
        assert_eq!(sh_quote(""), "''");
    }
}
