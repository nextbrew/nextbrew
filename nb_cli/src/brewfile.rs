//! Reading a Brewfile without Ruby. A Brewfile is Ruby evaluated by
//! `Homebrew::Bundle::Dsl`; nextbrew reads the declarative part of it: one
//! call per statement to a known directive (`brew "jq", link: false`), with
//! string, symbol, boolean, number, array and hash literals as arguments.
//! Anything else (conditions, interpolation, other methods) is refused with
//! the line it is on, rather than guessed at.

use std::fmt;

/// A literal argument or option value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Sym(String),
    Bool(bool),
    Int(i64),
    Nil,
    Array(Vec<Value>),
    Hash(Vec<(String, Value)>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Sym(s) => Some(s),
            _ => None,
        }
    }

    /// Ruby truthiness: everything but `nil` and `false`.
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => write!(f, "{s:?}"),
            Value::Sym(s) => write!(f, ":{s}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Nil => write!(f, "nil"),
            Value::Array(items) => {
                let items: Vec<String> = items.iter().map(ToString::to_string).collect();
                write!(f, "[{}]", items.join(", "))
            }
            Value::Hash(pairs) => {
                let pairs: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                write!(f, "{{ {} }}", pairs.join(", "))
            }
        }
    }
}

/// The kinds of Brewfile entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tap,
    Brew,
    Cask,
    /// A package manager other than Homebrew (`mas`, `vscode`, `cargo`, ...).
    Extension(&'static str),
}

/// The directives `brew bundle` knows besides `tap`, `brew`, `cask` and
/// `cask_args`.
const EXTENSIONS: &[&str] = &[
    "mas", "vscode", "go", "cargo", "uv", "flatpak", "npm", "krew", "winget",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: Kind,
    /// The name as `Homebrew::Bundle::Dsl` sanitises it.
    pub name: String,
    pub options: Vec<(String, Value)>,
    pub line: usize,
}

impl Entry {
    pub fn option(&self, key: &str) -> Option<&Value> {
        self.options.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid Brewfile: line {}: {}", self.line, self.message)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    /// `key:` in a call or a hash.
    Label(String),
    Value(Value),
    Comma,
    Arrow,
    Open(char),
    Close(char),
    End,
}

fn tokenize(source: &str) -> Result<Vec<(Token, usize)>, ParseError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut line = 1;
    let mut i = 0;
    let err = |line: usize, message: String| ParseError { line, message };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                tokens.push((Token::End, line));
                line += 1;
                i += 1;
            }
            ';' => {
                tokens.push((Token::End, line));
                i += 1;
            }
            ' ' | '\t' | '\r' => i += 1,
            '\\' if chars.get(i + 1) == Some(&'\n') => {
                line += 1;
                i += 2;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ',' => {
                tokens.push((Token::Comma, line));
                i += 1;
            }
            '=' if chars.get(i + 1) == Some(&'>') => {
                tokens.push((Token::Arrow, line));
                i += 2;
            }
            '(' | '[' | '{' => {
                tokens.push((Token::Open(c), line));
                i += 1;
            }
            ')' | ']' | '}' => {
                tokens.push((Token::Close(c), line));
                i += 1;
            }
            '"' | '\'' => {
                let quote = c;
                let start_line = line;
                let mut text = String::new();
                i += 1;
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return Err(err(start_line, "unterminated string".into()));
                    };
                    i += 1;
                    match ch {
                        _ if ch == quote => break,
                        '\\' => {
                            let next = chars.get(i).copied().unwrap_or('\\');
                            i += 1;
                            if quote == '\'' {
                                if next != '\'' && next != '\\' {
                                    text.push('\\');
                                }
                                text.push(next);
                            } else {
                                text.push(match next {
                                    'n' => '\n',
                                    't' => '\t',
                                    other => other,
                                });
                            }
                        }
                        '#' if quote == '"' && chars.get(i) == Some(&'{') => {
                            return Err(err(line, "string interpolation is Ruby".into()));
                        }
                        '\n' => {
                            line += 1;
                            text.push(ch);
                        }
                        _ => text.push(ch),
                    }
                }
                // `"key": value` is a label too.
                if chars.get(i) == Some(&':') && chars.get(i + 1) != Some(&':') {
                    i += 1;
                    tokens.push((Token::Label(text), line));
                } else {
                    tokens.push((Token::Value(Value::Str(text)), line));
                }
            }
            ':' if chars
                .get(i + 1)
                .is_some_and(|n| n.is_ascii_alphabetic() || *n == '_' || *n == '"') =>
            {
                i += 1;
                if chars[i] == '"' {
                    let start = i + 1;
                    let end = chars[start..]
                        .iter()
                        .position(|ch| *ch == '"')
                        .ok_or_else(|| err(line, "unterminated symbol".into()))?;
                    tokens.push((
                        Token::Value(Value::Sym(chars[start..start + end].iter().collect())),
                        line,
                    ));
                    i = start + end + 1;
                } else {
                    let start = i;
                    while i < chars.len()
                        && (chars[i].is_ascii_alphanumeric() || "_?!".contains(chars[i]))
                    {
                        i += 1;
                    }
                    tokens.push((
                        Token::Value(Value::Sym(chars[start..i].iter().collect())),
                        line,
                    ));
                }
            }
            '-' | '0'..='9' if c != '-' || chars.get(i + 1).is_some_and(char::is_ascii_digit) => {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().filter(|ch| **ch != '_').collect();
                let number = text
                    .parse()
                    .map_err(|_| err(line, format!("unsupported number {text}")))?;
                tokens.push((Token::Value(Value::Int(number)), line));
            }
            _ if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || "_?!".contains(chars[i]))
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if chars.get(i) == Some(&':') && chars.get(i + 1) != Some(&':') {
                    i += 1;
                    tokens.push((Token::Label(word), line));
                    continue;
                }
                let token = match word.as_str() {
                    "true" => Token::Value(Value::Bool(true)),
                    "false" => Token::Value(Value::Bool(false)),
                    "nil" => Token::Value(Value::Nil),
                    _ => Token::Ident(word),
                };
                tokens.push((token, line));
            }
            other => {
                return Err(err(
                    line,
                    format!("unexpected `{other}`; only plain entries are read"),
                ));
            }
        }
    }
    tokens.push((Token::End, line));
    Ok(tokens)
}

