//! A tap's formula file read as Homebrew's formula DSL, into the JSON
//! Homebrew's formula API would carry for it on this system, so the rest
//! of nextbrew treats it like a formula from homebrew/core.

use serde_json::{Map, Value as Json, json};

use super::eval::{Args, Eval, Platform, Result, RubyError, Scope, on_macos_version};
use super::value::Value;
use crate::tap::TapName;

/// Homebrew's `HOMEBREW_BOTTLE_DOMAIN` default, the root of a bottle that
/// names none.
const DEFAULT_BOTTLE_ROOT: &str = "https://ghcr.io/v2/homebrew/core";

/// A formula read from a tap.
#[derive(Debug, Clone)]
pub struct TapFormula {
    /// Shaped like the formula API's JSON.
    pub json: Json,
    /// Where the `def install` (or `define_method(:install)`) in effect on
    /// this system starts, to run it when there is no bottle.
    pub install_at: Option<usize>,
    /// The same for `def post_install`.
    pub post_install_at: Option<usize>,
    /// `resource` blocks: name, url and sha256.
    pub resources: Vec<(String, String, Option<String>)>,
    /// The stable url's `using:`.
    pub using: Option<String>,
}

#[derive(Default)]
struct Bottle {
    root_url: Option<String>,
    rebuild: i64,
    /// Tag, cellar and sha256, in the order declared.
    files: Vec<(String, String, String)>,
}

struct Dep {
    name: String,
    tags: Vec<String>,
}

struct FormulaDsl {
    name: String,
    tap: TapName,
    platform: Platform,
    today: String,
    desc: Option<String>,
    homepage: Option<String>,
    url: Option<(String, Args)>,
    sha256: Option<String>,
    version: Option<String>,
    revision: i64,
    version_scheme: i64,
    head: Option<String>,
    bottle: Bottle,
    deps: Vec<Dep>,
    uses_from_macos: Vec<(String, Vec<String>, Option<String>)>,
    requirements: Vec<Json>,
    /// `post_install_steps do ... end`, as the API carries them.
    post_install_steps: Vec<Json>,
    keg_only: Option<(Json, String)>,
    conflicts: Vec<(String, Option<String>)>,
    link_overwrite: Vec<String>,
    lifecycle: Map<String, Json>,
    caveats: Option<String>,
    service: Option<Json>,
    pour_bottle_only_if: Option<String>,
    install_at: Option<usize>,
    post_install_at: Option<usize>,
    resources: Vec<(String, String, Option<String>)>,
    patches: usize,
}

/// Read `source`, the formula `name` of `tap`, for `platform`.
pub fn read_formula(
    source: &[u8],
    name: &str,
    tap: &TapName,
    platform: Platform,
) -> std::result::Result<TapFormula, RubyError> {
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
    let mut dsl = FormulaDsl {
        name: name.to_string(),
        tap: tap.clone(),
        platform,
        today: chrono::Local::now().format("%Y-%m-%d").to_string(),
        desc: None,
        homepage: None,
        url: None,
        sha256: None,
        version: None,
        revision: 0,
        version_scheme: 0,
        head: None,
        bottle: Bottle::default(),
        deps: Vec::new(),
        uses_from_macos: Vec::new(),
        requirements: Vec::new(),
        keg_only: None,
        conflicts: Vec::new(),
        link_overwrite: Vec::new(),
        lifecycle: Map::new(),
        caveats: None,
        service: None,
        pour_bottle_only_if: None,
        install_at: None,
        post_install_at: None,
        post_install_steps: Vec::new(),
        resources: Vec::new(),
        patches: 0,
    };

    let program = root.as_program_node().expect("a program");
    let mut class_body = None;
    for statement in program.statements().body().iter() {
        if let Some(class) = statement.as_class_node() {
            let superclass = class
                .superclass()
                .map(|s| String::from_utf8_lossy(s.location().as_slice()).into_owned())
                .unwrap_or_default();
            if superclass.ends_with("Formula") && class_body.is_none() {
                class_body = Some(class.body());
            }
            // Other classes (download strategies and the like) are only
            // reached through what the formula uses.
            continue;
        }
        if let Some(call) = statement.as_call_node()
            && call.receiver().is_none()
            && matches!(call.name().as_slice(), b"require" | b"require_relative")
        {
            continue;
        }
        ev.eval(&statement, &mut dsl)?;
    }
    let Some(body) = class_body else {
        return Err(RubyError {
            message: "no formula class".to_string(),
            line: 1,
        });
    };
    if let Some(body) = body {
        let statements: Vec<_> = match body.as_statements_node() {
            Some(s) => s.body().iter().collect(),
            None => vec![body],
        };
        for statement in statements {
            dsl.class_statement(&mut ev, &statement)?;
        }
    }
    if ev.defs.contains_key("caveats") {
        let text = ev.call_def(&root, "caveats", &mut dsl)?;
        dsl.caveats = Some(text.to_s()).filter(|t| !t.is_empty());
    }
    if dsl.url.is_none() {
        return Err(RubyError {
            message: if dsl.head.is_some() {
                format!("{name} is a head-only formula")
            } else {
                format!("{name} has no stable download")
            },
            line: 1,
        });
    }
    Ok(dsl.finish())
}

