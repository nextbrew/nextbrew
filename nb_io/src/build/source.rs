use std::path::{Path, PathBuf};

use nb_core::Error;
use tokio::fs;

use crate::checksum::verify_sha256_bytes;
use crate::extraction::extract_tarball;

/// Fetch the source from `url`, else each of `mirrors` in turn (as
/// Homebrew's download strategies do), check it, and unpack it.
pub async fn download_and_extract_source(
    url: &str,
    mirrors: &[String],
    expected_checksum: Option<&str>,
    work_dir: &Path,
) -> Result<PathBuf, Error> {
    let tarball_path = work_dir.join("source.tar.gz");
    let mut result = download_source(url, &tarball_path).await;
    for mirror in mirrors {
        if result.is_ok() {
            break;
        }
        result = download_source(mirror, &tarball_path).await;
    }
    result?;

    verify_checksum(&tarball_path, expected_checksum, url).await?;

    let src_dir = work_dir.join("src");
    fs::create_dir_all(&src_dir)
        .await
        .map_err(Error::file("failed to create source directory"))?;

    unpack(&tarball_path, &src_dir).await?;

    find_source_root(&src_dir).await
}

/// `UnpackStrategy`'s as Homebrew picks it for a source: a zip with our
/// own unzip, anything else with the system's `tar`, which reads every
/// compression a source comes in (`.tar.bz2`, `.tar.lz` and the like) and
/// names the files as Homebrew's do.
async fn unpack(archive: &Path, dir: &Path) -> Result<(), Error> {
    let zip = fs::read(archive)
        .await
        .map(|bytes| bytes.starts_with(b"PK\x03\x04"))
        .unwrap_or(false);
    if zip {
        return extract_tarball(archive, dir);
    }
    let output = tokio::process::Command::new("tar")
        .args(["--extract", "--no-same-owner", "--file"])
        .arg(archive)
        .arg("--directory")
        .arg(dir)
        .output()
        .await
        .map_err(Error::exec("failed to run tar"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::ExecutionError {
        message: format!(
            "failed to unpack the source: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    })
}

/// The formula's own `mirror`s for its source, from its file: those at
/// the top of the class or of its `stable do` block, which a resource's or
/// a patch's are not.
pub fn source_mirrors(formula_rb: &str) -> Vec<String> {
    let mut in_stable = false;
    let mut mirrors = Vec::new();
    for line in formula_rb.lines() {
        match line {
            "  stable do" => in_stable = true,
            "  end" => in_stable = false,
            _ => {}
        }
        let indent = if in_stable { "    " } else { "  " };
        if let Some(url) = line
            .strip_prefix(indent)
            .and_then(|rest| rest.strip_prefix("mirror \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .filter(|url| !url.contains("#{"))
        {
            mirrors.push(url.to_string());
        }
    }
    mirrors
}

async fn download_source(url: &str, dest: &Path) -> Result<(), Error> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .use_preconfigured_tls((*crate::network::tls::shared_tls_config()).clone())
        .build()
        .map_err(Error::network("failed to create HTTP client"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(Error::network("failed to download source"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::NetworkFailure {
            message: format!("source download returned HTTP {status}"),
        });
    }

    let bytes = response
        .bytes()
        .await
        .map_err(Error::network("failed to read source response"))?;

    fs::write(dest, &bytes)
        .await
        .map_err(Error::file("failed to write source tarball"))
}

async fn verify_checksum(path: &Path, expected: Option<&str>, url: &str) -> Result<(), Error> {
    let bytes = fs::read(path)
        .await
        .map_err(Error::file("failed to read tarball for checksum"))?;

    verify_sha256_bytes(&bytes, expected).map_err(|e| match e {
        Error::ChecksumMismatch { .. } => e,
        Error::InvalidArgument { message } => Error::InvalidArgument {
            message: format!("invalid source checksum for '{url}': {message}"),
        },
        other => other,
    })
}

async fn find_source_root(src_dir: &Path) -> Result<PathBuf, Error> {
    let mut entries = fs::read_dir(src_dir)
        .await
        .map_err(Error::file("failed to read source directory"))?;

    let mut subdirs = Vec::new();
    let mut has_files = false;

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(Error::file("failed to read directory entry"))?
    {
        let ft = entry
            .file_type()
            .await
            .map_err(Error::file("failed to get file type"))?;
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else {
            has_files = true;
        }
    }

    if subdirs.len() == 1 && !has_files {
        return Ok(subdirs.into_iter().next().unwrap());
    }

    Ok(src_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_are_the_sources_own() {
        let rb = r#"class Ncurses < Formula
  url "https://invisible-mirror.net/archives/ncurses/ncurses-6.6.tar.gz"
  mirror "https://ftpmirror.gnu.org/ncurses/ncurses-6.6.tar.gz"
  mirror "https://ftp.gnu.org/gnu/ncurses/ncurses-#{version}.tar.gz"

  resource "x" do
    url "https://example.com/x.tar.gz"
    mirror "https://example.org/x.tar.gz"
  end
end
"#;
        assert_eq!(
            source_mirrors(rb),
            ["https://ftpmirror.gnu.org/ncurses/ncurses-6.6.tar.gz"]
        );
    }

    #[test]
    fn a_stable_blocks_mirrors_count() {
        let rb = r#"class Wget < Formula
  stable do
    url "https://ftpmirror.gnu.org/wget/wget-1.25.0.tar.gz"
    mirror "https://ftp.gnu.org/gnu/wget/wget-1.25.0.tar.gz"

    patch do
      url "https://example.com/x.diff"
    end
  end
end
"#;
        assert_eq!(
            source_mirrors(rb),
            ["https://ftp.gnu.org/gnu/wget/wget-1.25.0.tar.gz"]
        );
    }
}
