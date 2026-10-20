//! A tap's cask file read as Homebrew's cask DSL, into the JSON Homebrew's
//! cask API would carry for it on this system, so the rest of nextbrew
//! treats it like a cask from homebrew/cask.
//!
//! Homebrew writes that JSON while "generating a hash" (`APIHashable`):
//! `HOMEBREW_PREFIX` is `$HOMEBREW_PREFIX`, the home directory `/$HOME` and
//! `appdir` `$APPDIR`. The same placeholders are written here, and nextbrew
//! replaces them when it installs, as it does for the API's casks.

use serde_json::{Map, Value as Json, json};

use super::eval::{Args, Eval, Platform, Result, RubyError, Scope};
use super::formula::{on_system, path_method, to_json};
use super::value::{Value, codename_version};
use crate::tap::TapName;

const PREFIX: &str = "$HOMEBREW_PREFIX";
const HOME: &str = "/$HOME";
const APPDIR: &str = "$APPDIR";

/// A cask read from a tap.
#[derive(Debug, Clone)]
pub struct TapCask {
    /// Shaped like the cask API's JSON.
    pub json: Json,
}

/// The artifacts moved into place, with the directory each goes to when its
/// target is relative (`Cask::Config::DEFAULT_DIRS`).
const MOVED: &[(&str, Option<&str>)] = &[
    ("app", Some("/Applications")),
    ("app_image", Some("~/Applications")),
    ("suite", Some("/Applications")),
    ("artifact", None),
    ("colorpicker", Some("~/Library/ColorPickers")),
    ("prefpane", Some("~/Library/PreferencePanes")),
    ("qlplugin", Some("~/Library/QuickLook")),
    ("mdimporter", Some("~/Library/Spotlight")),
    ("dictionary", Some("~/Library/Dictionaries")),
    ("font", Some("~/Library/Fonts")),
    ("service", Some("~/Library/Services")),
    ("input_method", Some("~/Library/Input Methods")),
    ("internet_plugin", Some("~/Library/Internet Plug-Ins")),
    ("keyboard_layout", Some("/Library/Keyboard Layouts")),
    (
        "audio_unit_plugin",
        Some("~/Library/Audio/Plug-Ins/Components"),
    ),
    ("vst_plugin", Some("~/Library/Audio/Plug-Ins/VST")),
    ("vst3_plugin", Some("~/Library/Audio/Plug-Ins/VST3")),
    ("screen_saver", Some("~/Library/Screen Savers")),
];

const COMPLETIONS: &[&str] = &[
    "bash_completion",
    "zsh_completion",
    "fish_completion",
    "pwsh_completion",
];

/// `AbstractArtifact#sort_order`: where an artifact of `kind` comes in the
/// cask's `artifacts`.
fn sort_order(kind: &str) -> usize {
    match kind {
        "preflight_steps" => 0,
        "uninstall_preflight_steps" => 1,
        "preflight" | "uninstall_preflight" => 2,
        "uninstall" => 3,
        "generated_script" => 4,
        "installer" => 5,
        "pkg" => 6,
        "binary" | "command_wrapper" => 8,
        "manpage" => 9,
        "generate_completions_from_executable" => 11,
        "postflight_steps" => 12,
        "uninstall_postflight_steps" => 13,
        "postflight" | "uninstall_postflight" => 14,
        "zap" => 15,
        k if COMPLETIONS.contains(&k) => 10,
        // The moved artifacts, and `stage_only`.
        _ => 7,
    }
}

struct CaskDsl {
    token: String,
    /// Where the download is staged, once it is: Homebrew reads a cask
    /// again then, so `Dir[staged_path/...]` sees what was unpacked.
    staged: Option<std::path::PathBuf>,
    tap: String,
    platform: Platform,
    today: String,
    version: Option<String>,
    sha256: Option<String>,
    url: Option<String>,
    url_specs: Map<String, Json>,
    names: Vec<String>,
    desc: Option<String>,
    homepage: Option<String>,
    /// Each with its `sort_order`, in the order declared.
    artifacts: Vec<(usize, Json)>,
    custom_caveats: Vec<String>,
    /// `[name, *args]` and the text, in the order first given.
    built_in_caveats: Vec<(String, String)>,
    rosetta: bool,
    depends_on: Map<String, Json>,
    conflicts: Option<Vec<String>>,
    container: Option<Map<String, Json>>,
    auto_updates: Option<bool>,
    rename: Vec<Json>,
    languages: Vec<String>,
    language: Option<Value>,
    arch: Option<Value>,
    os: Option<Value>,
    lifecycle: Map<String, Json>,
}

/// Read `source`, the cask `token` of `tap`, for `platform`, with the
/// download staged in `staged` if it is.
pub fn read_cask(
    source: &[u8],
    token: &str,
    tap: &TapName,
    platform: Platform,
    staged: Option<&std::path::Path>,
) -> std::result::Result<TapCask, RubyError> {
    let parsed = ruby_prism::parse(source);
    if let Some(error) = parsed.errors().next() {
        let offset = error.location().start_offset();
        return Err(RubyError {
            message: format!("invalid Ruby: {}", error.message()),
            line: 1 + source[..offset.min(source.len())]
                .iter()
                .filter(|b| **b == b'\n')
                .count(),
        });
    }
    let root = parsed.node();
    let mut ev = Eval::new(source, platform);
    ev.cask_versions = true;
    let mut dsl = CaskDsl {
        token: token.to_string(),
        staged: staged.map(std::path::Path::to_path_buf),
        tap: tap.to_string(),
        platform,
        today: chrono::Local::now().format("%Y-%m-%d").to_string(),
        version: None,
        sha256: None,
        url: None,
        url_specs: Map::new(),
        names: Vec::new(),
        desc: None,
        homepage: None,
        artifacts: Vec::new(),
        custom_caveats: Vec::new(),
        built_in_caveats: Vec::new(),
        rosetta: false,
        depends_on: Map::new(),
        conflicts: None,
        container: None,
        auto_updates: None,
        rename: Vec::new(),
        languages: Vec::new(),
        language: None,
        arch: None,
        os: None,
        lifecycle: Map::new(),
    };
    let mut found = false;
    let program = root.as_program_node().expect("a program");
    for statement in program.statements().body().iter() {
        if let Some(call) = statement.as_call_node()
            && call.receiver().is_none()
        {
            match call.name().as_slice() {
                b"require" | b"require_relative" => continue,
                b"cask" => found = true,
                _ => {}
            }
        }
        ev.eval(&statement, &mut dsl)?;
    }
    if !found {
        return Err(RubyError {
            message: "no cask block".to_string(),
            line: 1,
        });
    }
    Ok(TapCask { json: dsl.finish() })
}

impl CaskDsl {
    fn staged_path(&self) -> String {
        format!(
            "{PREFIX}/Caskroom/{}/{}",
            self.token,
            self.version.as_deref().unwrap_or("unknown")
        )
    }