impl FormulaDsl {
    fn class_statement<'pr>(
        &mut self,
        ev: &mut Eval<'pr>,
        node: &ruby_prism::Node<'pr>,
    ) -> Result<()> {
        ev.eval(node, self)?;
        Ok(())
    }

    /// The `version` so far: given, or read from the stable url.
    fn current_version(&self) -> Option<String> {
        if let Some(version) = &self.version {
            return Some(version.clone());
        }
        let (url, args) = self.url.as_ref()?;
        let spec = args
            .keyword("tag")
            .map(Value::to_s)
            .unwrap_or_else(|| url.clone());
        crate::tap::version::detect(&spec)
    }

    fn pkg_version(&self) -> String {
        let version = self.current_version().unwrap_or_default();
        if self.revision > 0 {
            format!("{version}_{}", self.revision)
        } else {
            version
        }
    }

    fn path(&self, rel: &str) -> Value {
        let base = format!("$HOMEBREW_CELLAR/{}/{}", self.name, self.pkg_version());
        Value::Path(
            if rel.is_empty() {
                base
            } else {
                format!("{base}/{rel}")
            }
            .into(),
        )
    }

    fn opt(&self, rel: &str) -> Value {
        let base = format!("$HOMEBREW_PREFIX/opt/{}", self.name);
        Value::Path(
            if rel.is_empty() {
                base
            } else {
                format!("{base}/{rel}")
            }
            .into(),
        )
    }

    fn on_system(&mut self, name: &str, args: &Args) -> Option<bool> {
        on_system(self.platform, name, args)
    }

    fn depends_on(&mut self, args: &Args, line: usize) -> Result<()> {
        let requirement = |name: &str, version: Option<String>, contexts: Vec<String>| {
            json!({
                "name": name, "cask": null, "download": null,
                "version": version, "contexts": contexts, "specs": ["stable"],
            })
        };
        for (key, value) in &args.keywords {
            let tags: Vec<String> = match value {
                Value::Array(items) => items.iter().map(Value::to_s).collect(),
                other => vec![other.to_s()],
            };
            match key.as_str() {
                "macos" | "maximum_macos" => {
                    let version = tags
                        .first()
                        .and_then(|t| Value::Sym(t.clone()).macos_version())
                        .map(|(major, minor)| {
                            if minor == 0 {
                                major.to_string()
                            } else {
                                format!("{major}.{minor}")
                            }
                        });
                    self.requirements.push(requirement(
                        key,
                        version,
                        tags.into_iter().skip(1).collect(),
                    ));
                }
                "xcode" => {
                    let (version, contexts): (Vec<String>, Vec<String>) = tags
                        .into_iter()
                        .partition(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()));
                    self.requirements.push(requirement(
                        "xcode",
                        version.into_iter().next(),
                        contexts,
                    ));
                }
                "arch" => {
                    self.requirements
                        .push(requirement("arch", tags.first().cloned(), Vec::new()))
                }
                other => {
                    return Err(RubyError {
                        message: format!("`depends_on {other}:` is not supported"),
                        line,
                    });
                }
            }
        }
        for value in &args.positional {
            match value {
                Value::Str(name) => self.deps.push(Dep {
                    name: name.clone(),
                    tags: Vec::new(),
                }),
                Value::Hash(pairs) => {
                    // Only the first pair counts, as in Homebrew.
                    let Some((name, tags)) = pairs.first() else {
                        continue;
                    };
                    let tags = match tags {
                        Value::Array(items) => items.iter().map(Value::to_s).collect(),
                        other => vec![other.to_s()],
                    };
                    match name {
                        Value::Str(name) => self.deps.push(Dep {
                            name: name.clone(),
                            tags,
                        }),
                        Value::Sym(kind) => self.requirements.push(requirement(kind, None, tags)),
                        other => {
                            return Err(RubyError {
                                message: format!(
                                    "`depends_on {}` is not supported",
                                    other.inspect()
                                ),
                                line,
                            });
                        }
                    }
                }
                Value::Sym(kind) if matches!(kind.as_str(), "macos" | "linux" | "xcode") => {
                    self.requirements.push(requirement(kind, None, Vec::new()));
                }
                other => {
                    return Err(RubyError {
                        message: format!("`depends_on {}` is not supported", other.inspect()),
                        line,
                    });
                }
            }
        }
        Ok(())
    }

    fn lifecycle(&mut self, kind: &str, args: &Args) {
        let date = args.keyword("date").map(Value::to_s);
        let reason = args.keyword("because").map(Value::to_s);
        let replacement_formula = args
            .keyword("replacement_formula")
            .or(args.keyword("replacement"))
            .map(Value::to_s);
        let replacement_cask = args.keyword("replacement_cask").map(Value::to_s);
        let in_effect = date.as_ref().is_none_or(|d| *d <= self.today);
        let (flag, prefix) = if kind == "disable!" {
            if in_effect {
                ("disabled", "disable")
            } else {
                if self.lifecycle.contains_key("deprecation_date") {
                    return;
                }
                ("deprecated", "deprecation")
            }
        } else {
            if !in_effect {
                self.lifecycle
                    .insert("deprecation_date".into(), json!(date));
                return;
            }
            ("deprecated", "deprecation")
        };
        self.lifecycle.insert(flag.into(), json!(true));
        self.lifecycle.insert(format!("{prefix}_date"), json!(date));
        self.lifecycle
            .insert(format!("{prefix}_reason"), json!(reason));
        self.lifecycle.insert(
            format!("{prefix}_replacement_formula"),
            json!(replacement_formula),
        );
        self.lifecycle.insert(
            format!("{prefix}_replacement_cask"),
            json!(replacement_cask),
        );
    }

    fn finish(self) -> TapFormula {
        let version = self.current_version();
        let pkg_version = self.pkg_version();
        let root = self
            .bottle
            .root_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BOTTLE_ROOT.to_string());
        let ghcr = ghcr_root(&root);
        let mut files = Map::new();
        for (tag, cellar, sha) in &self.bottle.files {
            let suffix = if self.bottle.rebuild > 0 {
                format!(".{}", self.bottle.rebuild)
            } else {
                String::new()
            };
            let url = match &ghcr {
                Some(root) => {
                    let image = self.name.replace('@', "/").replace('+', "x");
                    format!("{root}/{image}/blobs/sha256:{sha}")
                }
                None => format!(
                    "{root}/{}",
                    url_encode(&format!(
                        "{}-{pkg_version}.{tag}.bottle{suffix}.tar.gz",
                        self.name
                    ))
                ),
            };
            files.insert(
                tag.clone(),
                json!({"cellar": cellar, "url": url, "sha256": sha}),
            );
        }
        let bottle = if files.is_empty() {
            json!({})
        } else {
            json!({"stable": {"rebuild": self.bottle.rebuild, "root_url": ghcr.unwrap_or(root), "files": files}})
        };
        let select = |want: &dyn Fn(&Dep) -> bool| -> Vec<String> {
            let mut names: Vec<String> = Vec::new();
            for dep in self.deps.iter().filter(|d| want(d)) {
                if !names.contains(&dep.name) {
                    names.push(dep.name.clone());
                }
            }
            names
        };
        let has = |d: &Dep, tag: &str| d.tags.iter().any(|t| t == tag);
        let dependencies = select(&|d| {
            !["build", "test", "optional", "recommended"]
                .iter()
                .any(|t| has(d, t))
        });
        let uses_from_macos: Vec<Json> = self
            .uses_from_macos
            .iter()
            .map(|(name, tags, _)| match tags.as_slice() {
                [] => json!(name),
                [one] => json!({ name.clone(): one }),
                many => json!({ name.clone(): many }),
            })
            .collect();
        let bounds: Vec<Json> = self
            .uses_from_macos
            .iter()
            .map(|(_, _, since)| match since {
                Some(since) => json!({"since": since}),
                None => json!({}),
            })
            .collect();
        let (keg_only, keg_only_reason) = match &self.keg_only {
            Some((reason, explanation)) => {
                (true, json!({"reason": reason, "explanation": explanation}))
            }
            None => (false, Json::Null),
        };
        let mut out = json!({
            "name": self.name,
            "full_name": format!("{}/{}", self.tap, self.name),
            "tap": self.tap.name(),
            "oldnames": [],
            "aliases": [],
            "desc": self.desc,
            "homepage": self.homepage,
            "versions": {"stable": version, "head": self.head.as_ref().map(|_| "HEAD"), "bottle": !self.bottle.files.is_empty()},
            "urls": {
                "stable": self.url.as_ref().map(|(url, args)| json!({
                    "url": url,
                    "tag": args.keyword("tag").map(Value::to_s),
                    "revision": args.keyword("revision").map(Value::to_s),
                    "using": args.keyword("using").map(Value::to_s),
                    "checksum": self.sha256,
                })),
                "head": self.head.as_ref().map(|url| json!({"url": url})),
            },
            "revision": self.revision,
            "version_scheme": self.version_scheme,
            "bottle": bottle,
            "pour_bottle_only_if": self.pour_bottle_only_if,
            "keg_only": keg_only,
            "keg_only_reason": keg_only_reason,
            "build_dependencies": select(&|d| has(d, "build")),
            "dependencies": dependencies,
            "test_dependencies": select(&|d| has(d, "test")),
            "recommended_dependencies": select(&|d| has(d, "recommended")),
            "optional_dependencies": select(&|d| has(d, "optional")),
            "uses_from_macos": uses_from_macos,
            "uses_from_macos_bounds": bounds,
            "requirements": self.requirements,
            "conflicts_with": self.conflicts.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "conflicts_with_reasons": self.conflicts.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>(),
            "link_overwrite": self.link_overwrite,
            "caveats": self.caveats,
            "deprecated": false,
            "disabled": false,
            "post_install_steps": self.post_install_steps,
            "post_install_defined": self.post_install_at.is_some(),
            "service": self.service,
            "variations": {},
        });
        if let Some(map) = out.as_object_mut() {
            for (key, value) in self.lifecycle {
                map.insert(key, value);
            }
        }
        let using = self
            .url
            .as_ref()
            .and_then(|(_, args)| args.keyword("using").map(Value::to_s));
        TapFormula {
            json: out,
            install_at: self.install_at,
            post_install_at: self.post_install_at,
            resources: self.resources,
            using,
        }
    }
}

