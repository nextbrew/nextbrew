//! `post_install_steps do ... end` read as Homebrew's
//! `Homebrew::InstallSteps::DSL`: each call records a step, shaped as the
//! formula API carries it, which nextbrew already runs.

use serde_json::{Map, Value as Json, json};

use super::eval::{Args, Eval, Result, Scope};
use super::value::Value;

/// `ABSOLUTE_TEMPLATE_TOKENS`: a path starting with one of these, like
/// `{{HOMEBREW_PREFIX}}/share`, needs no base.
const ABSOLUTE_TEMPLATE_TOKENS: &[&str] = &[
    "HOMEBREW_PREFIX",
    "HOMEBREW_CELLAR",
    "prefix",
    "opt_prefix",
    "bin",
    "sbin",
    "lib",
    "libexec",
    "share",
    "pkgshare",
    "var",
    "etc",
    "pkgetc",
    "staged_path",
    "appdir",
    "caskroom_path",
    "temp",
    "rack",
    "bash_completion",
    "zsh_completion",
    "fish_completion",
    "pwsh_completion",
];

/// `PRESERVED_STEP_VALUE_KEYS`: kept even when blank.
const PRESERVED_KEYS: &[&str] = &["after", "args", "before", "content", "env", "overwrite"];

/// The steps of a formula's `post_install_steps` block.
pub struct StepsDsl {
    default_base: Option<String>,
    default_source_base: Option<String>,
    default_target_base: Option<String>,
    pub steps: Vec<Json>,
    guards: Vec<Json>,
    next_guard_id: u32,
}

impl StepsDsl {
    /// The DSL `Formula.post_install_steps` builds: sources and targets
    /// are relative to the keg unless they say otherwise.
    pub fn for_formula() -> Self {
        Self {
            default_base: None,
            default_source_base: Some("prefix".into()),
            default_target_base: Some("prefix".into()),
            steps: Vec::new(),
            guards: Vec::new(),
            next_guard_id: 0,
        }
    }

    /// `add_step`, with `normalise_steps` and `compact_step`.
    fn add_step(&mut self, kind: &str, fields: Vec<(&str, Json)>) {
        let mut step = Map::new();
        let guards = (!self.guards.is_empty()).then(|| Json::Array(self.guards.clone()));
        for (key, value) in fields
            .into_iter()
            .chain(guards.map(|g| ("guards", g)))
            .chain([("type", json!(kind))])
        {
            if PRESERVED_KEYS.contains(&key) {
                let empty = matches!(key, "args" | "env") && is_empty_collection(&value);
                if !value.is_null() && !empty {
                    step.insert(key.into(), value);
                }
            } else if let Some(value) = deep_compact_blank(value) {
                step.insert(key.into(), value);
            }
        }
        self.steps.push(Json::Object(step));
    }

    fn with_guard<'pr>(
        &mut self,
        ev: &mut Eval<'pr>,
        mut guard: Map<String, Json>,
        block: Option<&ruby_prism::BlockNode<'pr>>,
    ) -> Result<()> {
        self.next_guard_id += 1;
        guard.insert("id".into(), json!(self.next_guard_id.to_string()));
        self.guards.push(Json::Object(guard));
        let result = match block {
            Some(block) => ev.exec(block.body(), self).map(|_| ()),
            None => Ok(()),
        };
        self.guards.pop();
        result
    }
}