    fn add_artifact(&mut self, kind: &str, args: &Args) -> Result<()> {
        let mut json = Map::new();
        let order = sort_order(kind);
        if let Some((_, base)) = MOVED.iter().find(|(k, _)| *k == kind) {
            let (source, target) = relocated(args);
            let resolved = resolve_target(&target.clone().unwrap_or(basename(&source)), *base);
            json.insert(kind.into(), relocated_args(&source, target));
            json.insert("target".into(), json!(resolved));
        } else if kind == "binary" {
            let (source, target) = relocated(args);
            let name = target.clone().unwrap_or(basename(&source));
            json.insert(kind.into(), relocated_args(&source, target));
            json.insert(
                "target".into(),
                json!(resolve_target(&name, Some(&format!("{PREFIX}/bin")))),
            );
        } else if kind == "manpage" {
            // The section comes from the name; a target is ignored.
            let source = args.first_str().unwrap_or_default();
            let name = basename(&source);
            let section = man_section(&source).unwrap_or_default();
            json.insert(kind.into(), json!([source]));
            json.insert(
                "target".into(),
                json!(format!("{PREFIX}/share/man/man{section}/{name}")),
            );
        } else if COMPLETIONS.contains(&kind) {
            let (source, target) = relocated(args);
            let name = completion_name(kind, &target.clone().unwrap_or(basename(&source)));
            let dir = match kind {
                "bash_completion" => "etc/bash_completion.d",
                "zsh_completion" => "share/zsh/site-functions",
                "fish_completion" => "share/fish/vendor_completions.d",
                _ => "share/pwsh/completions",
            };
            json.insert(kind.into(), relocated_args(&source, target));
            json.insert("target".into(), json!(format!("{PREFIX}/{dir}/{name}")));
        } else if kind == "command_wrapper" {
            let name = args.first_str().unwrap_or_default();
            // The options, as a hash or as keywords.
            let get = |key: &str| match args.positional.get(1) {
                Some(Value::Hash(pairs)) => pairs
                    .iter()
                    .find(|(k, _)| k.to_s() == key)
                    .map(|(_, v)| v.clone()),
                _ => args.keyword(key).cloned(),
            };
            let mut out = Map::new();
            if let Some(content) = get("content").filter(|v| !matches!(v, Value::Nil)) {
                out.insert("content".into(), to_json(&content));
            } else {
                out.insert(
                    "executable".into(),
                    to_json(&get("executable").unwrap_or(Value::Nil)),
                );
                for key in ["args", "env"] {
                    if let Some(value) = get(key).map(|v| to_json(&v)).filter(|v| !blank(v)) {
                        out.insert(key.into(), value);
                    }
                }
            }
            json.insert(kind.into(), json!([name, out]));
            json.insert("target".into(), json!(format!("{PREFIX}/bin/{name}")));
        } else if kind == "installer" {
            let mut pairs = args.keywords.clone();
            if let Some(pos) = pairs.iter().position(|(k, _)| k == "script")
                && !matches!(pairs[pos].1, Value::Hash(_))
            {
                let (_, script) = pairs.remove(pos);
                pairs.push(("executable".into(), script));
                pairs = vec![(
                    "script".into(),
                    Value::Hash(pairs.into_iter().map(|(k, v)| (Value::Sym(k), v)).collect()),
                )];
            }
            json.insert(kind.into(), json!([keywords_json(&pairs)]));
        } else if kind == "stage_only" {
            json.insert(kind.into(), json!([true]));
        } else if kind == "generate_completions_from_executable" {
            let mut items: Vec<Json> = args.positional.iter().map(to_json).collect();
            let mut options = Map::new();
            let format = args.keyword("shell_parameter_format");
            // `Utils::ShellCompletion.default_completion_shells`.
            let shells = args.keyword("shells").map(to_json).unwrap_or_else(|| {
                match format.map(Value::to_s).as_deref() {
                    Some("cobra" | "typer") => json!(["bash", "zsh", "fish", "pwsh"]),
                    _ => json!(["bash", "zsh", "fish"]),
                }
            });
            options.insert(
                "base_name".into(),
                args.keyword("base_name").map(to_json).unwrap_or(Json::Null),
            );
            options.insert(
                "shell_parameter_format".into(),
                format.map(to_json).unwrap_or(Json::Null),
            );
            options.insert("shells".into(), shells);
            items.push(Json::Object(options));
            json.insert(kind.into(), Json::Array(compact_blank(items)));
        } else {
            // `uninstall`, `zap`, `pkg`, `generated_script`: their arguments
            // as given (`@dsl_args.compact_blank`).
            let mut items: Vec<Json> = args.positional.iter().map(to_json).collect();
            if !args.keywords.is_empty() {
                items.push(keywords_json(&args.keywords));
            }
            json.insert(kind.into(), Json::Array(compact_blank(items)));
            if matches!(kind, "uninstall" | "zap") && args.keyword("kext").is_some() {
                self.built_in_caveat("kext", &[]);
            }
        }
        self.artifacts.push((order, Json::Object(json)));
        Ok(())
    }

