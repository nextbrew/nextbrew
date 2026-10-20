//! Evaluating the part of Ruby that formula and cask files use: literals,
//! string interpolation, conditionals on the system, local variables and
//! constants, and method calls nextbrew knows. A call it does not know is
//! an error naming it and its line, never a guess.

use std::collections::HashMap;

use ruby_prism::Node;

use super::value::{Value, codename_version, version_tokens};

/// Why a file could not be evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RubyError {
    pub message: String,
    pub line: usize,
}

impl std::fmt::Display for RubyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

pub type Result<T> = std::result::Result<T, RubyError>;

/// The system a file is evaluated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    /// macOS `(major, minor)`, or `None` on Linux.
    pub macos: Option<(u32, u32)>,
    pub arm: bool,
}

impl Platform {
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        let macos = Some((nb_core::macos_major_version().unwrap_or(15), 0));
        #[cfg(not(target_os = "macos"))]
        let macos = None;
        Self {
            macos,
            arm: matches!(std::env::consts::ARCH, "aarch64" | "arm"),
        }
    }

    /// Homebrew's `Hardware::CPU.arch` symbol.
    pub fn arch(&self) -> &'static str {
        if self.arm { "arm64" } else { "x86_64" }
    }
}

/// Arguments of a call: positional values, then `key: value` pairs.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub positional: Vec<Value>,
    pub keywords: Vec<(String, Value)>,
}

impl Args {
    pub fn first(&self) -> Option<&Value> {
        self.positional.first()
    }

    pub fn keyword(&self, key: &str) -> Option<&Value> {
        self.keywords.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn first_str(&self) -> Option<String> {
        self.first().map(Value::to_s)
    }
}

/// What a file's own methods mean: the DSL of a formula or cask class, or
/// the helpers available inside `def install`.
pub trait Scope<'pr> {
    /// A call without a receiver (`url "..."`, `bin.install ...`'s `bin`).
    /// `Ok(None)` if the scope does not know it.
    fn call(
        &mut self,
        ev: &mut Eval<'pr>,
        name: &str,
        args: Args,
        block: Option<&ruby_prism::BlockNode<'pr>>,
        line: usize,
    ) -> Result<Option<Value>>;

    /// A call on a value the evaluator does not know the method of
    /// (`bin.install`, `(bin/"x").write`). `Ok(None)` if unknown.
    fn call_on(
        &mut self,
        _ev: &mut Eval<'pr>,
        _receiver: &Value,
        _name: &str,
        _args: Args,
        _block: Option<&ruby_prism::BlockNode<'pr>>,
        _line: usize,
    ) -> Result<Option<Value>> {
        Ok(None)
    }

    /// A constant the scope defines (`HOMEBREW_PREFIX`).
    fn constant(&self, _name: &str) -> Option<Value> {
        None
    }

    /// `def name ... end`; false if the scope does not take definitions.
    fn define(
        &mut self,
        _ev: &mut Eval<'pr>,
        _def: ruby_prism::DefNode<'pr>,
        _at: usize,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Calls whose arguments do not matter and need not be understood
    /// (`license`, `include Language::Python::Virtualenv`).
    fn ignores(&self, _name: &str) -> bool {
        false
    }
}

pub struct Eval<'pr> {
    source: &'pr [u8],
    pub platform: Platform,
    pub locals: HashMap<String, Value>,
    pub constants: HashMap<String, Value>,
    /// Methods the file defines (`def php_version`), callable without
    /// arguments from what is evaluated.
    pub defs: HashMap<String, ruby_prism::DefNode<'pr>>,
    /// Whether `version` is a cask's (`Cask::DSL::Version`), whose methods
    /// differ from a formula's.
    pub cask_versions: bool,
}

fn name_of(id: ruby_prism::ConstantId<'_>) -> String {
    String::from_utf8_lossy(id.as_slice()).into_owned()
}

impl<'pr> Eval<'pr> {
    pub fn new(source: &'pr [u8], platform: Platform) -> Self {
        Self {
            source,
            platform,
            locals: HashMap::new(),
            constants: HashMap::new(),
            defs: HashMap::new(),
            cask_versions: false,
        }
    }