/// A call's positional values and its options.
type Arguments = (Vec<Value>, Vec<(String, Value)>);

struct Parser {
    tokens: Vec<(Token, usize)>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)].0
    }

    fn line(&self) -> usize {
        self.tokens[self.pos.min(self.tokens.len() - 1)].1
    }

    fn next(&mut self) -> Token {
        let token = self.peek().clone();
        self.pos += 1;
        token
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            line: self.line(),
            message: message.into(),
        }
    }

    /// Newlines inside brackets or after a comma don't end anything.
    fn skip_ends(&mut self) {
        while *self.peek() == Token::End && self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.next() {
            Token::Value(value) => Ok(value),
            Token::Open('[') => {
                let mut items = Vec::new();
                loop {
                    self.skip_ends();
                    if *self.peek() == Token::Close(']') {
                        self.next();
                        break;
                    }
                    items.push(self.value()?);
                    self.skip_ends();
                    match self.next() {
                        Token::Comma => continue,
                        Token::Close(']') => break,
                        _ => return Err(self.error("expected `,` or `]` in an array")),
                    }
                }
                Ok(Value::Array(items))
            }
            Token::Open('{') => {
                let mut pairs = Vec::new();
                loop {
                    self.skip_ends();
                    if *self.peek() == Token::Close('}') {
                        self.next();
                        break;
                    }
                    pairs.push(self.pair()?);
                    self.skip_ends();
                    match self.next() {
                        Token::Comma => continue,
                        Token::Close('}') => break,
                        _ => return Err(self.error("expected `,` or `}` in a hash")),
                    }
                }
                Ok(Value::Hash(pairs))
            }
            Token::Ident(word) => Err(self.error(format!("`{word}` is Ruby, not a literal"))),
            _ => Err(self.error("expected a value")),
        }
    }

    /// `key: value` or `:key => value` / `"key" => value`.
    fn pair(&mut self) -> Result<(String, Value), ParseError> {
        if let Token::Label(key) = self.peek().clone() {
            self.next();
            self.skip_ends();
            return Ok((key, self.value()?));
        }
        let key = match self.value()? {
            Value::Sym(key) | Value::Str(key) => key,
            _ => return Err(self.error("expected a hash key")),
        };
        self.skip_ends();
        if self.next() != Token::Arrow {
            return Err(self.error("expected `=>` after a hash key"));
        }
        self.skip_ends();
        Ok((key, self.value()?))
    }

    /// The arguments of a call: positional values, then options.
    fn arguments(&mut self) -> Result<Arguments, ParseError> {
        let parens = *self.peek() == Token::Open('(');
        if parens {
            self.next();
        }
        let mut positional = Vec::new();
        let mut options = Vec::new();
        loop {
            if parens {
                self.skip_ends();
                if *self.peek() == Token::Close(')') {
                    self.next();
                    break;
                }
            } else if *self.peek() == Token::End {
                break;
            }
            let is_pair = matches!(self.peek(), Token::Label(_))
                || matches!(
                    self.tokens.get(self.pos + 1).map(|t| &t.0),
                    Some(Token::Arrow)
                );
            if is_pair {
                options.push(self.pair()?);
            } else {
                match self.value()? {
                    // A trailing hash literal holds the options.
                    Value::Hash(pairs) if !positional.is_empty() => options.extend(pairs),
                    value => positional.push(value),
                }
            }
            match self.peek() {
                Token::Comma => {
                    self.next();
                    self.skip_ends();
                }
                Token::Close(')') if parens => {
                    self.next();
                    break;
                }
                Token::End if !parens => break,
                _ => return Err(self.error("expected `,` between arguments")),
            }
        }
        if *self.peek() != Token::End {
            return Err(self.error("unexpected text after the entry"));
        }
        Ok((positional, options))
    }
}