    fn depends_on(&mut self, args: &Args, line: usize) -> Result<()> {
        let error = |message: String| RubyError { message, line };
        let mut pairs = args.keywords.clone();
        match args.first() {
            Some(Value::Sym(s)) if s == "macos" => {
                pairs.push(("macos".into(), Value::Sym("any".into())))
            }
            Some(Value::Sym(s)) if s == "linux" => {
                pairs.push(("linux".into(), Value::Sym("any".into())))
            }
            Some(other) => {
                return Err(error(format!(
                    "invalid 'depends_on' value: {}",
                    other.inspect()
                )));
            }
            None => {}
        }
        for (key, value) in pairs {
            let json = match key.as_str() {
                "macos" | "maximum_macos" => {
                    let comparator = if key == "macos" { ">=" } else { "<=" };
                    let previous = self.depends_on.get("macos").cloned();
                    let requirement = macos_requirement(&value, comparator).map_err(error)?;
                    // `depends_on :macos` keeps a version given before it.
                    if key == "macos"
                        && requirement.as_object().is_some_and(Map::is_empty)
                        && previous
                            .as_ref()
                            .and_then(Json::as_object)
                            .is_some_and(|m| !m.is_empty())
                    {
                        previous.unwrap()
                    } else {
                        requirement
                    }
                }
                "linux" => json!({}),
                "arch" => {
                    let mut arches = self
                        .depends_on
                        .get("arch")
                        .and_then(Json::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for arch in values(&value) {
                        let arch = arch.to_s().to_lowercase().replace('-', "_");
                        arches.push(match arch.as_str() {
                            "intel" | "x86_64" => json!({"type": "intel", "bits": 64}),
                            "arm64" => json!({"type": "arm", "bits": 64}),
                            other => {
                                return Err(error(format!(
                                    "invalid 'depends_on arch' values: [:{other}]"
                                )));
                            }
                        });
                    }
                    Json::Array(arches)
                }
                "formula" | "cask" => {
                    let mut names = self
                        .depends_on
                        .get(&key)
                        .and_then(Json::as_array)
                        .cloned()
                        .unwrap_or_default();
                    names.extend(values(&value).iter().map(|v| json!(v.to_s())));
                    Json::Array(names)
                }
                other => return Err(error(format!("invalid depends_on key: ':{other}'"))),
            };
            self.depends_on.insert(key, json);
        }
        Ok(())
    }

    /// `deprecate!` and `disable!`, as `Cask::DSL` records them.
    fn lifecycle(&mut self, kind: &str, args: &Args) {
        let date = args.keyword("date").map(Value::to_s);
        let mut because = args.keyword("because").map(Value::to_s);
        if kind == "disable!" && because.as_deref() == Some("unsigned") {
            because = Some("fails_gatekeeper_check".into());
        }
        let replacement = args.keyword("replacement").map(Value::to_s);
        let formula = args
            .keyword("replacement_formula")
            .map(Value::to_s)
            .filter(|s| !s.is_empty());
        let cask = args
            .keyword("replacement_cask")
            .map(Value::to_s)
            .filter(|s| !s.is_empty());
        let in_effect = date.as_ref().is_some_and(|d| *d <= self.today);
        let (prefix, flag) = if kind == "deprecate!" {
            ("deprecation", "deprecated")
        } else {
            ("disable", "disabled")
        };
        self.lifecycle.insert(
            format!("{}_args", kind.trim_end_matches('!')),
            json!({
                "date": date, "because": because,
                "replacement_formula": formula, "replacement_cask": cask,
            }),
        );
        self.lifecycle.insert(format!("{prefix}_date"), json!(date));
        let (prefix, flag) = match (kind, in_effect) {
            ("deprecate!", false) => return,
            // A disable to come deprecates for now.
            ("disable!", false) => ("deprecation", "deprecated"),
            _ => (prefix, flag),
        };
        self.lifecycle
            .insert(format!("{prefix}_reason"), json!(because));
        self.lifecycle.insert(
            format!("{prefix}_replacement_formula"),
            json!(formula.or(replacement.clone())),
        );
        self.lifecycle.insert(
            format!("{prefix}_replacement_cask"),
            json!(cask.or(replacement)),
        );
        self.lifecycle.insert(flag.into(), json!(true));
    }

    /// A built-in caveat (`Cask::DSL::Caveats`): its text, if it has one on
    /// this system.
    fn built_in_caveat(&mut self, name: &str, args: &[Value]) {
        let token = self.token.clone();
        let macos = self.platform.macos;
        let first = args.first().map(Value::to_s);
        let text = match name {
            "kext" => macos.filter(|v| *v >= (14, 0)).map(|_| {
                format!(
                    "{token} requires a kernel extension to work.\n\
                     If the installation fails, retry after you enable it in:\n  \
                     System Settings → Privacy & Security\n\n\
                     For more information, refer to vendor documentation or this Apple Technical Note:\n  \
                     https://developer.apple.com/library/content/technotes/tn2459/_index.html\n"
                )
            }),
            "unsigned_accessibility" => macos.map(|version| {
                let access = first.clone().unwrap_or_else(|| "Accessibility".into());
                let pane = if version <= (12, 0) {
                    "System Preferences → Security & Privacy → Privacy"
                } else {
                    "System Settings → Privacy & Security"
                };
                format!(
                    "{token} is not signed and requires Accessibility access,\n\
                     so you will need to re-grant Accessibility access every time the app is updated.\n\n\
                     Enable or re-enable it in:\n  {pane} → {access}\n\
                     To re-enable, untick and retick {token}.app.\n"
                )
            }),
            "path_environment_variable" => {
                let path = first.clone().unwrap_or_default();
                Some(format!(
                    "To use {token}, you may need to add the {path} directory\n\
                     to your PATH environment variable, e.g. (for Bash shell):\n  \
                     export PATH={path}:\"$PATH\"\n"
                ))
            }
            "zsh_path_helper" => {
                let path = first.clone().unwrap_or_default();
                Some(format!(
                    "To use {token}, zsh users may need to add the following line to their\n\
                     ~/.zprofile. (Among other effects, {path} will be added to the\n\
                     PATH environment variable):\n  \
                     eval `/usr/libexec/path_helper -s`\n"
                ))
            }
            "depends_on_java" => Some(match first.as_deref() {
                None | Some("any") => format!(
                    "{token} requires Java. You can install the latest version with:\n  \
                     brew install --cask temurin\n"
                ),
                Some(v) if v.contains('+') => format!(
                    "{token} requires Java {v}. You can install the latest version with:\n  \
                     brew install --cask temurin\n"
                ),
                Some(v) => format!(
                    "{token} requires Java {v}. You can install it with:\n  \
                     brew install --cask temurin@{v}\n"
                ),
            }),
            "logout" => Some(format!(
                "You must log out and log back in for the installation of {token} to take effect.\n"
            )),
            "reboot" => Some(format!(
                "You must reboot for the installation of {token} to take effect.\n"
            )),
            "license" => Some(format!(
                "Installing {token} means you have AGREED to the license at:\n  {}\n",
                first.clone().unwrap_or_default()
            )),
            "free_license" => Some(format!(
                "The vendor offers a free license for {token} at:\n  {}\n",
                first.clone().unwrap_or_default()
            )),
            // Conditional: never in the API's caveats.
            "requires_rosetta" => {
                self.rosetta = true;
                None
            }
            _ => None,
        };
        let key = std::iter::once(name.to_string())
            .chain(args.iter().map(Value::inspect))
            .collect::<Vec<_>>()
            .join("\0");
        if let Some(text) = text {
            match self.built_in_caveats.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = text,
                None => self.built_in_caveats.push((key, text)),
            }
        }
    }

