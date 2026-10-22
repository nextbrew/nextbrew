//! Homebrew's pkg-config files for what macOS itself provides (zlib,
//! libcurl, sqlite3 and the like), from its `os/mac/pkgconfig/<version>`,
//! put at the same place under the prefix. Superenv gives a build these
//! as pkg-config's own search path; formulae (pkgconf) name the place.
//! The files are Homebrew's (BSD 2-Clause, see LICENSE-HOMEBREW).

use std::path::{Path, PathBuf};

/// Each macOS version's files: name and contents.
type Files = &'static [(&'static str, &'static str)];

const FILES: &[(&str, Files)] = &[
    (
        "14",
        &[
            ("bzip2.pc", include_str!("pkgconfig/14/bzip2.pc")),
            ("expat.pc", include_str!("pkgconfig/14/expat.pc")),
            ("libcurl.pc", include_str!("pkgconfig/14/libcurl.pc")),
            ("libedit.pc", include_str!("pkgconfig/14/libedit.pc")),
            ("libexslt.pc", include_str!("pkgconfig/14/libexslt.pc")),
            ("libffi.pc", include_str!("pkgconfig/14/libffi.pc")),
            ("libxml-2.0.pc", include_str!("pkgconfig/14/libxml-2.0.pc")),
            ("libxslt.pc", include_str!("pkgconfig/14/libxslt.pc")),
            ("ncurses.pc", include_str!("pkgconfig/14/ncurses.pc")),
            ("ncursesw.pc", include_str!("pkgconfig/14/ncursesw.pc")),
            ("sqlite3.pc", include_str!("pkgconfig/14/sqlite3.pc")),
            ("uuid.pc", include_str!("pkgconfig/14/uuid.pc")),
            ("zlib.pc", include_str!("pkgconfig/14/zlib.pc")),
        ],
    ),
    (
        "15",
        &[
            ("bzip2.pc", include_str!("pkgconfig/15/bzip2.pc")),
            ("expat.pc", include_str!("pkgconfig/15/expat.pc")),
            ("libcurl.pc", include_str!("pkgconfig/15/libcurl.pc")),
            ("libedit.pc", include_str!("pkgconfig/15/libedit.pc")),
            ("libexslt.pc", include_str!("pkgconfig/15/libexslt.pc")),
            ("libffi.pc", include_str!("pkgconfig/15/libffi.pc")),
            ("libxml-2.0.pc", include_str!("pkgconfig/15/libxml-2.0.pc")),
            ("libxslt.pc", include_str!("pkgconfig/15/libxslt.pc")),
            ("ncurses.pc", include_str!("pkgconfig/15/ncurses.pc")),
            ("ncursesw.pc", include_str!("pkgconfig/15/ncursesw.pc")),
            ("sqlite3.pc", include_str!("pkgconfig/15/sqlite3.pc")),
            ("uuid.pc", include_str!("pkgconfig/15/uuid.pc")),
            ("zlib.pc", include_str!("pkgconfig/15/zlib.pc")),
        ],
    ),
    (
        "26",
        &[
            ("bzip2.pc", include_str!("pkgconfig/26/bzip2.pc")),
            ("expat.pc", include_str!("pkgconfig/26/expat.pc")),
            ("libcurl.pc", include_str!("pkgconfig/26/libcurl.pc")),
            ("libedit.pc", include_str!("pkgconfig/26/libedit.pc")),
            ("libexslt.pc", include_str!("pkgconfig/26/libexslt.pc")),
            ("libffi.pc", include_str!("pkgconfig/26/libffi.pc")),
            ("libxml-2.0.pc", include_str!("pkgconfig/26/libxml-2.0.pc")),
            ("libxslt.pc", include_str!("pkgconfig/26/libxslt.pc")),
            ("ncurses.pc", include_str!("pkgconfig/26/ncurses.pc")),
            ("ncursesw.pc", include_str!("pkgconfig/26/ncursesw.pc")),
            ("sqlite3.pc", include_str!("pkgconfig/26/sqlite3.pc")),
            ("uuid.pc", include_str!("pkgconfig/26/uuid.pc")),
            ("zlib.pc", include_str!("pkgconfig/26/zlib.pc")),
        ],
    ),
    (
        "27",
        &[
            ("bzip2.pc", include_str!("pkgconfig/27/bzip2.pc")),
            ("expat.pc", include_str!("pkgconfig/27/expat.pc")),
            ("libcurl.pc", include_str!("pkgconfig/27/libcurl.pc")),
            ("libedit.pc", include_str!("pkgconfig/27/libedit.pc")),
            ("libexslt.pc", include_str!("pkgconfig/27/libexslt.pc")),
            ("libffi.pc", include_str!("pkgconfig/27/libffi.pc")),
            ("libxml-2.0.pc", include_str!("pkgconfig/27/libxml-2.0.pc")),
            ("libxslt.pc", include_str!("pkgconfig/27/libxslt.pc")),
            ("ncurses.pc", include_str!("pkgconfig/27/ncurses.pc")),
            ("ncursesw.pc", include_str!("pkgconfig/27/ncursesw.pc")),
            ("sqlite3.pc", include_str!("pkgconfig/27/sqlite3.pc")),
            ("uuid.pc", include_str!("pkgconfig/27/uuid.pc")),
            ("zlib.pc", include_str!("pkgconfig/27/zlib.pc")),
        ],
    ),
];

/// This Mac's files, written under `prefix` if they are not there yet;
/// where they are.
pub fn install(prefix: &Path) -> Option<PathBuf> {
    let version = nb_core::macos_major_version()?.to_string();
    let (_, files) = FILES.iter().find(|(v, _)| *v == version)?;
    let dir = prefix
        .join("Library/Homebrew/os/mac/pkgconfig")
        .join(&version);
    for (name, contents) in *files {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(*contents) {
            std::fs::create_dir_all(&dir).ok()?;
            std::fs::write(&path, contents).ok()?;
        }
    }
    Some(dir)
}