/// Read `source` into entries, `cask_args` folded into each cask's `args`
/// as `Dsl#cask` does.
pub fn parse(source: &str) -> Result<Vec<Entry>, ParseError> {
    let mut parser = Parser {
        tokens: tokenize(source)?,
        pos: 0,
    };
    let mut entries = Vec::new();
    let mut cask_args: Vec<(String, Value)> = Vec::new();
    loop {
        parser.skip_ends();
        let line = parser.line();
        let directive = match parser.next() {
            Token::End => break,
            Token::Ident(word) => word,
            _ => return Err(parser.error("expected a Brewfile directive")),
        };
        let kind = match directive.as_str() {
            "tap" => Kind::Tap,
            "brew" => Kind::Brew,
            "cask" => Kind::Cask,
            "cask_args" => {
                let (positional, options) = parser.arguments()?;
                if !positional.is_empty() {
                    return Err(parser.error("cask_args takes options only"));
                }
                cask_args.extend(options);
                continue;
            }
            other => match EXTENSIONS.iter().find(|ext| **ext == other) {
                Some(ext) => Kind::Extension(ext),
                None => {
                    return Err(ParseError {
                        line,
                        message: format!("`{other}` is not a Brewfile directive nextbrew reads"),
                    });
                }
            },
        };
        let (positional, mut options) = parser.arguments()?;
        let mut positional = positional.into_iter();
        let name = positional
            .next()
            .and_then(|value| value.as_str().map(str::to_string))
            .ok_or_else(|| ParseError {
                line,
                message: format!("`{directive}` needs a name"),
            })?;
        if let Some(clone_target) = positional.next() {
            if kind != Kind::Tap {
                return Err(ParseError {
                    line,
                    message: format!("`{directive}` takes one name"),
                });
            }
            options.push(("clone_target".to_string(), clone_target));
        }
        // `MacAppStore.entry` / `Extension.entry`.
        match kind {
            Kind::Extension("mas") => {
                let id = options.iter().find(|(k, _)| k == "id").map(|(_, v)| v);
                if !matches!(id, Some(Value::Int(_))) {
                    let shown = id.map_or("nil".to_string(), ToString::to_string);
                    return Err(ParseError {
                        line,
                        message: format!("options[:id]({shown}) should be an Integer object"),
                    });
                }
            }
            Kind::Extension(ext @ ("cargo" | "uv")) => {
                options = extension_options(ext, options)
                    .map_err(|message| ParseError { line, message })?;
            }
            Kind::Extension(ext @ ("flatpak" | "winget")) => {
                check_options(ext, &options).map_err(|message| ParseError { line, message })?;
            }
            Kind::Extension(ext @ ("vscode" | "go" | "krew" | "npm")) if !options.is_empty() => {
                return Err(ParseError {
                    line,
                    message: unknown_options(ext, &options, &[]),
                });
            }
            _ => {}
        }
        let name = match kind {
            Kind::Tap => sanitize_tap_name(&name),
            Kind::Brew => sanitize_brew_name(&name),
            Kind::Cask => {
                let mut args = cask_args.clone();
                if let Some(pos) = options.iter().position(|(k, _)| k == "args")
                    && let (_, Value::Hash(own)) = options.remove(pos)
                {
                    // `Hash#merge`: a key already there keeps its place.
                    for (key, value) in own {
                        match args.iter_mut().find(|(k, _)| *k == key) {
                            Some(slot) => slot.1 = value,
                            None => args.push((key, value)),
                        }
                    }
                }
                options.push(("full_name".to_string(), Value::Str(name.clone())));
                options.push(("args".to_string(), Value::Hash(args)));
                name.rsplit('/').next().unwrap_or(&name).to_lowercase()
            }
            Kind::Extension(_) => name,
        };
        entries.push(Entry {
            kind,
            name,
            options,
            line,
        });
    }
    Ok(entries)
}