    fn finish(mut self) -> Json {
        // `ArtifactSet#to_a` sorts them, as Ruby's `sort` does on macOS.
        darwin_qsort(
            &mut self.artifacts,
            &|a: &(usize, Json), b: &(usize, Json)| a.0.cmp(&b.0),
        );
        let caveats = self
            .custom_caveats
            .iter()
            .cloned()
            .chain(self.built_in_caveats.iter().map(|(_, text)| text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        let mut json = json!({
            "token": self.token,
            "full_token": format!("{}/{}", self.tap, self.token),
            "old_tokens": [],
            "tap": self.tap,
            "name": self.names,
            "desc": self.desc,
            "homepage": self.homepage,
            "url": self.url,
            "url_specs": self.url.as_ref().map(|_| Json::Object(self.url_specs.clone())),
            "version": self.version,
            "sha256": self.sha256,
            "artifacts": self.artifacts.into_iter().map(|(_, a)| a).collect::<Vec<_>>(),
            "caveats": (!caveats.is_empty()).then_some(caveats),
            "caveats_rosetta": self.rosetta.then_some(true),
            "depends_on": self.depends_on,
            "conflicts_with": self.conflicts.map(|c| json!({"cask": c})),
            "container": self.container,
            "rename": self.rename,
            "auto_updates": self.auto_updates,
            "deprecated": false,
            "deprecation_date": null,
            "deprecation_reason": null,
            "deprecation_replacement_formula": null,
            "deprecation_replacement_cask": null,
            "deprecate_args": null,
            "disabled": false,
            "disable_date": null,
            "disable_reason": null,
            "disable_replacement_formula": null,
            "disable_replacement_cask": null,
            "disable_args": null,
            "languages": self.languages,
        });
        let object = json.as_object_mut().unwrap();
        object.extend(self.lifecycle);
        json
    }
}

impl<'pr> Scope<'pr> for CaskDsl {
    fn call(
        &mut self,
        ev: &mut Eval<'pr>,
        name: &str,
        args: Args,
        block: Option<&ruby_prism::BlockNode<'pr>>,
        line: usize,
    ) -> Result<Option<Value>> {
        let text = || args.first_str().unwrap_or_default();
        let no_args = args.positional.is_empty() && args.keywords.is_empty();
        match name {
            "cask" if block.is_some() => {
                ev.exec(block.unwrap().body(), self)?;
            }
            "token" if no_args => return Ok(Some(Value::Str(self.token.clone()))),
            "version" if no_args => {
                return Ok(Some(
                    self.version
                        .clone()
                        .map(Value::Version)
                        .unwrap_or(Value::Nil),
                ));
            }
            "version" => {
                self.version = Some(match args.first() {
                    Some(Value::Sym(s)) if s == "latest" => "latest".to_string(),
                    _ => text(),
                });
            }
            "sha256" if no_args => {
                return Ok(Some(
                    self.sha256.clone().map(Value::Str).unwrap_or(Value::Nil),
                ));
            }
            "sha256" => {
                let pick = |arm: &str, intel: &[&str]| {
                    if self.platform.arm {
                        args.keyword(arm).cloned()
                    } else {
                        intel.iter().find_map(|k| args.keyword(k).cloned())
                    }
                };
                let value = match args.first() {
                    Some(value) => Some(value.clone()),
                    None if self.platform.macos.is_some() => pick("arm", &["x86_64", "intel"]),
                    None => pick("arm64_linux", &["x86_64_linux"]),
                };
                // `Checksum` keeps hex digits in lower case.
                self.sha256 = value.map(|v| match v {
                    Value::Sym(s) => s,
                    other => other.to_s().to_lowercase(),
                });
            }
            "url" if no_args => {
                return Ok(Some(self.url.clone().map(Value::Str).unwrap_or(Value::Nil)));
            }
            "url" => {
                self.url = Some(text());
                self.url_specs = url_specs(&args);
            }
            "name" if no_args => {
                return Ok(Some(Value::Array(
                    self.names.iter().cloned().map(Value::Str).collect(),
                )));
            }
            "name" => {
                for value in &args.positional {
                    self.names.extend(values(value).iter().map(Value::to_s));
                }
            }
            "desc" if no_args => {
                return Ok(Some(
                    self.desc.clone().map(Value::Str).unwrap_or(Value::Nil),
                ));
            }
            "desc" => self.desc = Some(text()),
            "homepage" if args.positional.is_empty() => {
                return Ok(Some(
                    self.homepage.clone().map(Value::Str).unwrap_or(Value::Nil),
                ));
            }
            "homepage" => self.homepage = Some(text()),
            "arch" | "os" if no_args => {
                let value = if name == "arch" { &self.arch } else { &self.os };
                return Ok(Some(value.clone().unwrap_or(Value::Nil)));
            }
            "arch" => {
                let key = if self.platform.arm { "arm" } else { "intel" };
                self.arch = Some(args.keyword(key).cloned().unwrap_or(Value::Nil));
            }
            "os" => {
                let key = if self.platform.macos.is_some() {
                    "macos"
                } else {
                    "linux"
                };
                self.os = Some(args.keyword(key).cloned().unwrap_or(Value::Nil));
            }
            "language" if no_args => return Ok(Some(self.language.clone().unwrap_or(Value::Nil))),
            "language" => {
                self.languages
                    .extend(args.positional.iter().map(Value::to_s));
                let default = args.keyword("default").is_some_and(Value::truthy);
                if let Some(block) = block
                    && default
                {
                    self.language = Some(ev.exec(block.body(), self)?);
                }
            }
            "depends_on" => self.depends_on(&args, line)?,
            "conflicts_with" => {
                let conflicts = self.conflicts.get_or_insert_default();
                for value in args.keyword("cask").map(values).unwrap_or_default() {
                    let token = value.to_s();
                    if !conflicts.contains(&token) {
                        conflicts.push(token);
                    }
                }
            }
            "container" => {
                let mut container = Map::new();
                for key in ["nested", "type"] {
                    if let Some(value) = args.keyword(key) {
                        container.insert(key.into(), to_json(value));
                    }
                }
                self.container = Some(container);
            }
            "auto_updates" => self.auto_updates = args.first().map(Value::truthy),
            "rename" => {
                let to = args.positional.get(1).map(Value::to_s);
                self.rename.push(json!({"from": text(), "to": to}));
            }
            "caveats" if block.is_some() => {
                let mut caveats = CaveatsDsl { cask: self };
                let result = ev.exec(block.unwrap().body(), &mut caveats)?;
                self.custom_caveat(&result);
            }
            "caveats" => {
                for value in &args.positional {
                    self.custom_caveat(value);
                }
            }
            "deprecate!" | "disable!" => self.lifecycle(name, &args),
            "staged_path" => return Ok(Some(Value::Path(self.staged_path().into()))),
            "caskroom_path" => {
                return Ok(Some(Value::Path(
                    format!("{PREFIX}/Caskroom/{}", self.token).into(),
                )));
            }
            "appdir" => return Ok(Some(Value::Path(APPDIR.into()))),
            "Pathname" => return Ok(Some(Value::Path(text().into()))),
            "preflight" | "postflight" | "uninstall_preflight" | "uninstall_postflight"
                if block.is_some() =>
            {
                // A block that only runs commands, as GoReleaser's casks
                // have, is the `run` steps that do the same. Other Ruby runs
                // only in Ruby: the API marks such a block as there.
                let kind = format!("{name}_steps");
                let mut flight = FlightDsl {
                    cask: self,
                    steps: super::steps::StepsDsl::for_cask(),
                };
                let mut json = Map::new();
                match ev.exec(block.unwrap().body(), &mut flight) {
                    Ok(_) => {
                        let steps = flight.steps.steps;
                        if steps.is_empty() {
                            return Ok(Some(Value::Nil));
                        }
                        json.insert(kind.clone(), json!([{"steps": steps}]));
                        self.artifacts.push((sort_order(&kind), Json::Object(json)));
                    }
                    Err(_) => {
                        json.insert(name.into(), Json::Null);
                        self.artifacts.push((sort_order(name), Json::Object(json)));
                    }
                }
            }
            "preflight_steps"
            | "postflight_steps"
            | "uninstall_preflight_steps"
            | "uninstall_postflight_steps"
                if block.is_some() =>
            {
                let mut steps = super::steps::StepsDsl::for_cask();
                ev.exec(block.unwrap().body(), &mut steps)?;
                let mut json = Map::new();
                json.insert(name.into(), json!([{"steps": steps.steps}]));
                self.artifacts.push((sort_order(name), Json::Object(json)));
            }
            "on_arch_conditional" => {
                let key = if self.platform.arm { "arm" } else { "intel" };
                return Ok(Some(args.keyword(key).cloned().unwrap_or(Value::Nil)));
            }
            "on_system_conditional" => {
                let key = if self.platform.macos.is_some() {
                    "macos"
                } else {
                    "linux"
                };
                return Ok(Some(args.keyword(key).cloned().unwrap_or(Value::Nil)));
            }
            n if IGNORED.contains(&n) => {}
            n if n.starts_with("on_") => {
                let Some(hit) = on_system(self.platform, n, &args) else {
                    return Ok(None);
                };
                if hit && let Some(block) = block {
                    return ev.exec(block.body(), self).map(Some);
                }
                return Ok(Some(Value::Nil));
            }
            n if is_artifact(n) => self.add_artifact(n, &args)?,
            _ => return Ok(None),
        }
        Ok(Some(Value::Nil))
    }

    fn call_on(
        &mut self,
        _ev: &mut Eval<'pr>,
        receiver: &Value,
        name: &str,
        args: Args,
        _block: Option<&ruby_prism::BlockNode<'pr>>,
        _line: usize,
    ) -> Result<Option<Value>> {
        // Files of the staged download are not there while the cask is
        // read, as when Homebrew reads it to install it.
        match (receiver, name) {
            (Value::Module(m), "[]" | "glob") if m == "Dir" => {
                return Ok(Some(Value::Array(self.staged_glob(&args))));
            }
            (Value::Module(m), "home") if m == "Dir" => return Ok(Some(Value::Path(HOME.into()))),
            (Value::Module(m), "new") if m == "Version" => {
                return Ok(Some(Value::Version(args.first_str().unwrap_or_default())));
            }
            (Value::Version(a), op @ ("<" | "<=" | ">" | ">=")) => {
                let Some(b) = args.first() else {
                    return Ok(None);
                };
                let ordering = compare_versions(a, &b.to_s());
                return Ok(Some(Value::Bool(match op {
                    "<" => ordering.is_lt(),
                    "<=" => ordering.is_le(),
                    ">" => ordering.is_gt(),
                    _ => ordering.is_ge(),
                })));
            }
            _ => {}
        }
        Ok(path_method(receiver, name, &args))
    }

    fn constant(&self, name: &str) -> Option<Value> {
        match name {
            "HOMEBREW_PREFIX" => Some(Value::Path(PREFIX.into())),
            "Version" => Some(Value::Module("Version".into())),
            "HOMEBREW_TEMP" => Some(Value::Path(
                std::env::var("HOMEBREW_TEMP")
                    .unwrap_or_else(|_| "/private/tmp".to_string())
                    .into(),
            )),
            _ => None,
        }
    }

    fn ignores(&self, name: &str) -> bool {
        IGNORED.contains(&name)
    }
}

impl CaskDsl {
    /// `Dir[...]` of files in the staged download, written under
    /// `staged_path` as the rest of the cask is. Nothing before it is staged.
    fn staged_glob(&self, args: &Args) -> Vec<Value> {
        let Some(real) = &self.staged else {
            return Vec::new();
        };
        let placeholder = self.staged_path();
        let mut found = Vec::new();
        for pattern in args.positional.iter().flat_map(values) {
            let pattern = pattern.to_s();
            let Some(rest) = pattern.strip_prefix(&placeholder) else {
                continue;
            };
            let pattern = std::path::PathBuf::from(format!("{}{rest}", real.display()));
            for path in crate::installer::post_install::ruby_glob(&pattern) {
                if let Ok(rel) = path.strip_prefix(real) {
                    found.push(Value::Path(
                        format!("{placeholder}/{}", rel.display()).into(),
                    ));
                }
            }
        }
        found
    }

    /// `Caveats#eval_caveats` of what a caveats block or string gave.
    fn custom_caveat(&mut self, result: &Value) {
        match result {
            Value::Nil | Value::Bool(false) => {}
            Value::Sym(s) if s == "built_in_caveat" => {}
            other => {
                let text = other.to_s();
                self.custom_caveats.push(format!("{}\n", text.trim_end()));
            }
        }
    }
}

/// Inside a Ruby `postflight` block (or the like) that only runs commands:
/// `system_command` is a `run` step. Anything else ends the reading.
struct FlightDsl<'c> {
    cask: &'c mut CaskDsl,
    steps: super::steps::StepsDsl,
}

impl<'pr> Scope<'pr> for FlightDsl<'_> {
    fn call(
        &mut self,
        ev: &mut Eval<'pr>,
        name: &str,
        args: Args,
        _block: Option<&ruby_prism::BlockNode<'pr>>,
        line: usize,
    ) -> Result<Option<Value>> {
        match name {
            "system_command" | "system_command!" => {
                let mut args = args;
                let known = [
                    "args",
                    "sudo",
                    "print_stderr",
                    "print_stdout",
                    "must_succeed",
                ];
                if args
                    .keywords
                    .iter()
                    .any(|(k, _)| !known.contains(&k.as_str()))
                {
                    return Ok(None);
                }
                if name == "system_command" && args.keyword("must_succeed").is_none() {
                    args.keywords
                        .push(("must_succeed".into(), Value::Bool(false)));
                }
                self.steps.call(ev, "run", args, None, line)?;
                // What it gives back (its status, its output) is not known.
                Ok(Some(Value::Nil))
            }
            // Where the cask is staged here, known when the steps run.
            "staged_path" => Ok(Some(Value::Str("{{staged_path}}".into()))),
            "appdir" => Ok(Some(Value::Str("{{appdir}}".into()))),
            "token" | "version" if args.positional.is_empty() => {
                self.cask.call(ev, name, args, None, line)
            }
            _ => Ok(None),
        }
    }
}