    pub fn line_of(&self, node: &Node<'pr>) -> usize {
        let offset = node.location().start_offset();
        1 + self.source[..offset.min(self.source.len())]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
    }

    pub fn error(&self, node: &Node<'pr>, message: impl Into<String>) -> RubyError {
        RubyError {
            message: message.into(),
            line: self.line_of(node),
        }
    }

    /// The source text of a node, for messages.
    pub fn text(&self, node: &Node<'pr>) -> String {
        let text = String::from_utf8_lossy(node.location().as_slice()).into_owned();
        let first = text.lines().next().unwrap_or_default();
        if first.len() > 60 {
            format!("{}...", &first[..60])
        } else {
            first.to_string()
        }
    }

    /// Run statements, returning the last value.
    pub fn exec(&mut self, node: Option<Node<'pr>>, scope: &mut dyn Scope<'pr>) -> Result<Value> {
        let Some(node) = node else {
            return Ok(Value::Nil);
        };
        if let Some(statements) = node.as_statements_node() {
            let mut last = Value::Nil;
            for statement in statements.body().iter() {
                last = self.eval(&statement, scope)?;
            }
            return Ok(last);
        }
        self.eval(&node, scope)
    }

    fn exec_statements(
        &mut self,
        statements: Option<ruby_prism::StatementsNode<'pr>>,
        scope: &mut dyn Scope<'pr>,
    ) -> Result<Value> {
        let mut last = Value::Nil;
        if let Some(statements) = statements {
            for statement in statements.body().iter() {
                last = self.eval(&statement, scope)?;
            }
        }
        Ok(last)
    }

    /// Run a block's body with its parameters bound to `values`.
    pub fn yield_block(
        &mut self,
        block: &ruby_prism::BlockNode<'pr>,
        values: &[Value],
        scope: &mut dyn Scope<'pr>,
    ) -> Result<Value> {
        let mut saved = Vec::new();
        if let Some(params) = block
            .parameters()
            .and_then(|p| p.as_block_parameters_node())
            .and_then(|p| p.parameters())
        {
            // A block of several parameters given one array takes its items.
            let spread;
            let values = match values {
                [Value::Array(items)] if params.requireds().iter().count() > 1 => {
                    spread = items.clone();
                    spread.as_slice()
                }
                _ => values,
            };
            for (i, param) in params.requireds().iter().enumerate() {
                if let Some(param) = param.as_required_parameter_node() {
                    let name = name_of(param.name());
                    let value = values.get(i).cloned().unwrap_or(Value::Nil);
                    saved.push((name.clone(), self.locals.insert(name, value)));
                }
            }
        }
        let result = self.exec(block.body(), scope);
        for (name, old) in saved {
            match old {
                Some(old) => self.locals.insert(name, old),
                None => self.locals.remove(&name),
            };
        }
        result
    }

    pub fn args(
        &mut self,
        arguments: Option<ruby_prism::ArgumentsNode<'pr>>,
        scope: &mut dyn Scope<'pr>,
    ) -> Result<Args> {
        let mut args = Args::default();
        let Some(arguments) = arguments else {
            return Ok(args);
        };
        for arg in arguments.arguments().iter() {
            if let Some(hash) = arg.as_keyword_hash_node() {
                for element in hash.elements().iter() {
                    let Some(assoc) = element.as_assoc_node() else {
                        return Err(self.error(&element, "`**` arguments are not supported"));
                    };
                    let key = self.eval(&assoc.key(), scope)?;
                    let value = self.eval(&assoc.value(), scope)?;
                    match key {
                        Value::Sym(key) => args.keywords.push((key, value)),
                        key => args.positional.push(Value::Hash(vec![(key, value)])),
                    }
                }
            } else if let Some(splat) = arg.as_splat_node() {
                match splat
                    .expression()
                    .map(|e| self.eval(&e, scope))
                    .transpose()?
                {
                    Some(Value::Array(items)) => args.positional.extend(items),
                    Some(other) => args.positional.push(other),
                    None => {}
                }
            } else {
                let value = self.eval(&arg, scope)?;
                args.positional.push(value);
            }
        }
        // `foo "a" => :b` is one Hash argument of string keys.
        let mut merged: Vec<Value> = Vec::new();
        for value in args.positional {
            match (merged.last_mut(), value) {
                (Some(Value::Hash(pairs)), Value::Hash(more)) => pairs.extend(more),
                (_, value) => merged.push(value),
            }
        }
        args.positional = merged;
        Ok(args)
    }