/// `unknown options([:a, :b]) for <ext>` for the options not in `known`.
fn unknown_options(ext: &str, options: &[(String, Value)], known: &[&str]) -> String {
    let keys: Vec<String> = options
        .iter()
        .filter(|(k, _)| !known.contains(&k.as_str()))
        .map(|(k, _)| format!(":{k}"))
        .collect();
    format!("unknown options([{}]) for {ext}", keys.join(", "))
}

/// `Flatpak.entry` and `Winget.entry`: string `remote:` and `url:` (not
/// both addresses), or `id:` and a known `source:`.
fn check_options(ext: &str, options: &[(String, Value)]) -> Result<(), String> {
    let known: &[&str] = if ext == "flatpak" {
        &["remote", "url"]
    } else {
        &["id", "source"]
    };
    if options.iter().any(|(k, _)| !known.contains(&k.as_str())) {
        return Err(unknown_options(ext, options, known));
    }
    for (key, value) in options {
        // A `flatpak` option may be `nil`; a `winget` one must be a string.
        if !matches!(value, Value::Str(_)) && (ext == "winget" || *value != Value::Nil) {
            return Err(format!(
                "options[:{key}]({value}) should be a String object"
            ));
        }
    }
    let get = |key: &str| {
        options
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_str())
    };
    if ext == "flatpak" {
        if get("url").is_some()
            && get("remote").is_some_and(|r| r.starts_with("http://") || r.starts_with("https://"))
        {
            return Err("url: parameter cannot be used when remote: is already a URL".to_string());
        }
    } else if let Some(source) = get("source").filter(|s| !["winget", "msstore"].contains(s)) {
        return Err(format!(
            "options[:source]({source:?}) should be one of [\"winget\", \"msstore\"]"
        ));
    }
    Ok(())
}

/// `Cargo.entry` and `Uv.entry`: their options checked and normalised as
/// Homebrew keeps them (`source:`, and `with:` for `uv`).
fn extension_options(
    ext: &str,
    options: Vec<(String, Value)>,
) -> Result<Vec<(String, Value)>, String> {
    let known: &[&str] = if ext == "uv" {
        &["with", "source"]
    } else {
        &["source"]
    };
    if options.iter().any(|(k, _)| !known.contains(&k.as_str())) {
        return Err(unknown_options(ext, &options, known));
    }
    let get = |key: &str| {
        options
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .filter(|v| **v != Value::Nil)
    };
    let mut normalized = Vec::new();
    if ext == "uv"
        && let Some(with) = get("with")
    {
        let requirements = match with {
            Value::Array(items) if items.iter().all(|v| matches!(v, Value::Str(_))) => {
                items.iter().filter_map(Value::as_str)
            }
            _ => {
                return Err(format!(
                    "options[:with]({with}) should be an Array of String objects"
                ));
            }
        };
        let with = uv_with(requirements);
        if !with.is_empty() {
            normalized.push((
                "with".to_string(),
                Value::Array(with.into_iter().map(Value::Str).collect()),
            ));
        }
    }
    let Some(source) = get("source") else {
        return Ok(normalized);
    };
    let Value::Str(text) = source else {
        return Err(format!(
            "options[:source]({source}) should be a String object"
        ));
    };
    let normalized_source = if ext == "uv" {
        let source = uv_source(text);
        if source.as_deref().is_some_and(is_local_source) {
            return Err(format!(
                "options[:source]({source:?}) is local to this machine so cannot be used in a Brewfile",
                source = text
            ));
        }
        source
    } else if text.trim().is_empty() {
        None
    } else {
        let source = cargo_source(text)
            .ok_or_else(|| format!("options[:source]({text:?}) should be a git URL"))?;
        let selector = source.split_once('?').map_or("", |(_, query)| query);
        let key = selector.split_once('=').map_or(selector, |(key, _)| key);
        if !selector.is_empty() && !CARGO_GIT_REFERENCES.contains(&key) {
            return Err(format!(
                "options[:source]({text:?}) should select a branch, tag or rev"
            ));
        }
        Some(source)
    };
    if let Some(source) = normalized_source {
        normalized.push(("source".to_string(), Value::Str(source)));
    }
    Ok(normalized)
}