/// `GitHubPackages::URL_REGEX`: a GHCR root, normalised.
fn ghcr_root(root: &str) -> Option<String> {
    let rest = root
        .strip_prefix("https://ghcr.io/v2/")
        .or_else(|| root.strip_prefix("docker://ghcr.io/"))?;
    let mut parts = rest.split('/');
    let (org, repo) = (parts.next()?, parts.next()?);
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    if !ok(org) || !ok(repo) {
        return None;
    }
    Some(format!(
        "https://ghcr.io/v2/{}/{}",
        org.to_lowercase(),
        repo.strip_prefix("homebrew-").unwrap_or(repo)
    ))
}

/// `ERB::Util.url_encode`.
fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.~-".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Calls the formula class body may make that change nothing nextbrew
/// uses.
const IGNORED: &[&str] = &[
    "license",
    "mirror",
    "livecheck",
    "test",
    "option",
    "deprecated_option",
    "skip_clean",
    "no_autobump!",
    "autobump",
    "compatibility_version",
    "fails_with",
    "cxxstdlib_check",
    "env",
    "needs",
    "go_resource",
    "allow_network_access!",
    "deny_network_access!",
    "preserve_rpath",
];

impl<'pr> Scope<'pr> for FormulaDsl {
    fn call(
        &mut self,
        ev: &mut Eval<'pr>,
        name: &str,
        args: Args,
        block: Option<&ruby_prism::BlockNode<'pr>>,
        line: usize,
    ) -> Result<Option<Value>> {
        let text = || args.first_str().unwrap_or_default();
        match name {
            "desc" => self.desc = Some(text()),
            "homepage" => self.homepage = Some(text()),
            "url" => {
                let mut args = args;
                let url = args.positional.remove(0).to_s();
                self.url = Some((url, args));
            }
            "sha256" => self.sha256 = Some(text()),
            "version" if args.positional.is_empty() => {
                return Ok(Some(
                    self.current_version()
                        .map(Value::Version)
                        .unwrap_or(Value::Nil),
                ));
            }
            "version" => self.version = Some(text()).filter(|v| !v.is_empty()),
            "revision" => {
                self.revision = match args.first() {
                    Some(Value::Int(n)) => *n,
                    _ => 0,
                }
            }
            "version_scheme" => {
                self.version_scheme = match args.first() {
                    Some(Value::Int(n)) => *n,
                    _ => 0,
                }
            }
            "name" if args.positional.is_empty() => return Ok(Some(Value::Str(self.name.clone()))),
            "head" => {
                if let Some(url) = args.first() {
                    self.head = Some(url.to_s());
                }
            }
            "stable" if block.is_some() => {
                ev.exec(block.unwrap().body(), self)?;
            }
            "bottle" if block.is_some() => {
                let mut bottle = BottleDsl {
                    bottle: std::mem::take(&mut self.bottle),
                };
                ev.exec(block.unwrap().body(), &mut bottle)?;
                self.bottle = bottle.bottle;
            }
            "depends_on" => self.depends_on(&args, line)?,
            "uses_from_macos" => {
                let since = args.keyword("since").map(Value::to_s);
                match args.first() {
                    Some(Value::Str(name)) => {
                        self.uses_from_macos.push((name.clone(), Vec::new(), since))
                    }
                    Some(Value::Hash(pairs)) => {
                        if let Some((name, tags)) = pairs.first() {
                            let tags = match tags {
                                Value::Array(items) => items.iter().map(Value::to_s).collect(),
                                other => vec![other.to_s()],
                            };
                            self.uses_from_macos.push((name.to_s(), tags, since));
                        }
                    }
                    _ => {}
                }
            }
            "keg_only" => {
                let reason = match args.first() {
                    Some(Value::Sym(s)) => json!(format!(":{s}")),
                    Some(other) => json!(other.to_s()),
                    None => json!(""),
                };
                let explanation = args.positional.get(1).map(Value::to_s).unwrap_or_default();
                self.keg_only = Some((reason, explanation));
            }
            "conflicts_with" => {
                let because = args.keyword("because").map(Value::to_s);
                for name in &args.positional {
                    self.conflicts.push((name.to_s(), because.clone()));
                }
            }
            "link_overwrite" => self
                .link_overwrite
                .extend(args.positional.iter().map(Value::to_s)),
            "deprecate!" | "disable!" => self.lifecycle(name, &args),
            "pour_bottle?" => self.pour_bottle_only_if = args.keyword("only_if").map(Value::to_s),
            "resource" => {
                if let Some(block) = block {
                    let mut resource = ResourceDsl::default();
                    ev.exec(block.body(), &mut resource)?;
                    if let Some(url) = resource.url {
                        self.resources.push((text(), url, resource.sha256));
                    }
                }
            }
            "patch" => self.patches += 1,
            "post_install_steps" if block.is_some() && args.positional.is_empty() => {
                let mut steps = super::steps::StepsDsl::for_formula();
                ev.exec(block.unwrap().body(), &mut steps)?;
                self.post_install_steps.extend(steps.steps);
            }
            "service" if block.is_some() => {
                let mut service = ServiceDsl {
                    formula: self,
                    out: Map::new(),
                };
                ev.exec(block.unwrap().body(), &mut service)?;
                let out = service.out;
                self.service = Some(Json::Object(out));
            }
            "define_method" if block.is_some() => match args.first().map(Value::to_s).as_deref() {
                Some("install") => self.install_at = Some(block.unwrap().location().start_offset()),
                Some("post_install") => {
                    self.post_install_at = Some(block.unwrap().location().start_offset())
                }
                _ => {}
            },
            "odie" => {
                return Err(RubyError {
                    message: text(),
                    line,
                });
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
            "build" => return Ok(Some(Value::Module("build".into()))),
            n if IGNORED.contains(&n) => {}
            n if n.starts_with("on_") => {
                let Some(hit) = self.on_system(n, &args) else {
                    return Ok(None);
                };
                if hit && let Some(block) = block {
                    return ev.exec(block.body(), self).map(Some);
                }
                return Ok(Some(Value::Nil));
            }
            // Paths, for `caveats` and `service`.
            "prefix" => return Ok(Some(self.path(""))),
            "bin" | "sbin" | "lib" | "libexec" | "include" | "share" | "frameworks" => {
                return Ok(Some(self.path(name)));
            }
            "pkgshare" => return Ok(Some(self.path(&format!("share/{}", self.name)))),
            "opt_prefix" => return Ok(Some(self.opt(""))),
            n if n.starts_with("opt_") => return Ok(Some(self.opt(n.trim_start_matches("opt_")))),
            "etc" => return Ok(Some(Value::Path("$HOMEBREW_PREFIX/etc".into()))),
            "pkgetc" => {
                return Ok(Some(Value::Path(
                    format!("$HOMEBREW_PREFIX/etc/{}", self.name).into(),
                )));
            }
            "var" => return Ok(Some(Value::Path("$HOMEBREW_PREFIX/var".into()))),
            "plist_name" => return Ok(Some(Value::Str(format!("homebrew.mxcl.{}", self.name)))),
            "service_name" => return Ok(Some(Value::Str(format!("homebrew.{}", self.name)))),
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
        Ok(path_method(receiver, name, &args))
    }

    fn constant(&self, name: &str) -> Option<Value> {
        match name {
            "HOMEBREW_PREFIX" => Some(Value::Path("$HOMEBREW_PREFIX".into())),
            "HOMEBREW_CELLAR" => Some(Value::Path("$HOMEBREW_CELLAR".into())),
            // Homebrew's own download strategies, by their `using:` names.
            "CurlDownloadStrategy" => Some(Value::Sym("curl".into())),
            "NoUnzipCurlDownloadStrategy" => Some(Value::Sym("nounzip".into())),
            "HomebrewCurlDownloadStrategy" => Some(Value::Sym("homebrew_curl".into())),
            "GitDownloadStrategy" => Some(Value::Sym("git".into())),
            _ => None,
        }
    }

    fn define(
        &mut self,
        ev: &mut Eval<'pr>,
        def: ruby_prism::DefNode<'pr>,
        at: usize,
    ) -> Result<bool> {
        match def.name().as_slice() {
            b"install" => self.install_at = Some(at),
            b"post_install" => self.post_install_at = Some(at),
            // Other methods, `caveats` among them, run when called: after
            // the class body for `caveats`, which may use ones defined later.
            name => {
                let name = String::from_utf8_lossy(name).into_owned();
                ev.defs.insert(name, def);
            }
        }
        Ok(true)
    }

    fn ignores(&self, name: &str) -> bool {
        IGNORED.contains(&name) || matches!(name, "include" | "extend")
    }
}

/// Whether the `on_macos` / `on_arm` / `on_sonoma :or_newer` block `name`
/// applies to `platform`; `None` for a name that is not one.
pub(super) fn on_system(platform: Platform, name: &str, args: &Args) -> Option<bool> {
    match name {
        "on_macos" => Some(platform.macos.is_some()),
        "on_linux" => Some(platform.macos.is_none()),
        "on_arm" => Some(platform.arm),
        "on_intel" => Some(!platform.arm),
        "on_system" => {
            let linux = platform.macos.is_none();
            let mac = args.keyword("macos").map(Value::to_s).is_some_and(|spec| {
                let (codename, condition) = match spec.split_once("_or_") {
                    Some((c, rest)) => (c.to_string(), Some(format!("or_{rest}"))),
                    None => (spec.clone(), None),
                };
                on_macos_version(&platform, &codename, condition.as_deref()).unwrap_or(false)
            });
            Some(linux && args.positional.iter().any(|a| a.to_s() == "linux") || mac)
        }
        _ => {
            let codename = name.strip_prefix("on_")?;
            let condition = args.first().map(Value::to_s);
            on_macos_version(&platform, codename, condition.as_deref())
        }
    }
}

/// `Pathname` methods that only build paths.
pub fn path_method(receiver: &Value, name: &str, args: &Args) -> Option<Value> {
    let Value::Path(path) = receiver else {
        return match (receiver, name) {
            (Value::FormulaRef(formula), n) if n == "opt_prefix" || n.starts_with("opt_") => {
                let base = format!("$HOMEBREW_PREFIX/opt/{formula}");
                let rel = n
                    .trim_start_matches("opt_prefix")
                    .trim_start_matches("opt_");
                Some(Value::Path(
                    if rel.is_empty() {
                        base
                    } else {
                        format!("{base}/{rel}")
                    }
                    .into(),
                ))
            }
            _ => None,
        };
    };
    match name {
        "/" | "+" | "join" => {
            let mut out = path.clone();
            for part in &args.positional {
                out.push(part.to_s());
            }
            Some(Value::Path(out))
        }
        "basename" => Some(Value::Path(
            path.file_name().map(Into::into).unwrap_or_default(),
        )),
        "dirname" | "parent" => Some(Value::Path(
            path.parent().map(Into::into).unwrap_or_default(),
        )),
        "to_path" => Some(Value::Str(path.to_string_lossy().into_owned())),
        _ => None,
    }
}

struct BottleDsl {
    bottle: Bottle,
}

impl<'pr> Scope<'pr> for BottleDsl {
    fn call(
        &mut self,
        _ev: &mut Eval<'pr>,
        name: &str,
        args: Args,
        _block: Option<&ruby_prism::BlockNode<'pr>>,
        line: usize,
    ) -> Result<Option<Value>> {
        match name {
            "root_url" => self.bottle.root_url = args.first_str(),
            "rebuild" => {
                self.bottle.rebuild = match args.first() {
                    Some(Value::Int(n)) => *n,
                    _ => 0,
                }
            }
            "sha256" => {
                if !args.positional.is_empty() {
                    return Err(RubyError {
                        message: "the old `sha256 \"…\" => :tag` bottle syntax is not supported"
                            .to_string(),
                        line,
                    });
                }
                let cellar = match args.keyword("cellar") {
                    Some(Value::Sym(s)) => format!(":{s}"),
                    Some(other) => other.to_s(),
                    None => default_cellar().to_string(),
                };
                let tag = args.keywords.iter().find(|(key, value)| {
                    key != "cellar"
                        && matches!(value, Value::Str(s) if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
                });
                let Some((tag, sha)) = tag else {
                    return Err(RubyError {
                        message: "a bottle `sha256` without a tag".to_string(),
                        line,
                    });
                };
                let sha = sha.to_s().to_lowercase();
                match self.bottle.files.iter_mut().find(|(t, _, _)| t == tag) {
                    Some(slot) => *slot = (tag.clone(), cellar, sha),
                    None => self.bottle.files.push((tag.clone(), cellar, sha)),
                }
            }
            _ => return Ok(None),
        }
        Ok(Some(Value::Nil))
    }
}

/// `Utils::Bottles::Tag#default_cellar` for this system.
fn default_cellar() -> &'static str {
    if cfg!(target_os = "linux") {
        "/home/linuxbrew/.linuxbrew/Cellar"
    } else if cfg!(target_arch = "aarch64") {
        "/opt/homebrew/Cellar"
    } else {
        "/usr/local/Cellar"
    }
}

#[derive(Default)]
struct ResourceDsl {
    url: Option<String>,
    sha256: Option<String>,
}

impl<'pr> Scope<'pr> for ResourceDsl {
    fn call(
        &mut self,
        ev: &mut Eval<'pr>,
        name: &str,
        args: Args,
        block: Option<&ruby_prism::BlockNode<'pr>>,
        _line: usize,
    ) -> Result<Option<Value>> {
        match name {
            "url" => self.url = args.first_str(),
            "sha256" => self.sha256 = args.first_str(),
            "mirror" | "version" | "patch" | "livecheck" => {}
            n if n.starts_with("on_") => {
                let platform = ev.platform;
                let hit = match n {
                    "on_macos" => platform.macos.is_some(),
                    "on_linux" => platform.macos.is_none(),
                    "on_arm" => platform.arm,
                    "on_intel" => !platform.arm,
                    _ => return Ok(None),
                };
                if hit && let Some(block) = block {
                    ev.exec(block.body(), self)?;
                }
            }
            _ => return Ok(None),
        }
        Ok(Some(Value::Nil))
    }
}

/// `service do ... end`, into `Homebrew::Service#to_hash`.
struct ServiceDsl<'f> {
    formula: &'f mut FormulaDsl,
    out: Map<String, Json>,
}

pub(super) fn to_json(value: &Value) -> Json {
    match value {
        Value::Nil => Json::Null,
        Value::Bool(b) => json!(b),
        Value::Int(n) => json!(n),
        Value::Float(f) => json!(f),
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Hash(pairs) => {
            Json::Object(pairs.iter().map(|(k, v)| (k.to_s(), to_json(v))).collect())
        }
        other => json!(other.to_s()),
    }
}

impl<'pr> Scope<'pr> for ServiceDsl<'_> {
    fn call(
        &mut self,
        ev: &mut Eval<'pr>,
        name: &str,
        args: Args,
        block: Option<&ruby_prism::BlockNode<'pr>>,
        line: usize,
    ) -> Result<Option<Value>> {
        let first = args.first().cloned().unwrap_or(Value::Nil);
        let value = match name {
            "run" if !args.keywords.is_empty() => Json::Object(
                args.keywords
                    .iter()
                    .map(|(k, v)| (k.clone(), to_json(v)))
                    .collect(),
            ),
            "run" if args.positional.len() > 1 => {
                Json::Array(args.positional.iter().map(to_json).collect())
            }
            "keep_alive" => match &first {
                Value::Bool(true) => json!({"always": true}),
                Value::Bool(false) => return Ok(Some(Value::Nil)),
                _ if !args.keywords.is_empty() => Json::Object(
                    args.keywords
                        .iter()
                        .map(|(k, v)| (k.clone(), to_json(v)))
                        .collect(),
                ),
                other => to_json(other),
            },
            "environment_variables" => match &first {
                Value::Hash(pairs) => Json::Object(
                    pairs
                        .iter()
                        .map(|(k, v)| (k.to_s(), json!(v.to_s())))
                        .collect(),
                ),
                _ => Json::Object(
                    args.keywords
                        .iter()
                        .map(|(k, v)| (k.clone(), json!(v.to_s())))
                        .collect(),
                ),
            },
            "name" => Json::Object(
                args.keywords
                    .iter()
                    .map(|(k, v)| (k.clone(), to_json(v)))
                    .collect(),
            ),
            "run"
            | "run_type"
            | "interval"
            | "cron"
            | "working_dir"
            | "root_dir"
            | "input_path"
            | "log_path"
            | "error_log_path"
            | "restart_delay"
            | "throttle_interval"
            | "stop_timeout"
            | "nice"
            | "process_type"
            | "macos_legacy_timers"
            | "launch_only_once"
            | "require_root"
            | "sockets" => match &first {
                Value::Sym(s) => json!(s),
                other => to_json(other),
            },
            _ => return self.formula.call(ev, name, args, block, line),
        };
        if !value.is_null() {
            self.out.insert(name.to_string(), value);
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
        Ok(path_method(receiver, name, &args))
    }

    fn constant(&self, name: &str) -> Option<Value> {
        self.formula.constant(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_ARM: Platform = Platform {
        macos: Some((26, 0)),
        arm: true,
    };

    fn read(source: &str) -> TapFormula {
        read_formula(
            source.as_bytes(),
            "foo",
            &TapName::parse("me/tools").unwrap(),
            MAC_ARM,
        )
        .unwrap()
    }

    #[test]
    fn reads_a_bottled_formula() {
        let f = read(
            r#"
class Foo < Formula
  desc "A tool"
  homepage "https://example.com"
  url "https://example.com/foo-1.2.3.tar.gz"
  sha256 "aaaa"
  revision 1
  bottle do
    root_url "https://ghcr.io/v2/me/homebrew-tools"
    rebuild 2
    sha256 cellar: :any, arm64_sequoia: "0000000000000000000000000000000000000000000000000000000000000000"
    sha256               sonoma:        "1111111111111111111111111111111111111111111111111111111111111111"
  end
  keg_only :versioned_formula
  depends_on "pkgconf" => :build
  depends_on "openssl@3"
  depends_on macos: :sonoma
  uses_from_macos "zlib"
  deprecate! date: "2000-01-01", because: :unmaintained
  def caveats
    "Run #{opt_bin}/foo"
  end
end
"#,
        );
        let j = &f.json;
        assert_eq!(j["versions"]["stable"], "1.2.3");
        assert_eq!(j["revision"], 1);
        assert_eq!(j["full_name"], "me/tools/foo");
        let file = &j["bottle"]["stable"]["files"]["arm64_sequoia"];
        assert_eq!(file["cellar"], ":any");
        assert_eq!(
            file["url"],
            "https://ghcr.io/v2/me/tools/foo/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            j["bottle"]["stable"]["files"]["sonoma"]["cellar"],
            "/opt/homebrew/Cellar"
        );
        assert_eq!(j["dependencies"], json!(["openssl@3"]));
        assert_eq!(j["build_dependencies"], json!(["pkgconf"]));
        assert_eq!(j["requirements"][0]["version"], "14");
        assert_eq!(j["keg_only_reason"]["reason"], ":versioned_formula");
        assert_eq!(j["deprecated"], true);
        assert_eq!(j["deprecation_reason"], "unmaintained");
        assert_eq!(j["caveats"], "Run $HOMEBREW_PREFIX/opt/foo/bin/foo");
        let formula: nb_core::Formula = serde_json::from_value(f.json.clone()).unwrap();
        assert_eq!(formula.effective_version(), "1.2.3_1");
    }

    #[test]
    fn follows_the_system_like_goreleaser_formulae() {
        let f = read(
            r#"
class Foo < Formula
  version "2.9.6"
  on_macos do
    if Hardware::CPU.intel?
      url "https://example.com/foo_#{version}_darwin_amd64.tar.gz"
      sha256 "intel"
      define_method(:install) do
        bin.install "foo"
      end
    end
    if Hardware::CPU.arm?
      url "https://example.com/foo_#{version}_darwin_arm64.tar.gz"
      sha256 "arm"
      define_method(:install) do
        bin.install "foo"
      end
    end
  end
  on_linux do
    url "https://example.com/foo_linux.tar.gz"
  end
  on_sonoma :or_newer do
    depends_on "newer"
  end
  on_ventura :or_older do
    depends_on "older"
  end
end
"#,
        );
        assert_eq!(
            f.json["urls"]["stable"]["url"],
            "https://example.com/foo_2.9.6_darwin_arm64.tar.gz"
        );
        assert_eq!(f.json["urls"]["stable"]["checksum"], "arm");
        assert!(f.install_at.is_some());
        assert_eq!(f.json["dependencies"], json!(["newer"]));
        assert_eq!(f.json["bottle"], json!({}));
    }

    #[test]
    fn reads_the_version_from_the_url_and_stops_at_unknown_ruby() {
        let f = read("class Foo < Formula\n  url \"https://example.com/foo-0.9.8s.tar.gz\"\nend\n");
        assert_eq!(f.json["versions"]["stable"], "0.9.8s");
        let err = read_formula(
            b"class Foo < Formula\n  url \"x\"\n  something_new 1\nend\n",
            "foo",
            &TapName::parse("me/tools").unwrap(),
            MAC_ARM,
        )
        .unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("something_new"), "{err}");
        let err = read_formula(
            b"class Foo < Formula\n  on_linux do\n  end\n  odie \"Unsupported platform\" if OS.mac?\nend\n",
            "foo",
            &TapName::parse("me/tools").unwrap(),
            MAC_ARM,
        )
        .unwrap_err();
        assert_eq!(err.message, "Unsupported platform");
    }

    #[test]
    fn reads_post_install_steps() {
        let source = br##"
class Foo < Formula
  url "https://example.com/foo-8.4.1.tar.gz"
  sha256 "abc"

  post_install_steps do
    mkdir_p "lib/foo", base: :homebrew_prefix
    unless_path_exists "pecl", base: :prefix do
      symlink "lib/foo", "pecl", source_base: :homebrew_prefix
    end
    on_macos do
      run "foo", base: :bin, args: ["set", "{{etc}}/foo/#{version.major_minor}"], print_stdout: true
    end
    inreplace "foo.conf", %r{{{HOMEBREW_CELLAR}}/foo/[^/]}, "{{opt_prefix}}", base: :etc
    copy "a", "b"
    set_permissions "var/foo", "0755", recursive: false
  end

  def caveats
    "config in #{etc}/foo/#{short_version}"
  end

  def short_version
    version.to_s.split(".")[0..1].join(".")
  end
end
"##;
        let f = read_formula(source, "foo", &TapName::parse("me/tools").unwrap(), MAC_ARM).unwrap();
        let steps = &f.json["post_install_steps"];
        assert_eq!(
            steps[0],
            json!({"type": "mkdir_p", "path": {"base": "homebrew_prefix", "path": "lib/foo"}})
        );
        assert_eq!(
            steps[1],
            json!({
                "type": "symlink",
                "guards": [{"base": "prefix", "condition": "unless_exists", "id": "1", "path": "pecl"}],
                "source": {"base": "homebrew_prefix", "path": "lib/foo"},
                "target": {"base": "prefix", "path": "pecl"},
            })
        );
        assert_eq!(
            steps[2],
            json!({
                "type": "run",
                "guards": [{"condition": "on", "value": "macos", "id": "2"}],
                "command": {"base": "bin", "path": "foo"},
                "args": ["set", "{{etc}}/foo/{{version.major_minor}}"],
                "print_stdout": true,
            })
        );
        assert_eq!(
            steps[3],
            json!({
                "type": "inreplace",
                "path": {"base": "etc", "path": "foo.conf"},
                "before": "{{HOMEBREW_CELLAR}}/foo/[^/]",
                "after": "{{opt_prefix}}",
                "regexp": true,
            })
        );
        assert_eq!(
            steps[4],
            json!({
                "type": "copy",
                "source": {"base": "prefix", "path": "a"},
                "target": {"base": "prefix", "path": "b"},
            })
        );
        assert_eq!(
            steps[5],
            json!({
                "type": "set_permissions",
                "paths": [{"path": "var/foo"}],
                "permissions": "0755",
                "non_recursive": true,
            })
        );
        // `caveats` may call a method defined after it.
        assert_eq!(f.json["caveats"], "config in $HOMEBREW_PREFIX/etc/foo/8.4");
    }

    #[test]
    fn evaluates_regexps_and_ranges() {
        let source = br#"
class Foo < Formula
  url "https://example.com/foo-1.2.3.tar.gz"
  desc "a-b-c".gsub(/-(\w)/, "_\\1").sub(/^a/i, "A") + "abcdef"[1..2] + [1, 2, 3][1..].join + "x"[5..].to_s
end
"#;
        let f = read_formula(source, "foo", &TapName::parse("me/tools").unwrap(), MAC_ARM).unwrap();
        assert_eq!(f.json["desc"], "A_b_cbc23");
    }

    /// Compare the steps read from each `<name>.rb` under
    /// `$NB_STEPS_COMPARE` with the formula API's, in `<name>.json`.
    #[test]
    #[ignore]
    fn post_install_steps_match_the_api() {
        let Some(root) = std::env::var_os("NB_STEPS_COMPARE") else {
            return;
        };
        let core = TapName::parse("homebrew/core").unwrap();
        let (mut same, mut differ) = (0, 0);
        let mut files: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "rb"))
            .collect();
        files.sort();
        for file in files {
            let name = file.file_stem().unwrap().to_string_lossy().into_owned();
            let expected: Json =
                serde_json::from_slice(&std::fs::read(file.with_extension("json")).unwrap())
                    .unwrap();
            let source = std::fs::read(&file).unwrap();
            match read_formula(&source, &name, &core, MAC_ARM) {
                Ok(f) if f.json["post_install_steps"] == expected => same += 1,
                Ok(f) => {
                    differ += 1;
                    println!(
                        "{name}:\n  ours: {}\n   api: {expected}",
                        f.json["post_install_steps"]
                    );
                }
                Err(e) => {
                    differ += 1;
                    println!("{name}: line {}: {}", e.line, e.message);
                }
            }
        }
        println!("same {same}, differ {differ}");
    }