    pub fn eval(&mut self, node: &Node<'pr>, scope: &mut dyn Scope<'pr>) -> Result<Value> {
        use Value::*;
        if let Some(s) = node.as_string_node() {
            return Ok(Str(String::from_utf8_lossy(s.unescaped()).into_owned()));
        }
        if let Some(s) = node.as_interpolated_string_node() {
            let mut out = String::new();
            for part in s.parts().iter() {
                out.push_str(&self.eval(&part, scope)?.to_s());
            }
            return Ok(Str(out));
        }
        if let Some(e) = node.as_embedded_statements_node() {
            return self.exec_statements(e.statements(), scope);
        }
        if let Some(s) = node.as_symbol_node() {
            return Ok(Sym(String::from_utf8_lossy(s.unescaped()).into_owned()));
        }
        if let Some(s) = node.as_interpolated_symbol_node() {
            let mut out = String::new();
            for part in s.parts().iter() {
                out.push_str(&self.eval(&part, scope)?.to_s());
            }
            return Ok(Sym(out));
        }
        if node.as_integer_node().is_some() {
            let text = String::from_utf8_lossy(node.location().as_slice()).replace('_', "");
            return text
                .parse()
                .map(Int)
                .map_err(|_| self.error(node, format!("integer `{text}` is not supported")));
        }
        if let Some(range) = node.as_range_node() {
            let mut bound = |ev: &mut Self, part: Option<Node<'pr>>, missing: i64| -> Result<i64> {
                match part.map(|p| ev.eval(&p, scope)).transpose()? {
                    None | Some(Nil) => Ok(missing),
                    Some(Int(n)) => Ok(n),
                    Some(_) => Err(ev.error(node, "only integer ranges are supported")),
                }
            };
            let first = bound(self, range.left(), 0)?;
            let last = bound(self, range.right(), i64::MAX)?;
            return Ok(Range(first, last, range.is_exclude_end()));
        }
        if let Some(f) = node.as_float_node() {
            return Ok(Float(f.value()));
        }
        if node.as_true_node().is_some() {
            return Ok(Bool(true));
        }
        if node.as_false_node().is_some() {
            return Ok(Bool(false));
        }
        if node.as_nil_node().is_some() {
            return Ok(Nil);
        }
        if let Some(a) = node.as_array_node() {
            let mut items = Vec::new();
            for element in a.elements().iter() {
                if let Some(splat) = element.as_splat_node() {
                    if let Some(Array(more)) = splat
                        .expression()
                        .map(|e| self.eval(&e, scope))
                        .transpose()?
                    {
                        items.extend(more);
                    }
                    continue;
                }
                items.push(self.eval(&element, scope)?);
            }
            return Ok(Array(items));
        }
        if let Some(h) = node.as_hash_node() {
            let mut pairs = Vec::new();
            for element in h.elements().iter() {
                let Some(assoc) = element.as_assoc_node() else {
                    return Err(self.error(&element, "`**` in a hash is not supported"));
                };
                pairs.push((
                    self.eval(&assoc.key(), scope)?,
                    self.eval(&assoc.value(), scope)?,
                ));
            }
            return Ok(Hash(pairs));
        }
        if let Some(p) = node.as_parentheses_node() {
            return self.exec(p.body(), scope);
        }
        if let Some(a) = node.as_and_node() {
            let left = self.eval(&a.left(), scope)?;
            return if left.truthy() {
                self.eval(&a.right(), scope)
            } else {
                Ok(left)
            };
        }
        if let Some(o) = node.as_or_node() {
            let left = self.eval(&o.left(), scope)?;
            return if left.truthy() {
                Ok(left)
            } else {
                self.eval(&o.right(), scope)
            };
        }
        if let Some(i) = node.as_if_node() {
            return if self.eval(&i.predicate(), scope)?.truthy() {
                self.exec_statements(i.statements(), scope)
            } else {
                match i.subsequent() {
                    Some(next) => self.eval(&next, scope),
                    None => Ok(Nil),
                }
            };
        }
        if let Some(e) = node.as_else_node() {
            return self.exec_statements(e.statements(), scope);
        }
        if let Some(u) = node.as_unless_node() {
            return if !self.eval(&u.predicate(), scope)?.truthy() {
                self.exec_statements(u.statements(), scope)
            } else {
                match u.else_clause() {
                    Some(e) => self.exec_statements(e.statements(), scope),
                    None => Ok(Nil),
                }
            };
        }
        if let Some(c) = node.as_case_node() {
            let subject = c.predicate().map(|p| self.eval(&p, scope)).transpose()?;
            for condition in c.conditions().iter() {
                let Some(when) = condition.as_when_node() else {
                    continue;
                };
                for test in when.conditions().iter() {
                    let test = self.eval(&test, scope)?;
                    let hit = match &subject {
                        Some(subject) => values_equal(subject, &test),
                        None => test.truthy(),
                    };
                    if hit {
                        return self.exec_statements(when.statements(), scope);
                    }
                }
            }
            return match c.else_clause() {
                Some(e) => self.exec_statements(e.statements(), scope),
                None => Ok(Nil),
            };
        }
        if let Some(v) = node.as_local_variable_read_node() {
            let name = name_of(v.name());
            return Ok(self.locals.get(&name).cloned().unwrap_or(Nil));
        }
        if let Some(w) = node.as_local_variable_write_node() {
            let value = self.eval(&w.value(), scope)?;
            self.locals.insert(name_of(w.name()), value.clone());
            return Ok(value);
        }
        if let Some(w) = node.as_constant_write_node() {
            let value = self.eval(&w.value(), scope)?;
            self.constants.insert(name_of(w.name()), value.clone());
            return Ok(value);
        }
        if let Some(c) = node.as_constant_read_node() {
            let name = name_of(c.name());
            return self.constant(node, &name, scope);
        }
        if node.as_constant_path_node().is_some() {
            // `A::B::C` written out; only constant names may appear in it.
            let text = String::from_utf8_lossy(node.location().as_slice()).into_owned();
            let path = text.trim_start_matches("::");
            if !path
                .split("::")
                .all(|part| part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            {
                return Err(self.error(node, format!("`{}` is not supported", self.text(node))));
            }
            return self.constant(node, path, scope);
        }
        if let Some(call) = node.as_call_node() {
            return self.call(node, &call, scope);
        }
        if let Some(def) = node.as_def_node() {
            let at = node.location().start_offset();
            let name = name_of(def.name());
            if scope.define(self, def, at)? {
                return Ok(Value::Sym(name));
            }
            return Err(self.error(node, format!("`def {name}` is not supported")));
        }
        if node.as_self_node().is_some() {
            return Ok(Module("self".to_string()));
        }
        if let Some(r) = node.as_regular_expression_node() {
            // The flags follow the closing delimiter: `/a/i`, `%r{a}mx`.
            let text = self.text(node);
            let flags = &text[text.trim_end_matches(char::is_alphabetic).len()..];
            let options = flags.chars().fold(0, |options, flag| {
                options
                    | match flag {
                        'i' => 1,
                        'x' => 2,
                        'm' => 4,
                        _ => 0,
                    }
            });
            let source = String::from_utf8_lossy(r.unescaped()).into_owned();
            return Ok(Regexp(source, options));
        }
        Err(self.error(node, format!("`{}` is not supported", self.text(node))))
    }