/// Homebrew's `Version` order, enough for comparing release numbers.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (
        super::value::version_tokens(a),
        super::value::version_tokens(b),
    );
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).map_or("0", String::as_str),
            b.get(i).map_or("0", String::as_str),
        );
        let ordering = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.cmp(y),
        };
        if ordering.is_ne() {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

/// Calls whose arguments do not matter.
const IGNORED: &[&str] = &["livecheck", "no_autobump!", "autobump"];

/// `Cask::DSL::ORDINARY_ARTIFACT_CLASSES`' DSL methods.
fn is_artifact(name: &str) -> bool {
    MOVED.iter().any(|(k, _)| *k == name)
        || COMPLETIONS.contains(&name)
        || matches!(
            name,
            "installer"
                | "binary"
                | "command_wrapper"
                | "generated_script"
                | "manpage"
                | "pkg"
                | "stage_only"
                | "generate_completions_from_executable"
                | "uninstall"
                | "zap"
        )
}

/// Inside `caveats do ... end`: the built-in caveats, then the cask's DSL.
struct CaveatsDsl<'c> {
    cask: &'c mut CaskDsl,
}

const BUILT_IN_CAVEATS: &[&str] = &[
    "kext",
    "unsigned_accessibility",
    "path_environment_variable",
    "zsh_path_helper",
    "files_in_usr_local",
    "depends_on_java",
    "requires_rosetta",
    "logout",
    "reboot",
    "license",
    "free_license",
];

