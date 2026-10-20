//! The values a tap's Ruby works with, and the methods on them nextbrew
//! understands.

use std::path::PathBuf;

/// macOS codenames Homebrew knows, with their versions (`MacOSVersion`).
pub const MACOS_CODENAMES: &[(&str, u32, u32)] = &[
    ("golden_gate", 27, 0),
    ("tahoe", 26, 0),
    ("sequoia", 15, 0),
    ("sonoma", 14, 0),
    ("ventura", 13, 0),
    ("monterey", 12, 0),
    ("big_sur", 11, 0),
    ("catalina", 10, 15),
    ("mojave", 10, 14),
    ("high_sierra", 10, 13),
    ("sierra", 10, 12),
    ("el_capitan", 10, 11),
];

pub fn codename_version(name: &str) -> Option<(u32, u32)> {
    MACOS_CODENAMES
        .iter()
        .find(|(codename, _, _)| *codename == name)
        .map(|(_, major, minor)| (*major, *minor))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Sym(String),
    Array(Vec<Value>),
    Hash(Vec<(Value, Value)>),
    Path(PathBuf),
    /// A formula's or cask's `version`.
    Version(String),
    /// `MacOS.version`.
    MacOs(u32, u32),
    /// A module or class used as a receiver: `OS`, `Hardware::CPU`, `MacOS`.
    Module(String),
    /// `Formula["name"]`: another formula, by name.
    FormulaRef(String),
}

impl Value {
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    /// `to_s`.
    pub fn to_s(&self) -> String {
        match self {
            Value::Nil => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Int(n) => n.to_string(),
            Value::Float(f) => {
                if f.fract() == 0.0 {
                    format!("{f:.1}")
                } else {
                    f.to_string()
                }
            }
            Value::Str(s) | Value::Sym(s) | Value::Version(s) | Value::Module(s) => s.clone(),
            Value::FormulaRef(name) => name.clone(),
            Value::Path(p) => p.to_string_lossy().into_owned(),
            Value::MacOs(major, 0) if *major >= 11 => major.to_string(),
            Value::MacOs(major, minor) => format!("{major}.{minor}"),
            Value::Array(items) => {
                let items: Vec<String> = items.iter().map(Value::inspect).collect();
                format!("[{}]", items.join(", "))
            }
            Value::Hash(_) => self.inspect(),
        }
    }

    /// `inspect`, for messages and `Symbol#inspect` in the API.
    pub fn inspect(&self) -> String {
        match self {
            Value::Nil => "nil".to_string(),
            Value::Str(s) => format!("{s:?}"),
            Value::Sym(s) => format!(":{s}"),
            Value::Hash(pairs) => {
                let pairs: Vec<String> = pairs
                    .iter()
                    .map(|(k, v)| format!("{} => {}", k.inspect(), v.inspect()))
                    .collect();
                format!("{{{}}}", pairs.join(", "))
            }
            other => other.to_s(),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Sym(s) | Value::Version(s) => Some(s),
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "Integer",
            Value::Float(_) => "Float",
            Value::Str(_) => "String",
            Value::Sym(_) => "Symbol",
            Value::Array(_) => "Array",
            Value::Hash(_) => "Hash",
            Value::Path(_) => "Pathname",
            Value::Version(_) => "Version",
            Value::MacOs(..) => "MacOSVersion",
            Value::Module(_) => "Module",
            Value::FormulaRef(_) => "Formula",
        }
    }

    /// The macOS version a value names, for comparing with `MacOS.version`:
    /// a codename symbol, or a version string or number.
    pub fn macos_version(&self) -> Option<(u32, u32)> {
        match self {
            Value::MacOs(major, minor) => Some((*major, *minor)),
            Value::Sym(name) => codename_version(name),
            Value::Str(text) | Value::Version(text) => parse_macos(text),
            Value::Int(n) => Some((*n as u32, 0)),
            Value::Float(f) => parse_macos(&f.to_string()),
            _ => None,
        }
    }
}

fn parse_macos(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    // `10.16` is Big Sur.
    Some(if (major, minor) == (10, 16) {
        (11, 0)
    } else {
        (major, minor)
    })
}

/// `Version#major` and the like: the version's first tokens.
pub fn version_tokens(version: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut digits = None;
    for c in version.chars() {
        let is_digit = c.is_ascii_digit();
        if !c.is_ascii_alphanumeric() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            digits = None;
            continue;
        }
        if digits.is_some_and(|d| d != is_digit) && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        digits = Some(is_digit);
        current.push(c);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_versions_and_macos() {
        assert_eq!(version_tokens("1.25.4"), ["1", "25", "4"]);
        assert_eq!(version_tokens("2.0rc1"), ["2", "0", "rc", "1"]);
        assert_eq!(Value::Sym("sonoma".into()).macos_version(), Some((14, 0)));
        assert_eq!(Value::Str("10.16".into()).macos_version(), Some((11, 0)));
        assert_eq!(Value::MacOs(26, 0).to_s(), "26");
        assert_eq!(Value::MacOs(10, 15).to_s(), "10.15");
    }
}