    /// Read every formula of the taps under `$NB_TAP_SURVEY` (one
    /// `user--repo` directory per tap) and count what could not be read.
    #[test]
    #[ignore]
    fn survey_real_taps() {
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
            let mut files = Vec::new();
            for sub in ["Formula", "HomebrewFormula"] {
                let dir = tap_dir.path().join(sub);
                if dir.is_dir() {
                    files = super::super::super::walkdir(&dir);
                    break;
                }
            }
            for file in files
                .into_iter()
                .filter(|f| f.extension().is_some_and(|e| e == "rb"))
            {
                let name = file.file_stem().unwrap().to_string_lossy().into_owned();
                let source = std::fs::read(&file).unwrap();
                match read_formula(&source, &name, &tap, MAC_ARM) {
                    Ok(f) => match serde_json::from_value::<nb_core::Formula>(f.json.clone()) {
                        Ok(_) => ok += 1,
                        Err(e) => failures
                            .entry(format!("does not deserialize: {e}"))
                            .or_default()
                            .push(format!("{tap}/{name}")),
                    },
                    Err(e) => {
                        let key = e.message.lines().next().unwrap_or_default().to_string();
                        failures
                            .entry(key)
                            .or_default()
                            .push(format!("{tap}/{name}:{}", e.line));
                    }
                }
            }
        }
        let failed: usize = failures.values().map(Vec::len).sum();
        println!("read {ok}, failed {failed}");
        let mut by_count: Vec<_> = failures.into_iter().collect();
        by_count.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        for (reason, names) in by_count.iter().take(40) {
            println!(
                "{:>4}  {reason}  e.g. {}",
                names.len(),
                names.iter().take(2).cloned().collect::<Vec<_>>().join(", ")
            );
        }
    }

    #[test]
    fn reads_services() {
        let f = read(
            r#"
class Foo < Formula
  url "https://example.com/foo-1.0.tar.gz"
  service do
    run [opt_bin/"foo", "serve"]
    keep_alive true
    log_path var/"log/foo.log"
    environment_variables PATH: std_service_path_env
  end
end
"#
            .replace("std_service_path_env", "\"/usr/bin\"")
            .as_str(),
        );
        let service = &f.json["service"];
        assert_eq!(
            service["run"],
            json!(["$HOMEBREW_PREFIX/opt/foo/bin/foo", "serve"])
        );
        assert_eq!(service["keep_alive"], json!({"always": true}));
        assert_eq!(service["log_path"], "$HOMEBREW_PREFIX/var/log/foo.log");
        assert_eq!(service["environment_variables"]["PATH"], "/usr/bin");
    }
}
