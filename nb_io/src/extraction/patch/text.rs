//! Rewrite Homebrew placeholders and prefixes in a keg's text files.

use std::fs;
use std::io::{self, Read};
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::OnceLock;

use memchr::memmem;
use regex::bytes::Regex;

use super::phar;

/// Replace each `(from, to)` pair in the file at `path`, in order.
///
/// Scripts that start with `#!` are rewritten even if they contain binary
/// data, as Homebrew does: PHP archives are scripts with a binary payload.
/// Other files with NUL bytes in their first 8 KiB are treated as binaries
/// and left alone. PHP archives get their signature recomputed after
/// patching. Returns whether the file changed.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn rewrite_text_file(path: &Path, replacements: &[(&str, &str)]) -> io::Result<bool> {
    if is_binary(path)? {
        return Ok(false);
    }
    rewrite_text(path, replacements)
}

/// [`rewrite_text_file`] for a file the caller already knows is text.
pub(crate) fn rewrite_text(path: &Path, replacements: &[(&str, &str)]) -> io::Result<bool> {
    let mut content = fs::read(path)?;
    let mut changed = false;
    for (from, to) in replacements {
        if from == to {
            continue;
        }
        if let Some(replaced) = replace_all(&content, from.as_bytes(), to.as_bytes()) {
            content = replaced;
            changed = true;
        }
    }
    if !changed {
        return Ok(false);
    }
    if let Some(resigned) = phar::resign(&content) {
        content = resigned;
    }
    if let Some(recounted) = recount_php_strings(&content) {
        content = recounted;
    }

    write_preserving_mode(path, &content)?;
    Ok(true)
}

/// Whether `path` is a binary file: it has a NUL byte in its first 8 KiB and
/// is not a `#!` script. This is the split Homebrew uses between files it
/// rewrites freely and files whose strings must be replaced in place.
pub(crate) fn is_binary(path: &Path) -> io::Result<bool> {
    let mut head = [0u8; 8192];
    let n = fs::File::open(path)?.read(&mut head)?;
    Ok(head[..n].contains(&0) && !head[..n].starts_with(b"#!"))
}

fn replace_all(haystack: &[u8], from: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    if from.is_empty() {
        return None;
    }
    let finder = memmem::Finder::new(from);
    let mut rest = haystack;
    let mut out = Vec::new();
    let mut found = false;
    while let Some(i) = finder.find(rest) {
        found = true;
        out.extend_from_slice(&rest[..i]);
        out.extend_from_slice(to);
        rest = &rest[i + from.len()..];
    }
    if !found {
        return None;
    }
    out.extend_from_slice(rest);
    Some(out)
}

/// PHP's `serialize` records each string's byte length (`s:67:"..."`), and
/// PEAR keeps its config and registry that way, sized for the Cellar the
/// bottle was built for. Recount the strings a new prefix resized.
// ponytail: only files that start like PEAR's (`a:` or `#PEAR_Config`), and
// only path strings with no `"` in them; a real unserializer if more turn up.
fn recount_php_strings(content: &[u8]) -> Option<Vec<u8>> {
    static STRING: OnceLock<Regex> = OnceLock::new();
    if !(content.starts_with(b"a:") || content.starts_with(b"#PEAR_Config")) {
        return None;
    }
    let string = STRING.get_or_init(|| Regex::new(r#"s:(\d+):"([^"]*/[^"]*)";"#).unwrap());
    let mut changed = false;
    let out = string.replace_all(content, |c: &regex::bytes::Captures| {
        let value = &c[2];
        if std::str::from_utf8(&c[1]).ok().and_then(|n| n.parse().ok()) == Some(value.len()) {
            return c[0].to_vec();
        }
        changed = true;
        let mut s = format!("s:{}:\"", value.len()).into_bytes();
        s.extend_from_slice(value);
        s.extend_from_slice(b"\";");
        s
    });
    changed.then(|| out.into_owned())
}

pub(crate) fn write_preserving_mode(path: &Path, content: &[u8]) -> io::Result<()> {
    let permissions = fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    let readonly = mode & 0o200 == 0;
    if readonly {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200))?;
    }
    let result = fs::write(path, content);
    if readonly {
        fs::set_permissions(path, permissions)?;
    }
    result
}

/// Replace `old` with `new` wherever it starts a path in `contents`: it is
/// followed by the end of the data, a NUL, a `/` or a `:`. A shorter `new`
/// is padded with `/` to keep the file layout, as Homebrew does, so the rest
/// of the path is still reachable. Returns the byte ranges that changed.
pub(crate) fn replace_prefix_in_place(
    contents: &mut [u8],
    old: &[u8],
    new: &[u8],
) -> Vec<Range<usize>> {
    debug_assert!(new.len() <= old.len());
    let finder = memmem::Finder::new(old);
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(found) = finder.find(&contents[from..]) {
        let start = from + found;
        let end = start + old.len();
        let boundary = matches!(contents.get(end), None | Some(0) | Some(b'/') | Some(b':'));
        if boundary {
            contents[start..start + new.len()].copy_from_slice(new);
            contents[start + new.len()..end].fill(b'/');
            ranges.push(start..end);
            from = end;
        } else {
            from = start + 1;
        }
    }
    ranges
}