/// `Cargo::GIT_REFERENCE_KEYS`.
pub const CARGO_GIT_REFERENCES: &[&str] = &["branch", "tag", "rev"];

/// `Cargo.normalize_source`: a git address without its `#commit`.
pub fn cargo_source(source: &str) -> Option<String> {
    let source = source.trim();
    let source = match source.rfind('#') {
        Some(at) => &source[..at],
        None => source,
    };
    let remote = ["ssh://", "git://", "http://", "https://"]
        .iter()
        .any(|scheme| source.starts_with(scheme));
    (!source.trim().is_empty() && remote).then(|| source.to_string())
}

/// `Uv.normalize_source`.
pub fn uv_source(source: &str) -> Option<String> {
    let source = source.trim();
    (!source.is_empty()).then(|| source.to_string())
}

/// `Uv::LOCAL_SOURCE_REGEX`: a file path or `file://` address.
fn is_local_source(source: &str) -> bool {
    let source = source.strip_prefix("git+").unwrap_or(source);
    ["file://", "/", "./", "../"]
        .iter()
        .any(|start| source.starts_with(start))
}

/// `Uv.normalize_with`: trimmed, sorted, without blanks or duplicates.
pub fn uv_with<'a>(requirements: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut with: Vec<String> = requirements
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect();
    with.sort();
    with.dedup();
    with
}

/// `Dsl.sanitize_brew_name`.
fn sanitize_brew_name(name: &str) -> String {
    let name = name.to_lowercase();
    let parts: Vec<&str> = name.split('/').collect();
    match parts.as_slice() {
        ["homebrew", "homebrew", formula] => formula.to_string(),
        [user, repo, formula] => {
            format!(
                "{user}/{}/{formula}",
                repo.strip_prefix("homebrew-").unwrap_or(repo)
            )
        }
        _ => name,
    }
}