    fn constant(&self, node: &Node<'pr>, name: &str, scope: &mut dyn Scope<'pr>) -> Result<Value> {
        if let Some(value) = self.constants.get(name) {
            return Ok(value.clone());
        }
        if let Some(value) = scope.constant(name) {
            return Ok(value);
        }
        match name {
            "OS" | "Hardware" | "Hardware::CPU" | "MacOS" | "OS::Mac" | "Formula" | "ENV"
            | "File" | "Dir" | "Utils" => Ok(Value::Module(name.to_string())),
            _ => Err(self.error(node, format!("the constant `{name}` is not supported"))),
        }
    }

    fn call(
        &mut self,
        node: &Node<'pr>,
        call: &ruby_prism::CallNode<'pr>,
        scope: &mut dyn Scope<'pr>,
    ) -> Result<Value> {
        let name = name_of(call.name());
        let line = self.line_of(node);
        let block_node = call.block();
        let block = block_node.as_ref().and_then(|b| b.as_block_node());
        if block_node.is_some() && block.is_none() {
            return Err(self.error(node, "`&` block arguments are not supported"));
        }
        let Some(receiver) = call.receiver() else {
            // A local variable read Prism could not tell from a call.
            if call.arguments().is_none()
                && block.is_none()
                && let Some(value) = self.locals.get(&name)
            {
                return Ok(value.clone());
            }
            if scope.ignores(&name) {
                return Ok(Value::Nil);
            }
            let args = self.args(call.arguments(), scope)?;
            let no_args = args.positional.is_empty() && args.keywords.is_empty();
            return match scope.call(self, &name, args, block.as_ref(), line)? {
                Some(value) => Ok(value),
                None if no_args && block.is_none() && self.defs.contains_key(&name) => {
                    self.call_def(node, &name, scope)
                }
                None => Err(self.error(node, format!("the method `{name}` is not supported"))),
            };
        };
        // `a && b` style operators and comparisons evaluate both sides.
        let receiver_value = self.eval(&receiver, scope)?;
        let args = self.args(call.arguments(), scope)?;
        if let Some(value) = self.builtin(&receiver_value, &name, &args, block.as_ref(), scope)? {
            return Ok(value);
        }
        match scope.call_on(self, &receiver_value, &name, args, block.as_ref(), line)? {
            Some(value) => Ok(value),
            None => Err(self.error(
                node,
                format!(
                    "the method `{name}` on {} is not supported",
                    receiver_value.type_name()
                ),
            )),
        }
    }