/// Whether `contents` has a path under `prefix` that could not be rewritten.
pub(crate) fn has_paths_under(contents: &[u8], prefix: &[u8]) -> bool {
    memmem::find_iter(contents, prefix)
        .any(|start| contents.get(start + prefix.len()) == Some(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn replace_prefix_in_place_only_matches_whole_path_components() {
        let mut data =
            b"/opt/homebrew/lib\0/opt/homebrewery\0/opt/homebrew:/x\0/opt/homebrew".to_vec();

        let ranges = replace_prefix_in_place(&mut data, b"/opt/homebrew", b"/opt/nb");

        assert_eq!(ranges.len(), 3);
        assert_eq!(
            data,
            b"/opt/nb///////lib\0/opt/homebrewery\0/opt/nb//////:/x\0/opt/nb//////".to_vec()
        );
    }

    const PLACEHOLDERS: &[(&str, &str)] = &[
        ("@@HOMEBREW_PREFIX@@", "/opt/nextbrew"),
        ("/opt/homebrew", "/opt/nextbrew"),
    ];

    #[test]
    fn rewrites_placeholders_and_prefix_in_text() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("script.sh");
        fs::write(
            &file,
            "#!/bin/sh\nA=@@HOMEBREW_PREFIX@@/bin\nB=/opt/homebrew/etc\n",
        )
        .unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "#!/bin/sh\nA=/opt/nextbrew/bin\nB=/opt/nextbrew/etc\n"
        );
    }

    #[test]
    fn rewrites_scripts_with_binary_payloads() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("tool.phar");
        fs::write(
            &file,
            b"#!/usr/bin/env php\n\0\x01\xc9'@@HOMEBREW_PREFIX@@/etc/cert.pem'",
        )
        .unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        assert_eq!(
            fs::read(&file).unwrap(),
            b"#!/usr/bin/env php\n\0\x01\xc9'/opt/nextbrew/etc/cert.pem'"
        );
    }

    #[test]
    fn rewrites_text_that_is_not_utf8() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("latin1.conf");
        fs::write(&file, b"# caf\xe9\nprefix=@@HOMEBREW_PREFIX@@\n").unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        assert_eq!(
            fs::read(&file).unwrap(),
            b"# caf\xe9\nprefix=/opt/nextbrew\n"
        );
    }

    #[test]
    fn leaves_binaries_alone() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("libfoo.a");
        let original = b"!<arch>\n\0\0@@HOMEBREW_PREFIX@@/lib".to_vec();
        fs::write(&file, &original).unwrap();

        assert!(!rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        assert_eq!(fs::read(&file).unwrap(), original);
    }

    #[test]
    fn preserves_read_only_and_executable_modes() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("tool");
        fs::write(&file, "#!/bin/sh\nexec @@HOMEBREW_PREFIX@@/bin/real\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o555)).unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o555);
    }

    #[test]
    fn reports_unchanged_files() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("README");
        fs::write(&file, "nothing to see\n").unwrap();
        assert!(!rewrite_text_file(&file, PLACEHOLDERS).unwrap());
    }

    #[test]
    fn recounts_pear_config_strings() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("pear.conf");
        let old = "/home/linuxbrew/.linuxbrew/share/pear";
        let conf = format!(
            "#PEAR_Config 0.9\na:2:{{s:7:\"php_dir\";s:{}:\"{}\";s:3:\"bin\";s:4:\"/x\"y\";}}",
            old.len(),
            old.replace("/home/linuxbrew/.linuxbrew", "@@HOMEBREW_PREFIX@@")
        );
        fs::write(&file, conf).unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        let new = "/opt/nextbrew/share/pear";
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            format!(
                "#PEAR_Config 0.9\na:2:{{s:7:\"php_dir\";s:{}:\"{new}\";s:3:\"bin\";s:4:\"/x\"y\";}}",
                new.len()
            )
        );
    }

    #[test]
    fn resigns_patched_phars() {
        use sha2::{Digest, Sha512};

        let body =
            b"#!/usr/bin/env php\n<?php __HALT_COMPILER(); ?>\0'@@HOMEBREW_PREFIX@@/etc'".to_vec();
        let mut data = body.clone();
        data.extend_from_slice(&Sha512::digest(&body));
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(b"GBMB");

        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("composer");
        fs::write(&file, &data).unwrap();

        assert!(rewrite_text_file(&file, PLACEHOLDERS).unwrap());
        let patched = fs::read(&file).unwrap();
        let body_len = patched.len() - 8 - 64;
        assert_eq!(
            &patched[body_len..patched.len() - 8],
            &Sha512::digest(&patched[..body_len])[..]
        );
        assert!(patched.windows(13).any(|w| w == b"/opt/nextbrew"));
    }
}
