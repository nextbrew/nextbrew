//! `Version.parse`: the version Homebrew reads from a download URL when a
//! formula does not state one, by trying its parsers in order.

use std::sync::LazyLock;

use regex::Regex;

const N_OPT: &str = r"(?:\d+(?:\.\d+)*)";
const N_DOTS: &str = r"(?:\d+(?:\.\d+)+)";
const MINOR_OR_PATCH: &str = r"(?:\d+(?:\.\d+){1,2})";
const CONTENT_SUFFIX: &str = r"(?:[._-](?i:bin|dist|stable|src|sources?|final|full))";
const PRERELEASE_SUFFIX: &str = r"(?:[._-]?(?i:alpha|beta|pre|rc)\.?\d{0,2})";

enum Input {
    /// The whole spec.
    Url,
    /// The file name without its extension.
    Stem,
}

struct Parser {
    input: Input,
    regex: Regex,
    underscores_to_dots: bool,
}

fn parser(input: Input, pattern: &str) -> Parser {
    let pattern = pattern
        .replace("N_OPT", N_OPT)
        .replace("N_DOTS", N_DOTS)
        .replace("MINOR_OR_PATCH", MINOR_OR_PATCH)
        .replace("CONTENT_SUFFIX", CONTENT_SUFFIX)
        .replace("PRERELEASE_SUFFIX", PRERELEASE_SUFFIX);
    Parser {
        input,
        regex: Regex::new(&ascii(&pattern)).unwrap(),
        underscores_to_dots: false,
    }
}

/// Ruby's `\d` and `\w` are ASCII; Rust's are Unicode.
fn ascii(pattern: &str) -> String {
    pattern
        .replace(r"\d", "[0-9]")
        .replace(r"\w", "[A-Za-z0-9_]")
}

/// `Version::VERSION_PARSERS`, in order.
static PARSERS: LazyLock<Vec<Parser>> = LazyLock::new(|| {
    use Input::{Stem, Url};
    vec![
        parser(Stem, r"(?:^|[._-]?)v?(\d{4}-\d{2}-\d{2})"),
        parser(
            Url,
            r"github\.com/.+/(?:zip|tar)ball/(?:v|\w+-)?((?:\d+[._-])+\d*)$",
        ),
        parser(
            Url,
            r"github\.com/.+/releases/download/(?:[rvV]_?)?(N_DOTS)/",
        ),
        parser(Url, r"[_-]([Rr]\d+[AaBb]\d*(?:-\d+)?)"),
        Parser {
            underscores_to_dots: true,
            ..parser(Stem, r"((?:\d+_)+\d+)$")
        },
        parser(Stem, r"[_-](N_DOTS-(?:p|P|rc|RC)?\d+)CONTENT_SUFFIX?$"),
        parser(Stem, r"^v?(N_DOTS(?:-N_OPT)+)"),
        parser(Url, r"[-v](N_OPT)$"),
        parser(Stem, r"-(\d+-\d+)"),
        parser(Stem, r"-(N_OPT)$"),
        parser(Stem, r"-(N_OPT(?:.post\d+)?)$"),
        parser(Stem, r"-(N_OPT(?:[abc]|rc|RC)\d*)$"),
        parser(Stem, r"-(N_OPT-(?:alpha|beta|rc)\d*)$"),
        parser(Stem, r"-(MINOR_OR_PATCH)-w(?:in)?(?:32|64)$"),
        parser(Stem, r"\.(MINOR_OR_PATCH)\+opam$"),
        parser(
            Stem,
            r"[_-](MINOR_OR_PATCH(?:-\d+)?)[._-](?:i[36]86|x86|x64(?:[_-](?:32|64))?)$",
        ),
        parser(Stem, r"[-.vV]?(N_DOTSPRERELEASE_SUFFIX)"),
        parser(Stem, r"(N_OPT)$"),
        parser(Stem, r"[-vV](N_DOTS[abc]?)CONTENT_SUFFIX$"),
        parser(Stem, r"-(N_DOTS)-"),
        parser(Stem, r"_(N_OPT[abc]?)\.orig$"),
        parser(Stem, r"-v?(\d[^-]+)"),
        parser(Stem, r"_v?(\d[^_]+)"),
        parser(Url, r"/(?:[rvV]_?)?(\d+\.\d+(?:\.\d+){0,2})"),
        parser(Stem, r"\.v(\d+[a-z]?)"),
        parser(Url, r"[-.vV]?(N_DOTSPRERELEASE_SUFFIX?)"),
    ]
});

/// `Version.detect(url)`: the version in `spec`, or `None`.
pub fn detect(spec: &str) -> Option<String> {
    let spec = decode_www_form_component(spec);
    for parser in PARSERS.iter() {
        let input = match parser.input {
            Input::Url => spec.clone(),
            Input::Stem => stem_input(&spec),
        };
        let Some(found) = parser.regex.captures(&input).and_then(|c| c.get(1)) else {
            continue;
        };
        let version = found.as_str();
        if version.trim().is_empty() {
            continue;
        }
        return Some(if parser.underscores_to_dots {
            version.replace('_', ".")
        } else {
            version.to_string()
        });
    }
    None
}