impl<'pr> Scope<'pr> for CaveatsDsl<'_> {
    fn call(
        &mut self,
        ev: &mut Eval<'pr>,
        name: &str,
        args: Args,
        block: Option<&ruby_prism::BlockNode<'pr>>,
        line: usize,
    ) -> Result<Option<Value>> {
        if BUILT_IN_CAVEATS.contains(&name) {
            // `files_in_usr_local` says something only under /usr/local.
            self.cask.built_in_caveat(name, &args.positional);
            return Ok(Some(Value::Sym("built_in_caveat".into())));
        }
        if name == "puts" {
            self.cask
                .custom_caveats
                .extend(args.positional.iter().map(Value::to_s));
            return Ok(Some(Value::Sym("built_in_caveat".into())));
        }
        self.cask.call(ev, name, args, block, line)
    }

    fn call_on(
        &mut self,
        ev: &mut Eval<'pr>,
        receiver: &Value,
        name: &str,
        args: Args,
        block: Option<&ruby_prism::BlockNode<'pr>>,
        line: usize,
    ) -> Result<Option<Value>> {
        self.cask.call_on(ev, receiver, name, args, block, line)
    }

    fn constant(&self, name: &str) -> Option<Value> {
        self.cask.constant(name)
    }
}

/// The source and the `target:` of a moved or linked artifact.
fn relocated(args: &Args) -> (String, Option<String>) {
    let source = args.first_str().unwrap_or_default();
    let target = args
        .keyword("target")
        .map(Value::to_s)
        .filter(|t| !t.is_empty());
    (source, target)
}

fn relocated_args(source: &str, target: Option<String>) -> Json {
    match target {
        Some(target) => json!([source, {"target": target}]),
        None => json!([source]),
    }
}

/// `Relocated#resolve_target`: a relative target goes in `base`, and `~`
/// is the home directory.
fn resolve_target(target: &str, base: Option<&str>) -> String {
    if let Some(rest) = target.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return format!("{HOME}{rest}");
    }
    if target.starts_with('/') {
        return target.to_string();
    }
    match base {
        Some(base) => format!("{}/{target}", resolve_target(base, None)),
        None => target.to_string(),
    }
}

fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// `File.extname`: none for a dotfile.
fn extname(name: &str) -> &str {
    match name.rfind('.') {
        Some(0) | None => "",
        Some(i) => &name[i..],
    }
}

fn man_section(source: &str) -> Option<String> {
    let name = source.strip_suffix(".gz").unwrap_or(source);
    let (_, section) = name.rsplit_once('.')?;
    (matches!(section, "n" | "l")
        || (section.len() == 1 && ('1'..='8').contains(&section.chars().next()?)))
    .then(|| section.to_string())
}

/// The name a shell completion is linked as.
fn completion_name(kind: &str, target: &str) -> String {
    let name = basename(target);
    let stem = name[..name.len() - extname(&name).len()].to_string();
    match kind {
        "bash_completion" => stem,
        "zsh_completion" if name.starts_with('_') => name,
        "zsh_completion" => format!("_{stem}"),
        "fish_completion" if name.ends_with(".fish") => name,
        "fish_completion" => format!("{stem}.fish"),
        _ if name.starts_with('_') && name.ends_with(".ps1") => name,
        _ => format!("_{stem}.ps1"),
    }
}

/// `MacOSRequirement#to_h` of `depends_on macos:` / `maximum_macos:`.
fn macos_requirement(value: &Value, comparator: &str) -> std::result::Result<Json, String> {
    let version = |name: &str| -> std::result::Result<String, String> {
        let (major, minor) = codename_version(name)
            .ok_or_else(|| format!("invalid 'depends_on macos' value: unknown version :{name}"))?;
        Ok(if major >= 11 {
            major.to_string()
        } else {
            format!("{major}.{minor}")
        })
    };
    let items = values(value);
    match items.as_slice() {
        [Value::Sym(s)] if s == "any" => Ok(json!({})),
        [Value::Sym(s)] => Ok(json!({ comparator: [version(s)?] })),
        [Value::Str(s)] => {
            let s = s.trim();
            let split = s
                .find(|c: char| c != '<' && c != '>' && c != '=')
                .unwrap_or(0);
            let (op, rest) = s.split_at(split);
            let name = rest.trim().strip_prefix(':').unwrap_or(rest.trim());
            if !matches!(op, "<" | ">" | "==" | "<=" | ">=") {
                return Err(format!("invalid 'depends_on macos' value: {s}"));
            }
            Ok(json!({ op: [version(name)?] }))
        }
        many if many.len() > 1 => Ok(json!({
            "==": many.iter().map(|v| version(&v.to_s())).collect::<std::result::Result<Vec<_>, _>>()?
        })),
        _ => Err(format!(
            "invalid 'depends_on macos' value: {}",
            value.inspect()
        )),
    }
}

/// `Cask::URL#specs`, as `Cask#url_specs` gives them.
fn url_specs(args: &Args) -> Map<String, Json> {
    let mut specs = Map::new();
    for key in [
        "using",
        "tag",
        "branch",
        "revisions",
        "revision",
        "trust_cert",
        "cookies",
        "referer",
        "header",
        "user_agent",
        "data",
        "only_path",
    ] {
        let Some(value) = args.keyword(key).filter(|v| !matches!(v, Value::Nil)) else {
            continue;
        };
        let json = match (key, value) {
            ("user_agent", Value::Sym(s)) if s == "default" => continue,
            ("user_agent", Value::Sym(s)) => json!(format!(":{s}")),
            ("header", Value::Array(_)) => to_json(value),
            ("header", other) => json!([to_json(other)]),
            _ => to_json(value),
        };
        let key = if key == "header" { "headers" } else { key };
        specs.insert(key.into(), json);
    }
    specs
}

fn keywords_json(pairs: &[(String, Value)]) -> Json {
    Json::Object(pairs.iter().map(|(k, v)| (k.clone(), to_json(v))).collect())
}

/// One value, or the items of an array.
fn values(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    }
}