/// `Dsl.sanitize_tap_name`.
fn sanitize_tap_name(name: &str) -> String {
    let name = name.to_lowercase();
    match name.split_once('/') {
        Some((user, repo)) if !repo.contains('/') => {
            format!("{user}/{}", repo.strip_prefix("homebrew-").unwrap_or(repo))
        }
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_entries_with_options() {
        let entries = parse(
            r#"
# Taps
tap "homebrew/bundle"
tap "Homebrew/homebrew-cask" # inline comment
cask_args appdir: "~/Applications", require_sha: true
brew "jq"
brew "postgresql@16", restart_service: :changed, link: false
brew("mysql", conflicts_with: ["mariadb"], args: ["with-foo"])
brew "x", :link => true, "postinstall" => "echo '#done'"
cask "firefox", args: { appdir: "/Applications" }, greedy: true
mas "Xcode", id: 497799835
vscode "golang.go"
brew "wget",
  link: true
"#,
        )
        .unwrap();
        let summary: Vec<(Kind, &str)> =
            entries.iter().map(|e| (e.kind, e.name.as_str())).collect();
        assert_eq!(
            summary,
            [
                (Kind::Tap, "homebrew/bundle"),
                (Kind::Tap, "homebrew/cask"),
                (Kind::Brew, "jq"),
                (Kind::Brew, "postgresql@16"),
                (Kind::Brew, "mysql"),
                (Kind::Brew, "x"),
                (Kind::Cask, "firefox"),
                (Kind::Extension("mas"), "Xcode"),
                (Kind::Extension("vscode"), "golang.go"),
                (Kind::Brew, "wget"),
            ]
        );
        assert_eq!(
            entries[3].option("restart_service"),
            Some(&Value::Sym("changed".into()))
        );
        assert_eq!(entries[3].option("link"), Some(&Value::Bool(false)));
        assert_eq!(
            entries[4].option("conflicts_with"),
            Some(&Value::Array(vec![Value::Str("mariadb".into())]))
        );
        assert_eq!(
            entries[5].option("postinstall"),
            Some(&Value::Str("echo '#done'".into()))
        );
        assert_eq!(
            entries[6].option("args"),
            Some(&Value::Hash(vec![
                ("appdir".into(), Value::Str("/Applications".into())),
                ("require_sha".into(), Value::Bool(true)),
            ]))
        );
        assert_eq!(entries[6].option("greedy"), Some(&Value::Bool(true)));
        assert_eq!(entries[7].option("id"), Some(&Value::Int(497799835)));
        assert_eq!(entries[9].option("link"), Some(&Value::Bool(true)));
        assert_eq!(entries[9].line, 13);
    }

    #[test]
    fn refuses_ruby_with_its_line() {
        for (source, line) in [
            ("brew \"jq\"\nif OS.mac?\n  brew \"x\"\nend\n", 2),
            ("brew \"a#{1}\"\n", 1),
            ("brew \"jq\"\nbrew \"x\" if true\n", 2),
            ("\ninstance_eval File.read(\"x\")\n", 2),
        ] {
            let err = parse(source).unwrap_err();
            assert_eq!(err.line, line, "{source}: {err}");
        }
    }

    #[test]
    fn checks_extension_options_like_homebrew() {
        assert!(parse("mas \"Xcode\", id: 497799835\nvscode \"golang.go\"\n").is_ok());
        assert!(
            parse("mas \"Xcode\"\n")
                .unwrap_err()
                .message
                .contains("should be an Integer")
        );
        assert!(parse("mas \"Xcode\", id: \"1\"\n").is_err());
        assert!(
            parse("vscode \"golang.go\", x: 1\n")
                .unwrap_err()
                .message
                .contains("unknown options")
        );
        assert!(
            parse(concat!(
                "flatpak \"a\", remote: \"mine\", url: \"https://x\"\n",
                "flatpak \"b\", remote: nil\n",
                "winget \"Git\", id: \"Git.Git\", source: \"msstore\"\n",
            ))
            .is_ok()
        );
        assert_eq!(
            parse("go \"x\", with: [\"y\"]\n").unwrap_err().message,
            "unknown options([:with]) for go"
        );

        let entries = parse(concat!(
            "cargo \"foo\", source: \" https://github.com/x/foo?tag=v1#abc \"\n",
            "cargo \"bar\", source: \"\"\n",
            "uv \"black\", with: [\" requests\", \"attrs\", \"attrs\", \"\"], source: nil\n",
        ))
        .unwrap();
        assert_eq!(
            entries[0].options,
            [(
                "source".to_string(),
                Value::Str("https://github.com/x/foo?tag=v1".into())
            )]
        );
        assert!(entries[1].options.is_empty());
        assert_eq!(
            entries[2].options,
            [(
                "with".to_string(),
                Value::Array(vec![
                    Value::Str("attrs".into()),
                    Value::Str("requests".into())
                ])
            )]
        );
        for (source, error) in [
            ("cargo \"x\", source: \"/tmp/x\"", "should be a git URL"),
            (
                "cargo \"x\", source: \"https://h/x?x=1\"",
                "should select a branch, tag or rev",
            ),
            ("cargo \"x\", source: :git", "should be a String object"),
            (
                "cargo \"x\", path: \"y\"",
                "unknown options([:path]) for cargo",
            ),
            (
                "uv \"x\", with: \"y\"",
                "should be an Array of String objects",
            ),
            (
                "uv \"x\", source: \"git+file:///x\"",
                "is local to this machine",
            ),
            (
                "flatpak \"x\", branch: \"y\"",
                "unknown options([:branch]) for flatpak",
            ),
            (
                "flatpak \"x\", remote: :flathub",
                "options[:remote](:flathub) should be a String object",
            ),
            (
                "flatpak \"x\", remote: \"https://a\", url: \"https://b\"",
                "url: parameter cannot be used when remote: is already a URL",
            ),
            (
                "winget \"x\", id: nil",
                "options[:id](nil) should be a String object",
            ),
            (
                "winget \"x\", source: \"choco\"",
                "options[:source](\"choco\") should be one of [\"winget\", \"msstore\"]",
            ),
        ] {
            let message = parse(source).unwrap_err().message;
            assert!(message.contains(error), "{source}: {message}");
        }
    }

    #[test]
    fn sanitizes_names_like_homebrew() {
        assert_eq!(sanitize_brew_name("Homebrew/homebrew/Wget"), "wget");
        assert_eq!(sanitize_brew_name("user/homebrew-tap/x"), "user/tap/x");
        assert_eq!(sanitize_tap_name("Homebrew/homebrew-cask"), "homebrew/cask");
    }
}