fn is_empty_collection(value: &Json) -> bool {
    match value {
        Json::Array(items) => items.is_empty(),
        Json::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// `Utils.deep_compact_blank`: drops nil, false, zero and blank strings,
/// arrays and hashes, at every depth.
fn deep_compact_blank(value: Json) -> Option<Json> {
    match value {
        Json::Null | Json::Bool(false) => None,
        Json::Number(n) if n.as_f64() == Some(0.0) => None,
        Json::String(s) if s.trim().is_empty() => None,
        Json::Array(items) => {
            let items: Vec<Json> = items.into_iter().filter_map(deep_compact_blank).collect();
            (!items.is_empty()).then_some(Json::Array(items))
        }
        Json::Object(map) => {
            let map: Map<String, Json> = map
                .into_iter()
                .filter_map(|(k, v)| deep_compact_blank(v).map(|v| (k, v)))
                .collect();
            (!map.is_empty()).then_some(Json::Object(map))
        }
        other => Some(other),
    }
}

/// `default_base_for`: none for an absolute path.
fn default_base_for(path: &str, default_base: Option<&str>) -> Option<String> {
    if path.starts_with(['/', '~'])
        || ABSOLUTE_TEMPLATE_TOKENS
            .iter()
            .any(|token| path.starts_with(&format!("{{{{{token}}}}}")))
    {
        return None;
    }
    default_base.map(str::to_string)
}

/// `path_spec`.
fn path_spec(
    path: &Value,
    base: Option<&Value>,
    formula: Option<&Value>,
    default_base: Option<&str>,
) -> Map<String, Json> {
    let path = path.to_s();
    let base = base
        .filter(|b| !matches!(b, Value::Nil))
        .map(Value::to_s)
        .or_else(|| default_base_for(&path, default_base));
    let mut spec = Map::new();
    for (key, value) in [
        ("base", base),
        ("formula", formula.map(Value::to_s)),
        ("path", Some(path)),
    ] {
        if let Some(value) = value.filter(|v| !v.trim().is_empty()) {
            spec.insert(key.into(), json!(value));
        }
    }
    spec
}

/// `path_specs`: one path or an array of them.
fn path_specs(paths: &Value, base: Option<&Value>, default_base: Option<&str>) -> Json {
    let paths = match paths {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    Json::Array(
        paths
            .iter()
            .map(|path| Json::Object(path_spec(path, base, None, default_base)))
            .collect(),
    )
}

fn strings(value: Option<&Value>) -> Json {
    match value {
        Some(Value::Array(items)) => Json::Array(items.iter().map(|v| json!(v.to_s())).collect()),
        _ => json!([]),
    }
}

/// A Boolean keyword, or `default` when not given.
fn flag(args: &Args, key: &str, default: bool) -> bool {
    args.keyword(key).map_or(default, Value::truthy)
}

/// `sudo:` as Homebrew records it: a Boolean, or a symbol's name.
fn sudo(args: &Args) -> Json {
    match args.keyword("sudo") {
        Some(Value::Sym(s)) => json!(s),
        Some(other) => json!(other.truthy()),
        None => json!(false),
    }
}

impl<'pr> Scope<'pr> for StepsDsl {
    fn call(
        &mut self,
        ev: &mut Eval<'pr>,
        name: &str,
        args: Args,
        block: Option<&ruby_prism::BlockNode<'pr>>,
        _line: usize,
    ) -> Result<Option<Value>> {
        let nil = Value::Nil;
        let arg = |i: usize| args.positional.get(i).unwrap_or(&nil);
        let kw = |key: &str| args.keyword(key);
        let base = self.default_base.clone();
        let source_base = self.default_source_base.clone();
        let target_base = self.default_target_base.clone();
        let path = |i: usize| json!(path_spec(arg(i), kw("base"), None, base.as_deref()));
        let source = |i: usize, formula: Option<&Value>| {
            json!(path_spec(
                arg(i),
                kw("source_base"),
                formula,
                source_base.as_deref()
            ))
        };
        let target = |value: &Value, formula: Option<&Value>| {
            json!(path_spec(
                value,
                kw("target_base"),
                formula,
                target_base.as_deref()
            ))
        };
        match name {
            // Template tokens Homebrew expands when it runs the steps.
            "formula_name" | "name" | "token" | "arch" => {
                return Ok(Some(Value::Str(format!("{{{{{name}}}}}"))));
            }
            "version" => return Ok(Some(Value::Str("{{version}}".into()))),
            "if_path_exists" | "unless_path_exists" => {
                let mut guard = path_spec(arg(0), kw("base"), None, base.as_deref());
                let condition = name.trim_end_matches("_path_exists");
                guard.insert("condition".into(), json!(format!("{condition}_exists")));
                self.with_guard(ev, guard, block)?;
            }
            "on_macos" | "on_linux" => {
                let mut guard = Map::new();
                guard.insert("condition".into(), json!("on"));
                guard.insert("value".into(), json!(name.trim_start_matches("on_")));
                self.with_guard(ev, guard, block)?;
            }
            "mkdir" | "mkdir_p" | "touch" => self.add_step(name, vec![("path", path(0))]),
            "move" | "mv" => {
                let force = flag(&args, "force", false);
                let overwrite = flag(&args, "overwrite", true);
                let overwrite = if name == "mv" {
                    overwrite || force
                } else {
                    overwrite
                };
                let force = name == "move" && force;
                self.add_step(
                    "move",
                    vec![
                        ("source", source(0, None)),
                        ("target", target(arg(1), None)),
                        ("force", json!(force)),
                        ("overwrite", json!(overwrite)),
                        ("source_glob", json!(flag(&args, "source_glob", false))),
                    ],
                );
            }
            "move_children" | "move_contents" => self.add_step(
                name,
                vec![
                    ("source", source(0, None)),
                    ("target", target(arg(1), None)),
                ],
            ),
            "copy" => self.add_step(
                "copy",
                vec![
                    ("source", source(0, None)),
                    ("target", target(arg(1), None)),
                    ("recursive", json!(flag(&args, "recursive", false))),
                    (
                        "overwrite",
                        if flag(&args, "overwrite", true) {
                            Json::Null
                        } else {
                            json!(false)
                        },
                    ),
                    ("source_glob", json!(flag(&args, "source_glob", false))),
                ],
            ),
            "remove" => self.add_step(
                "remove",
                vec![
                    ("paths", path_specs(arg(0), kw("base"), base.as_deref())),
                    ("recursive", json!(flag(&args, "recursive", false))),
                    ("sudo", sudo(&args)),
                    (
                        "symlink_target_contains",
                        json!(kw("symlink_target_contains").map(Value::to_s)),
                    ),
                    (
                        "content_contains",
                        json!(kw("content_contains").map(Value::to_s)),
                    ),
                ],
            ),
            "inreplace" => self.add_step(
                "inreplace",
                vec![
                    ("path", path(0)),
                    (
                        "before",
                        json!(match arg(1) {
                            Value::Regexp(source, _) => source.clone(),
                            other => other.to_s(),
                        }),
                    ),
                    ("after", json!(arg(2).to_s())),
                    ("regexp", json!(matches!(arg(1), Value::Regexp(..)))),
                    (
                        "regexp_options",
                        match arg(1) {
                            Value::Regexp(_, options) => json!(options),
                            _ => Json::Null,
                        },
                    ),
                    ("skip_audit", json!(!flag(&args, "audit_result", true))),
                    ("first_only", json!(!flag(&args, "global", true))),
                ],
            ),
            "symlink" | "ln_s" | "ln_sf" => {
                let force = flag(&args, "force", false);
                let overwrite = name == "ln_sf" || flag(&args, "overwrite", false);
                let uninstall =
                    flag(&args, "uninstall", false) || flag(&args, "remove_on_uninstall", false);
                self.add_step(
                    "symlink",
                    vec![
                        ("source", source(0, kw("source_formula"))),
                        ("target", target(arg(1), kw("target_formula"))),
                        (
                            "force",
                            if force || overwrite {
                                json!(true)
                            } else {
                                Json::Null
                            },
                        ),
                        (
                            "uninstall",
                            if uninstall { json!(true) } else { Json::Null },
                        ),
                        ("source_glob", json!(flag(&args, "source_glob", false))),
                        ("sudo", sudo(&args)),
                    ],
                );
            }
            "link_dir" | "symlink_tree" | "link_children" | "symlink_children" => {
                let kind = if name.ends_with("children") {
                    "link_children"
                } else {
                    "link_dir"
                };
                // `target_base` defaults to `:homebrew_prefix` here.
                let target_base = kw("target_base")
                    .cloned()
                    .unwrap_or(Value::Sym("homebrew_prefix".into()));
                let target_path = match (kind, arg(1)) {
                    ("link_children", Value::Nil) => arg(0),
                    (_, value) => value,
                };
                let mut fields = vec![
                    ("source", source(0, None)),
                    (
                        "target",
                        json!(path_spec(
                            target_path,
                            Some(&target_base),
                            None,
                            self.default_target_base.as_deref()
                        )),
                    ),
                ];
                if kind == "link_children" {
                    fields.push(("prefix", json!(kw("prefix").map(Value::to_s))));
                    fields.push(("suffix", json!(kw("suffix").map(Value::to_s))));
                }
                self.add_step(kind, fields);
            }
            "write" | "write_file" => {
                let mut content = arg(1).to_s();
                let append = if name == "write" {
                    true
                } else {
                    flag(&args, "append_newline", false)
                };
                if append && !content.ends_with('\n') {
                    content.push('\n');
                }
                let overwrite = flag(&args, "overwrite", name == "write_file");
                self.add_step(
                    "write",
                    vec![
                        ("path", path(0)),
                        ("content", json!(content)),
                        (
                            "overwrite",
                            if overwrite { json!(true) } else { Json::Null },
                        ),
                    ],
                );
            }
            "init_data_dir" => {
                let using = match kw("using").map(Value::to_s).as_deref() {
                    Some("postgresql") => "postgresql_initdb".to_string(),
                    Some("mysql") => "mysql_initialize".to_string(),
                    Some("mariadb") => "mariadb_install_db".to_string(),
                    other => other.unwrap_or_default().to_string(),
                };
                self.add_step(
                    "init_data_dir",
                    vec![
                        ("path", path(0)),
                        ("using", json!(using)),
                        ("locale", json!(kw("locale").map(Value::to_s))),
                    ],
                );
            }
            "compile_gsettings_schemas"
            | "gio_querymodules"
            | "update_gio_modules_cache"
            | "gtk_update_icon_cache"
            | "update_gtk_icon_cache"
            | "update_mime_database"
            | "update_desktop_database" => {
                let (kind, dir) = match name {
                    "compile_gsettings_schemas" => (name, "share/glib-2.0/schemas"),
                    "gio_querymodules" | "update_gio_modules_cache" => {
                        ("gio_querymodules", "lib/gio/modules")
                    }
                    "gtk_update_icon_cache" | "update_gtk_icon_cache" => {
                        ("gtk_update_icon_cache", "share/icons/hicolor")
                    }
                    "update_mime_database" => (name, "share/mime"),
                    _ => (name, "share/applications"),
                };
                let spec = path_spec(
                    &Value::Str(dir.into()),
                    Some(&Value::Sym("homebrew_prefix".into())),
                    None,
                    None,
                );
                self.add_step(kind, vec![("path", json!(spec))]);
            }
            "gdk_pixbuf_query_loaders" | "update_gdk_pixbuf_loaders_cache" => {
                self.add_step("gdk_pixbuf_query_loaders", Vec::new())
            }
            "delete_keychain_certificate" | "delete_keychain_certificates" => {
                let key = if name.ends_with('s') {
                    "fingerprint_of"
                } else {
                    "matching_certificate"
                };
                let matching = kw(key)
                    .map(|path| json!(path_spec(path, kw("base"), None, None)))
                    .unwrap_or(Json::Null);
                self.add_step(
                    "delete_keychain_certificate",
                    vec![
                        ("name", json!(arg(0).to_s())),
                        ("matching_certificate", matching),
                    ],
                );
            }
            "set_permissions" => self.add_step(
                name,
                vec![
                    ("paths", path_specs(arg(0), kw("base"), base.as_deref())),
                    ("permissions", json!(arg(1).to_s())),
                    ("non_recursive", json!(!flag(&args, "recursive", true))),
                ],
            ),
            "set_ownership" => {
                let group = kw("group").map(Value::to_s).filter(|g| g != "staff");
                self.add_step(
                    name,
                    vec![
                        ("paths", path_specs(arg(0), kw("base"), base.as_deref())),
                        ("user", json!(kw("user").map(Value::to_s))),
                        ("group", json!(group)),
                        ("non_recursive", json!(!flag(&args, "recursive", true))),
                    ],
                );
            }
            "change_dylib_id" => self.add_step(
                name,
                vec![
                    (
                        "source",
                        json!(path_spec(
                            arg(0),
                            kw("base"),
                            None,
                            self.default_source_base.as_deref()
                        )),
                    ),
                    ("id", json!(arg(1).to_s())),
                    (
                        "resolve_source",
                        json!(flag(&args, "resolve_source", false)),
                    ),
                ],
            ),
            "run" => {
                let optional = |key: &str| {
                    kw(key)
                        .filter(|v| !matches!(v, Value::Nil))
                        .map(|p| json!(path_spec(p, None, None, base.as_deref())))
                        .unwrap_or(Json::Null)
                };
                let env = match kw("env") {
                    Some(Value::Hash(pairs)) => Json::Object(
                        pairs
                            .iter()
                            .filter(|(_, v)| !matches!(v, Value::Nil))
                            .map(|(k, v)| (k.to_s(), json!(v.to_s())))
                            .collect(),
                    ),
                    _ => json!({}),
                };
                let writable = kw("writable_paths")
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                self.add_step(
                    "run",
                    vec![
                        ("command", json!(path_spec(arg(0), kw("base"), None, None))),
                        ("args", strings(kw("args"))),
                        ("env", env),
                        ("sudo", json!(flag(&args, "sudo", false))),
                        ("allow_failure", json!(!flag(&args, "must_succeed", true))),
                        ("print_stdout", json!(flag(&args, "print_stdout", false))),
                        ("suppress_stderr", json!(!flag(&args, "print_stderr", true))),
                        ("stdin_path", optional("stdin_path")),
                        ("stdout_path", optional("stdout_path")),
                        ("chdir", optional("chdir")),
                        (
                            "writable_paths",
                            path_specs(&writable, kw("writable_base"), base.as_deref()),
                        ),
                        (
                            "network_access",
                            json!(flag(&args, "network_access", false)),
                        ),
                    ],
                );
            }
            "terminate_process" => {
                let matching = kw("match").map(Value::to_s).filter(|m| m != "name");
                let attempts = match kw("attempts") {
                    Some(Value::Int(n)) if *n != 1 => json!(n),
                    _ => Json::Null,
                };
                self.add_step(
                    name,
                    vec![
                        ("name", json!(arg(0).to_s())),
                        ("match", json!(matching)),
                        ("sudo", json!(flag(&args, "sudo", false))),
                        ("attempts", attempts),
                        ("must_succeed", json!(flag(&args, "must_succeed", false))),
                        ("notices", strings(kw("notices"))),
                        (
                            "failure_message",
                            json!(kw("failure_message").map(Value::to_s)),
                        ),
                    ],
                );
            }
            "warn" => self.add_step(name, vec![("message", json!(arg(0).to_s()))]),
            "install_gzipped_executable" => self.add_step(
                name,
                vec![
                    ("source", source(0, None)),
                    ("target", target(arg(1), None)),
                ],
            ),
            "configure_gcc_runtime"
            | "configure_glibc_runtime"
            | "configure_clang_system"
            | "configure_php"
            | "bootstrap_cpython" => self.add_step(name, Vec::new()),
            "bootstrap_pypy" => self.add_step(
                name,
                vec![("abi_version", json!(kw("abi_version").map(Value::to_s)))],
            ),
            _ => return Ok(None),
        }
        Ok(Some(Value::Nil))
    }

    /// `version.major` and `version.major_minor` are templates too.
    fn call_on(
        &mut self,
        _ev: &mut Eval<'pr>,
        receiver: &Value,
        name: &str,
        _args: Args,
        _block: Option<&ruby_prism::BlockNode<'pr>>,
        _line: usize,
    ) -> Result<Option<Value>> {
        Ok(match (receiver, name) {
            (Value::Str(s), "major" | "major_minor") if s == "{{version}}" => {
                Some(Value::Str(format!("{{{{version.{name}}}}}")))
            }
            _ => None,
        })
    }
}