    /// Run the file's own method `name`, which takes no arguments.
    pub fn call_def(
        &mut self,
        node: &Node<'pr>,
        name: &str,
        scope: &mut dyn Scope<'pr>,
    ) -> Result<Value> {
        // Taken out while it runs, so a method calling itself is an error
        // rather than endless.
        let Some(def) = self.defs.remove(name) else {
            return Err(self.error(node, format!("the method `{name}` is not supported")));
        };
        let result = if def.parameters().is_some() {
            Err(self.error(node, format!("`{name}` with parameters is not supported")))
        } else {
            self.exec(def.body(), scope)
        };
        self.defs.insert(name.to_string(), def);
        result
    }

    /// Methods on values that mean the same everywhere.
    fn builtin(
        &mut self,
        receiver: &Value,
        name: &str,
        args: &Args,
        block: Option<&ruby_prism::BlockNode<'pr>>,
        scope: &mut dyn Scope<'pr>,
    ) -> Result<Option<Value>> {
        use Value::*;
        let arg = args.first();
        let platform = self.platform;
        if self.cask_versions
            && let Version(v) = receiver
            && let Some(value) = super::value::cask_version_method(v, name, args)
        {
            return Ok(Some(value));
        }
        let value = match (receiver, name) {
            // The system.
            (Module(m), "mac?") if m == "OS" => Bool(platform.macos.is_some()),
            (Module(m), "linux?") if m == "OS" => Bool(platform.macos.is_none()),
            (Module(m), "arm?") if m == "Hardware::CPU" => Bool(platform.arm),
            (Module(m), "intel?") if m == "Hardware::CPU" => Bool(!platform.arm),
            (Module(m), "is_64_bit?") if m == "Hardware::CPU" => Bool(true),
            (Module(m), "arch") if m == "Hardware::CPU" => Sym(platform.arch().to_string()),
            (Module(m), "type") if m == "Hardware::CPU" => {
                Sym(if platform.arm { "arm" } else { "intel" }.to_string())
            }
            (Module(m), "version") if m == "MacOS" || m == "OS::Mac" => match platform.macos {
                Some((major, minor)) => MacOs(major, minor),
                None => return Ok(None),
            },
            (Module(m), "head?" | "with?" | "bottle?") if m == "build" => Bool(false),
            (Module(m), "stable?" | "without?") if m == "build" => Bool(true),
            (Module(m), "[]") if m == "Formula" => {
                FormulaRef(arg.map(Value::to_s).unwrap_or_default())
            }
            (Module(m), "[]") if m == "ENV" => {
                let key = arg.map(Value::to_s).unwrap_or_default();
                std::env::var(key).map(Str).unwrap_or(Nil)
            }
            (Module(m), "fetch") if m == "ENV" => {
                let key = arg.map(Value::to_s).unwrap_or_default();
                match std::env::var(key) {
                    Ok(value) => Str(value),
                    Err(_) => args.positional.get(1).cloned().unwrap_or(Nil),
                }
            }
            // Comparisons.
            (_, "==") => Bool(values_equal(receiver, arg.unwrap_or(&Nil))),
            (_, "!=") => Bool(!values_equal(receiver, arg.unwrap_or(&Nil))),
            (_, "!") => Bool(!receiver.truthy()),
            (_, "nil?") => Bool(matches!(receiver, Nil)),
            (MacOs(major, minor), op @ (">=" | ">" | "<=" | "<")) => {
                let Some(other) = arg.and_then(Value::macos_version) else {
                    return Ok(None);
                };
                Bool(compare(op, (*major, *minor).cmp(&other)))
            }
            (Int(a), op @ (">=" | ">" | "<=" | "<")) => match arg {
                Some(Int(b)) => Bool(compare(op, a.cmp(b))),
                _ => return Ok(None),
            },
            // Versions.
            (Version(v), "major" | "minor" | "patch") => {
                let index = ["major", "minor", "patch"]
                    .iter()
                    .position(|n| *n == name)
                    .unwrap_or(0);
                version_tokens(v)
                    .get(index)
                    .cloned()
                    .map(Version)
                    .unwrap_or(Nil)
            }
            (Version(v), "major_minor" | "major_minor_patch") => {
                let n = if name == "major_minor" { 2 } else { 3 };
                Version(
                    version_tokens(v)
                        .into_iter()
                        .take(n)
                        .collect::<Vec<_>>()
                        .join("."),
                )
            }
            (Version(v), "csv") => Array(v.split(',').map(|s| Version(s.to_string())).collect()),
            (Version(v), "before_comma") => {
                Version(v.split(',').next().unwrap_or_default().to_string())
            }
            (Version(v), "after_comma") => Version(
                v.split_once(',')
                    .map(|(_, b)| b)
                    .unwrap_or_default()
                    .to_string(),
            ),
            (Version(v), "no_dots") => Version(v.replace('.', "")),
            (Version(v), "dots_to_underscores") => Version(v.replace('.', "_")),
            (Version(v), "dots_to_hyphens") => Version(v.replace('.', "-")),
            (Version(v), "to_i") => Int(version_tokens(v)
                .first()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0)),
            // Strings and symbols.
            (_, "to_s" | "to_str") => Str(receiver.to_s()),
            (_, "to_sym") => Sym(receiver.to_s()),
            (_, "inspect") => Str(receiver.inspect()),
            // Constants are often frozen; values here never change anyway.
            (value, "freeze" | "dup") => value.clone(),
            (Str(s), "to_i") => Int(s.trim().parse().unwrap_or(0)),
            (Str(_) | Version(_), "+") => Str(format!(
                "{}{}",
                receiver.to_s(),
                arg.map(Value::to_s).unwrap_or_default()
            )),
            (Str(s) | Version(s), "include?") => {
                Bool(s.contains(&arg.map(Value::to_s).unwrap_or_default()))
            }
            (Str(s) | Version(s), "start_with?") => {
                Bool(args.positional.iter().any(|a| s.starts_with(&a.to_s())))
            }
            (Str(s) | Version(s), "end_with?") => {
                Bool(args.positional.iter().any(|a| s.ends_with(&a.to_s())))
            }
            (Str(s), "downcase") => Str(s.to_lowercase()),
            (Str(s), "upcase") => Str(s.to_uppercase()),
            (Str(s), "strip") => Str(s.trim().to_string()),
            (Str(s), "chomp") => Str(s.strip_suffix('\n').unwrap_or(s).to_string()),
            (Str(s), "empty?") => Bool(s.is_empty()),
            (Str(s) | Version(s), "length" | "size") => Int(s.chars().count() as i64),
            (Str(s), "delete_prefix") => Str(s
                .strip_prefix(&arg.map(Value::to_s).unwrap_or_default())
                .unwrap_or(s)
                .to_string()),
            (Str(s), "delete_suffix") => Str(s
                .strip_suffix(&arg.map(Value::to_s).unwrap_or_default())
                .unwrap_or(s)
                .to_string()),
            (Str(s) | Version(s), "split") => {
                let sep = arg.map(Value::to_s).unwrap_or_else(|| " ".to_string());
                Array(s.split(sep.as_str()).map(|p| Str(p.to_string())).collect())
            }
            (Str(s) | Version(s), "ljust" | "rjust") => {
                let width = match arg {
                    Some(Int(n)) => *n as usize,
                    _ => return Ok(None),
                };
                let pad: Vec<char> = args
                    .positional
                    .get(1)
                    .map(Value::to_s)
                    .unwrap_or_else(|| " ".to_string())
                    .chars()
                    .collect();
                let len = s.chars().count();
                if pad.is_empty() || len >= width {
                    Str(s.clone())
                } else {
                    let fill: String = pad.iter().cycle().take(width - len).collect();
                    Str(if name == "ljust" {
                        format!("{s}{fill}")
                    } else {
                        format!("{fill}{s}")
                    })
                }
            }
            (Str(s) | Version(s), "tr") => {
                let from: Vec<char> = arg.map(Value::to_s).unwrap_or_default().chars().collect();
                let to: Vec<char> = args
                    .positional
                    .get(1)
                    .map(Value::to_s)
                    .unwrap_or_default()
                    .chars()
                    .collect();
                Str(s
                    .chars()
                    .map(|c| match from.iter().position(|f| *f == c) {
                        Some(i) => *to.get(i).or(to.last()).unwrap_or(&c),
                        None => c,
                    })
                    .collect())
            }
            (Str(s) | Version(s), "sub" | "gsub")
                if args.positional.len() == 2 && matches!(args.positional[0], Regexp(..)) =>
            {
                let Some(pattern) = args.positional[0].to_regex() else {
                    return Ok(None);
                };
                // Ruby's `\\1` back-references are `${1}` to the `regex` crate.
                let to = args.positional[1].to_s().replace('$', "$$");
                let to = backreference_to_regex(&to);
                Str(if name == "sub" {
                    pattern.replacen(s, 1, to.as_str()).into_owned()
                } else {
                    pattern.replace_all(s, to.as_str()).into_owned()
                })
            }
            (Str(s) | Version(s), "sub" | "gsub") if args.positional.len() == 2 => {
                let from = args.positional[0].to_s();
                let to = args.positional[1].to_s();
                Str(if name == "sub" {
                    s.replacen(&from, &to, 1)
                } else {
                    s.replace(&from, &to)
                })
            }
            // Arrays and hashes.
            (Array(items), "first") => items.first().cloned().unwrap_or(Nil),
            (Array(items), "last") => items.last().cloned().unwrap_or(Nil),
            // Active Support's.
            (Array(items), "second" | "third" | "fourth" | "fifth") => {
                let index = ["second", "third", "fourth", "fifth"]
                    .iter()
                    .position(|n| *n == name)
                    .unwrap_or(0)
                    + 1;
                items.get(index).cloned().unwrap_or(Nil)
            }
            (Array(items), "empty?") => Bool(items.is_empty()),
            (Array(items), "any?") if block.is_none() => Bool(!items.is_empty()),
            (Array(items), "length" | "size" | "count") if block.is_none() => {
                Int(items.len() as i64)
            }
            (Array(items), "include?") => {
                Bool(items.iter().any(|i| values_equal(i, arg.unwrap_or(&Nil))))
            }
            (Array(items), "join") => Str(items
                .iter()
                .map(Value::to_s)
                .collect::<Vec<_>>()
                .join(&arg.map(Value::to_s).unwrap_or_default())),
            (Array(items), "compact") => Array(
                items
                    .iter()
                    .filter(|i| !matches!(i, Nil))
                    .cloned()
                    .collect(),
            ),
            (Array(items), "flatten") => Array(flatten(items)),
            (Array(items), "[]") => match arg {
                Some(Int(i)) => {
                    let i = if *i < 0 { items.len() as i64 + i } else { *i };
                    items.get(i as usize).cloned().unwrap_or(Nil)
                }
                Some(range @ Range(..)) => match range.range_bounds(items.len()) {
                    Some(bounds) => Array(items[bounds].to_vec()),
                    None => Nil,
                },
                _ => return Ok(None),
            },
            (Str(s), "[]") if matches!(arg, Some(Range(..))) => {
                let chars: Vec<char> = s.chars().collect();
                match arg.unwrap().range_bounds(chars.len()) {
                    Some(bounds) => Str(chars[bounds].iter().collect()),
                    None => Nil,
                }
            }
            (Range(first, last, exclusive), "to_a") if *last != i64::MAX => {
                let end = if *exclusive { *last } else { *last + 1 };
                Array((*first..end).map(Int).collect())
            }
            // `each_with_index` without a block, as `map` then sees it.
            (Array(items), "each_with_index") if block.is_none() => Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| Array(vec![item.clone(), Int(i as i64)]))
                    .collect(),
            ),
            (Array(items), "each" | "map" | "select" | "reject" | "each_with_index")
                if block.is_some() =>
            {
                let block = block.unwrap();
                let mut out = Vec::new();
                for (i, item) in items.iter().enumerate() {
                    let values = if name == "each_with_index" {
                        vec![item.clone(), Int(i as i64)]
                    } else {
                        vec![item.clone()]
                    };
                    let result = self.yield_block(block, &values, scope)?;
                    match name {
                        "map" => out.push(result),
                        "select" if result.truthy() => out.push(item.clone()),
                        "reject" if !result.truthy() => out.push(item.clone()),
                        _ => {}
                    }
                }
                match name {
                    "each" | "each_with_index" => receiver.clone(),
                    _ => Array(out),
                }
            }
            (Hash(pairs), "[]" | "fetch") => pairs
                .iter()
                .find(|(k, _)| values_equal(k, arg.unwrap_or(&Nil)))
                .map(|(_, v)| v.clone())
                .or_else(|| args.positional.get(1).cloned())
                .unwrap_or(Nil),
            (Hash(pairs), "key?") => Bool(
                pairs
                    .iter()
                    .any(|(k, _)| values_equal(k, arg.unwrap_or(&Nil))),
            ),
            _ => return Ok(None),
        };
        Ok(Some(value))
    }
}