/// `Object#blank?`.
fn blank(value: &Json) -> bool {
    match value {
        Json::Null | Json::Bool(false) => true,
        Json::String(s) => s.trim().is_empty(),
        Json::Array(items) => items.is_empty(),
        Json::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// `Array#compact_blank`.
fn compact_blank(items: Vec<Json>) -> Vec<Json> {
    items.into_iter().filter(|item| !blank(item)).collect()
}

/// macOS's `qsort_r`, which Ruby's `Array#sort` uses there: not stable, so
/// equal items (artifacts of a kind) come out in its order, which the API's
/// casks have. Bentley and McIlroy's quicksort, as in FreeBSD, with an
/// insertion sort below 8 items.
fn darwin_qsort<T>(v: &mut [T], cmp: &dyn Fn(&T, &T) -> std::cmp::Ordering) {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let med3 = |v: &[T], a: usize, b: usize, c: usize| {
        if cmp(&v[a], &v[b]) == Less {
            if cmp(&v[b], &v[c]) == Less {
                b
            } else if cmp(&v[a], &v[c]) == Less {
                c
            } else {
                a
            }
        } else if cmp(&v[b], &v[c]) == Greater {
            b
        } else if cmp(&v[a], &v[c]) == Less {
            a
        } else {
            c
        }
    };
    let n = v.len();
    if n < 8 {
        for m in 1..n {
            let mut l = m;
            while l > 0 && cmp(&v[l - 1], &v[l]) == Greater {
                v.swap(l, l - 1);
                l -= 1;
            }
        }
        return;
    }
    let mut pm = n / 2;
    let (mut pl, mut pn) = (0, n - 1);
    if n > 40 {
        let d = n / 8;
        pl = med3(v, pl, pl + d, pl + 2 * d);
        pm = med3(v, pm - d, pm, pm + d);
        pn = med3(v, pn - 2 * d, pn - d, pn);
    }
    pm = med3(v, pl, pm, pn);
    v.swap(0, pm);
    let (mut pa, mut pb) = (1, 1);
    let (mut pc, mut pd) = (n as isize - 1, n - 1);
    loop {
        while pb as isize <= pc {
            match cmp(&v[pb], &v[0]) {
                Greater => break,
                Equal => {
                    v.swap(pa, pb);
                    pa += 1;
                }
                Less => {}
            }
            pb += 1;
        }
        while pb as isize <= pc {
            let c = pc as usize;
            match cmp(&v[c], &v[0]) {
                Less => break,
                Equal => {
                    v.swap(c, pd);
                    pd -= 1;
                }
                Greater => {}
            }
            pc -= 1;
        }
        if pb as isize > pc {
            break;
        }
        v.swap(pb, pc as usize);
        pb += 1;
        pc -= 1;
    }
    let pc = (pc + 1) as usize; // one past the last item less than the pivot
    let d1 = pa.min(pb - pa);
    for i in 0..d1 {
        v.swap(i, pb - d1 + i);
    }
    let d1 = (pd + 1 - pc).min(n - pd - 1);
    for i in 0..d1 {
        v.swap(pb + i, n - d1 + i);
    }
    let (left, right) = (pb - pa, pd + 1 - pc);
    if left > 1 {
        darwin_qsort(&mut v[..left], cmp);
    }
    if right > 1 {
        darwin_qsort(&mut v[n - right..], cmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_ARM: Platform = Platform {
        macos: Some((26, 0)),
        arm: true,
    };

    fn read(source: &str) -> Json {
        let tap = TapName::parse("someone/tools").unwrap();
        read_cask(source.as_bytes(), "aerospace", &tap, MAC_ARM, None)
            .unwrap()
            .json
    }

    #[test]
    fn reads_a_cask_like_the_api() {
        let json = read(
            r##"cask "aerospace" do
  version '0.21.3-Beta'
  sha256 "bdbd"

  url "https://github.com/nikitabobko/AeroSpace/releases/download/v#{version}/AeroSpace-v#{version}.zip"
  name "AeroSpace"
  desc "i3-like tiling window manager"
  homepage "https://github.com/nikitabobko/AeroSpace"
  conflicts_with cask: 'aerospace-dev'

  depends_on macos: :ventura

  postflight_steps do
    run "/usr/bin/xattr", args: ["-d", "com.apple.quarantine", "{{appdir}}/AeroSpace.app"], must_succeed: false
  end

  app "AeroSpace-v#{version}/AeroSpace.app"
  binary "AeroSpace-v#{version}/bin/aerospace"
  binary "AeroSpace-v#{version}/shell-completion/zsh/_aerospace",
      target: "#{HOMEBREW_PREFIX}/share/zsh/site-functions/_aerospace"

  Dir["#{staged_path}/AeroSpace-v#{version}/manpage/*"].each { |man| manpage man }

  zap trash: ["~/.aerospace.toml"]
end
"##,
        );
        assert_eq!(json["full_token"], "someone/tools/aerospace");
        assert_eq!(
            json["url"],
            "https://github.com/nikitabobko/AeroSpace/releases/download/v0.21.3-Beta/AeroSpace-v0.21.3-Beta.zip"
        );
        assert_eq!(json["url_specs"], json!({}));
        assert_eq!(json["depends_on"], json!({"macos": {">=": ["13"]}}));
        assert_eq!(json["conflicts_with"], json!({"cask": ["aerospace-dev"]}));
        assert_eq!(
            json["artifacts"],
            json!([
                {"app": ["AeroSpace-v0.21.3-Beta/AeroSpace.app"], "target": "/Applications/AeroSpace.app"},
                {"binary": ["AeroSpace-v0.21.3-Beta/bin/aerospace"], "target": "$HOMEBREW_PREFIX/bin/aerospace"},
                // A target under `HOMEBREW_PREFIX` is relative while the API
                // is written, so the API's has the prefix twice; nextbrew
                // takes the one given.
                {"binary": [
                    "AeroSpace-v0.21.3-Beta/shell-completion/zsh/_aerospace",
                    {"target": "$HOMEBREW_PREFIX/share/zsh/site-functions/_aerospace"}
                ], "target": "$HOMEBREW_PREFIX/bin/$HOMEBREW_PREFIX/share/zsh/site-functions/_aerospace"},
                {"postflight_steps": [{"steps": [{
                    "allow_failure": true,
                    "args": ["-d", "com.apple.quarantine", "{{appdir}}/AeroSpace.app"],
                    "command": {"path": "/usr/bin/xattr"},
                    "type": "run"
                }]}]},
                {"zap": [{"trash": ["~/.aerospace.toml"]}]},
            ])
        );
    }

    #[test]
    fn globs_the_staged_download_once_there() {
        let source = r##"cask "aerospace" do
  version "1.0"
  sha256 :no_check
  url "https://example.com/a.zip"
  Dir["#{staged_path}/a/manpage/*"].each { |man| manpage man }
end
"##;
        let tap = TapName::parse("someone/tools").unwrap();
        let staged = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(staged.path().join("a/manpage")).unwrap();
        for name in ["x.1", "y.5", ".hidden.1"] {
            std::fs::write(staged.path().join("a/manpage").join(name), "").unwrap();
        }
        let read = |staged| {
            read_cask(source.as_bytes(), "aerospace", &tap, MAC_ARM, staged)
                .unwrap()
                .json
        };
        assert_eq!(read(None)["artifacts"], json!([]));
        assert_eq!(
            read(Some(staged.path()))["artifacts"],
            json!([
                {"manpage": ["$HOMEBREW_PREFIX/Caskroom/aerospace/1.0/a/manpage/x.1"],
                 "target": "$HOMEBREW_PREFIX/share/man/man1/x.1"},
                {"manpage": ["$HOMEBREW_PREFIX/Caskroom/aerospace/1.0/a/manpage/y.5"],
                 "target": "$HOMEBREW_PREFIX/share/man/man5/y.5"},
            ])
        );
    }

    #[test]
    fn sorts_like_ruby_on_macos() {
        // From Ruby's `sort` on macOS.
        let sort = |keys: &[u8]| {
            let mut v: Vec<(u8, usize)> = keys.iter().copied().zip(0..).collect();
            darwin_qsort(&mut v, &|a, b| a.0.cmp(&b.0));
            v.into_iter().map(|(_, i)| i).collect::<Vec<_>>()
        };
        assert_eq!(sort(&[0; 7]), [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(sort(&[0; 8]), [7, 1, 2, 3, 4, 5, 6, 0]);
    }

    #[test]
    fn follows_the_system_and_versions() {
        let json = read(
            r#"cask "aerospace" do
  arch arm: "arm64", intel: "amd64"
  version "1.2.3,45"
  sha256 arm: "aaa", intel: "bbb"
  url "https://example.com/#{version.csv.first}/#{version.major_minor}/x_#{arch}.zip",
      user_agent: :fake, verified: "example.com/"
  on_arm do
    caveats do
      requires_rosetta
    end
  end
  caveats "Something to know.  "
  font "x.ttf"
  manpage "x.1.gz"
  fish_completion "completions/x.bash"
  uninstall quit: "com.example.x", delete: "/Library/x"
end
"#,
        );
        assert_eq!(json["url"], "https://example.com/1.2.3/1.2/x_arm64.zip");
        assert_eq!(json["url_specs"], json!({"user_agent": ":fake"}));
        assert_eq!(json["sha256"], "aaa");
        assert_eq!(json["caveats"], "Something to know.\n");
        assert_eq!(json["caveats_rosetta"], true);
        assert_eq!(
            json["artifacts"],
            json!([
                {"uninstall": [{"quit": "com.example.x", "delete": "/Library/x"}]},
                {"font": ["x.ttf"], "target": "/$HOME/Library/Fonts/x.ttf"},
                {"manpage": ["x.1.gz"], "target": "$HOMEBREW_PREFIX/share/man/man1/x.1.gz"},
                {"fish_completion": ["completions/x.bash"], "target": "$HOMEBREW_PREFIX/share/fish/vendor_completions.d/x.fish"},
            ])
        );
    }

    /// Read every cask of the taps under `$NB_TAP_SURVEY` (one
    /// `user--repo` directory per tap) and count what could not be read.
    #[test]
    #[ignore]
    fn survey_tap_casks() {
        let Some(root) = std::env::var_os("NB_TAP_SURVEY") else {
            return;
        };
        let mut ok = 0;
        let mut failures: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for tap_dir in std::fs::read_dir(&root).unwrap().filter_map(|e| e.ok()) {
            let dir_name = tap_dir.file_name().to_string_lossy().replace("--", "/");
            let Ok(tap) = TapName::parse(&dir_name) else {
                continue;
            };
            let dir = tap_dir.path().join("Casks");
            if !dir.is_dir() {
                continue;
            }
            for file in crate::tap::walkdir(&dir)
                .into_iter()
                .filter(|f| f.extension().is_some_and(|e| e == "rb"))
            {
                let token = file.file_stem().unwrap().to_string_lossy().into_owned();
                let source = std::fs::read(&file).unwrap();
                let read = read_cask(&source, &token, &tap, MAC_ARM, None).map_err(|e| {
                    (
                        e.message.lines().next().unwrap_or_default().to_string(),
                        e.line,
                    )
                });
                let read = read.and_then(|cask| {
                    crate::installer::cask::resolve_cask(&token, &cask.json)
                        .map_err(|e| (format!("nextbrew: {e}"), 0))
                });
                match read {
                    Ok(_) => ok += 1,
                    Err((key, line)) => failures
                        .entry(key)
                        .or_default()
                        .push(format!("{tap}/{token}:{line}")),
                }
            }
        }
        let failed: usize = failures.values().map(Vec::len).sum();
        println!("read {ok}, failed {failed}");
        for (reason, names) in &failures {
            println!(
                "{:>4}  {reason}  e.g. {}",
                names.len(),
                names.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            );
        }
    }

    /// Read the casks in `$NB_CASK_COMPARE` (`token.rb` and the API's
    /// `token.json` for it) and compare what is read with the API.
    #[test]
    #[ignore]
    fn casks_match_the_api() {
        let Some(dir) = std::env::var_os("NB_CASK_COMPARE") else {
            return;
        };
        const KEYS: &[&str] = &[
            "name",
            "desc",
            "homepage",
            "url",
            "url_specs",
            "version",
            "sha256",
            "artifacts",
            "caveats",
            "depends_on",
            "conflicts_with",
            "container",
            "rename",
            "auto_updates",
            "deprecated",
            "disabled",
            "languages",
        ];
        // `$NB_CASK_TAG`, like `sonoma` (Intel) or `arm64_sequoia`.
        let tag = std::env::var("NB_CASK_TAG").unwrap_or_else(|_| "arm64_tahoe".into());
        let codename = tag.trim_start_matches("arm64_");
        let platform = Platform {
            macos: codename_version(codename),
            arm: tag.starts_with("arm64_"),
        };
        let tap = TapName::parse("homebrew/cask").unwrap();
        let (mut same, mut failed) = (0, 0);
        let mut differ: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
        let mut reasons: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "rb"))
            .collect();
        files.sort();
        for file in files {
            let token = file.file_stem().unwrap().to_string_lossy().into_owned();
            let api: Json =
                serde_json::from_slice(&std::fs::read(file.with_extension("json")).unwrap())
                    .unwrap();
            let api = crate::installer::cask::merge_variations(&api, Some(&tag));
            let read = match read_cask(&std::fs::read(&file).unwrap(), &token, &tap, platform, None)
            {
                Ok(read) => read.json,
                Err(e) => {
                    failed += 1;
                    let key = e.message.lines().next().unwrap_or_default().to_string();
                    reasons
                        .entry(key)
                        .or_default()
                        .push(format!("{token}:{}", e.line));
                    continue;
                }
            };
            let mut ok = true;
            for key in KEYS {
                let (ours, theirs) = (&read[*key], &api[*key]);
                let equal = ours == theirs
                    || (blank(ours) && blank(theirs) && *key != "deprecated" && *key != "disabled");
                if !equal {
                    ok = false;
                    if std::env::var_os("NB_CASK_VERBOSE").is_some() {
                        println!("{token} {key}\n  ours   {ours}\n  theirs {theirs}");
                    }
                    differ.entry(key).or_default().push(token.clone());
                }
            }
            if ok {
                same += 1;
            }
        }
        println!("same {same}, failed {failed}");
        for (key, tokens) in &differ {
            println!(
                "{:>4} differ in {key}: {}",
                tokens.len(),
                tokens
                    .iter()
                    .take(6)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let mut by_count: Vec<_> = reasons.into_iter().collect();
        by_count.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        for (reason, tokens) in by_count.iter().take(30) {
            println!(
                "{:>4}  {reason}  e.g. {}",
                tokens.len(),
                tokens
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}