/// `StemParser.process_spec`.
fn stem_input(spec: &str) -> String {
    static SOURCEFORGE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:sourceforge\.net|sf\.net)/.*/download$").unwrap());
    static NO_EXTENSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.[^a-zA-Z]+$").unwrap());
    if SOURCEFORGE.is_match(spec) {
        return stem(&dirname(spec));
    }
    if NO_EXTENSION.is_match(spec) {
        return basename(spec);
    }
    stem(spec)
}

/// `File.basename`: the last path component, trailing slashes ignored.
fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

fn dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(at) => trimmed[..at].to_string(),
        None => ".".to_string(),
    }
}

/// Homebrew's `Pathname#extname`: bottle and `.tar.*` double extensions,
/// and nothing for a name ending in a version number.
fn extname(path: &str) -> String {
    static BOTTLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\.([a-z0-9_]+)\.bottle\.(?:(\d+)\.)?tar\.gz$").unwrap());
    static ARCHIVE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\.(?:tar|cpio|pax)\.(?:gz|bz2|lz|xz|zst|Z))\z").unwrap());
    static VERSIONED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b[0-9]+\.[0-9]+[^.]*\z").unwrap());
    let name = basename(path);
    if let Some(found) = BOTTLE.find(&name) {
        return found.as_str().to_string();
    }
    if let Some(found) = ARCHIVE.captures(&name).and_then(|c| c.get(1)) {
        return found.as_str().to_string();
    }
    if VERSIONED.is_match(&name) && !name.ends_with(".7z") {
        return String::new();
    }
    // `File.extname`: from the last dot, unless it starts the name.
    match name.rfind('.') {
        Some(at) if at > 0 => name[at..].to_string(),
        _ => String::new(),
    }
}

/// `Pathname#stem`: the base name without `extname`.
fn stem(path: &str) -> String {
    let name = basename(path);
    let ext = extname(path);
    name.strip_suffix(ext.as_str()).unwrap_or(&name).to_string()
}

/// `URI.decode_www_form_component`: `+` is a space, `%XX` a byte.
fn decode_www_form_component(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_versions_like_homebrew() {
        for (url, version) in [
            ("https://example.com/foo-2023-09-28.tar.gz", "2023-09-28"),
            ("https://github.com/foo/bar/tarball/v1.2.3", "1.2.3"),
            (
                "https://github.com/foo/bar/releases/download/v1.2/foo-1.2.0.tar.gz",
                "1.2",
            ),
            ("https://example.com/boost_1_39_0.tar.gz", "1.39.0"),
            ("https://example.com/foobar-4.5.1-1.tar.gz", "4.5.1-1"),
            ("https://example.com/ruby-1.9.1-p243.tar.gz", "1.9.1-p243"),
            ("https://waf.io/waf-1.8.12", "1.8.12"),
            (
                "https://codeload.github.com/gsamokovarov/jump/tar.gz/v0.7.1",
                "0.7.1",
            ),
            ("https://example.com/foobar-4.5.1.tar.gz", "4.5.1"),
            ("https://example.com/foobar-4.5.1b.tar.gz", "4.5.1b"),
            (
                "https://example.com/foobar-4.5.0-beta1.tar.gz",
                "4.5.0-beta1",
            ),
            (
                "https://ftpmirror.gnu.org/libidn/libidn-1.29-win64.zip",
                "1.29",
            ),
            ("https://opam.ocaml.org/archives/sha.1.9+opam.tar.gz", "1.9"),
            (
                "https://github.com/dlang/dmd/archive/v2.074.0-rc1.tar.gz",
                "2.074.0-rc1",
            ),
            ("https://example.com/foobar4.5.1.tar.gz", "4.5.1"),
            (
                "http://www.antlr.org/download/antlr-3.4-complete.jar",
                "3.4",
            ),
            ("https://example.com/dash_0.5.5.1.orig.tar.gz", "0.5.5.1"),
            (
                "https://www.openssl.org/source/openssl-0.9.8s.tar.gz",
                "0.9.8s",
            ),
            ("https://example.com/astyle_1.23_macosx.tar.gz", "1.23"),
            (
                "http://mirrors.jenkins-ci.org/war/1.486/jenkins.war",
                "1.486",
            ),
            ("https://www.ijg.org/files/jpegsrc.v8d.tar.gz", "8d"),
            (
                "https://secure.php.net/get/php-7.1.10.tar.bz2/from/this/mirror",
                "7.1.10",
            ),
            (
                "https://github.com/fluxcd/flux2/releases/download/v2.9.6/flux_2.9.6_darwin_arm64.tar.gz",
                "2.9.6",
            ),
        ] {
            assert_eq!(detect(url).as_deref(), Some(version), "{url}");
        }
        assert_eq!(detect("https://example.com/download"), None);
    }

    #[test]
    fn splits_names_like_pathname() {
        assert_eq!(extname("a/foo-1.2.tar.gz"), ".tar.gz");
        assert_eq!(stem("a/foo-1.2.tar.gz"), "foo-1.2");
        assert_eq!(extname("foo-1.2"), "");
        assert_eq!(extname("foo.zip"), ".zip");
        assert_eq!(decode_www_form_component("a%20b+c"), "a b c");
        assert_eq!(decode_www_form_component("%zz%aé%"), "%zz%aé%");
    }
}