/// A Ruby replacement string's `\\1` and `\\0` as `${1}` and `${0}`.
fn backreference_to_regex(to: &str) -> String {
    let mut out = String::new();
    let mut chars = to.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(d)) if d.is_ascii_digit() => {
                out.push_str(&format!("${{{d}}}"));
                chars.next();
            }
            ('\\', Some('\\')) => {
                out.push('\\');
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

fn compare(op: &str, ordering: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        ">=" => ordering != Less,
        ">" => ordering == Greater,
        "<=" => ordering != Greater,
        _ => ordering == Less,
    }
}

fn flatten(items: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Value::Array(inner) => out.extend(flatten(inner)),
            other => out.push(other.clone()),
        }
    }
    out
}

/// `==`, with `MacOS.version == :sonoma` and versions against strings.
pub fn values_equal(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (MacOs(..), other) | (other, MacOs(..)) => {
            a.macos_version().is_some() && a.macos_version() == other.macos_version()
        }
        (Version(x), Str(y)) | (Str(x), Version(y)) => x == y,
        (Sym(x), Sym(y)) => x == y,
        _ => a == b,
    }
}

/// `on_<codename>` with an optional `:or_newer` / `:or_older`.
pub fn on_macos_version(
    platform: &Platform,
    codename: &str,
    condition: Option<&str>,
) -> Option<bool> {
    let wanted = codename_version(codename)?;
    let Some(current) = platform.macos else {
        return Some(false);
    };
    Some(match condition {
        Some("or_newer") => current >= wanted,
        Some("or_older") => current <= wanted,
        _ => current == wanted,
    })
}
