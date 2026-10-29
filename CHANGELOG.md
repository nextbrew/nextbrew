# Changelog

All notable changes to nextbrew will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3] - 2026-10-29

### Added
- `nb leaves`, `cleanup`, `fetch`, `shellenv`, `config`, `home`, `source`, `desc`, `vulns` and `man`, `--prefix`, `--cellar` and `--cache`, and installing an older version (`version-install`), as in `brew`
- `--greedy`, `--greedy-latest` and `--greedy-auto-updates` for `outdated` and `upgrade`
- `nb bundle` installs flatpak and winget entries, and a `brew` entry's `args` (`build-from-source`)
- Casks install in the language asked for with `--language` or `$HOMEBREW_CASK_OPTS`, else the system's, as Homebrew does

### Changed
- Installs are faster: bottles are unpacked on several threads, only the files a bottle's tab lists are relocated, only executables, dylibs and bundles are signed, links are made in parallel, and the first bottle no longer waits for a GHCR token
- `nb upgrade` downloads everything at once, keeps its output tidy, and `--quiet` hides the progress bars
- `nb uninstall` prints each failure once
- `nb services` on a Linux without a running systemd (a container, WSL without `systemd=true`) says so, and how to turn it on in WSL
- Intel macOS is no longer supported: Homebrew ships no bottles for it, so everything builds from source. The `darwin-x64` binary is still built, untested

### Fixed
- Third-party tap formulae that shell out in their class methods or use class variables load (ariga/tap's atlas, azure-functions-core-tools@4), and a one-string `popen_read` runs in a shell (pulumi)
- Building from source: Rust and Go formulae on Linux, patches and resources from their mirrors when a download fails or its checksum differs, retries on network errors, and more of Homebrew's formula API (python@3.15, ruby, git, lua, gmp, go, jemalloc)
- A Java class file is no longer taken for a fat Mach-O file (gettext failed to install)
- On Linux, every hard link of a patched ELF file is patched (perl's `bin/perl` kept a placeholder interpreter)
- Dependencies: block-buffer 0.12.1 (GHSA-qwgh-2vcv-g2f7), zstd 0.14.3 (RUSTSEC-2026-0391 and 0395-0406)

## [0.1.2] - 2026-10-25

### Changed
- Installs are faster: kegs are cloned and linked in parallel, ad-hoc signatures on macOS are rehashed in place instead of running `codesign`, and an unshared `share/man` is linked as one directory

### Fixed
- The Linux binaries run on glibc 2.17 and later; they needed glibc 2.34, so they failed on Ubuntu 20.04 and Zorin OS 16
- `install.sh` shows why `nb` failed to run instead of only saying it did not execute properly
- Installing anything that depends on `ca-certificates` (openssl, curl, git, node, ...) on macOS no longer takes about 12 seconds longer since its 2026-10-20 release

## [0.1.1] - 2026-10-23

### Fixed
- `nb init` on Linux asks for sudo to create `/home/nextbrew/.nextbrew` instead of failing with "Permission denied"

## [0.1.0] - 2026-10-23

First release of nextbrew, as `nb` and `nbx`.

### Added
- Casks: every artifact Homebrew installs (apps, pkg and installer, suites, DMGs with a license agreement, `app_image` on Linux), `depends_on`, conflicts, quarantine, `uninstall` and `--zap`, `outdated` and `upgrade`, and cask options such as `--appdir` and `--require-sha`
- Third-party taps: formulae and casks from taps, including their `post_install` steps
- `nb bundle` reads Brewfiles as `brew bundle` does: `install`, `check`, `list`, `dump`, `cleanup`, `add`, `remove`, `exec`, `sh` and `env`, with `mas`, `vscode`, `go`, `cargo`, `uv`, `npm` and `krew` entries
- `nb migrate` takes over formulae, casks, pins and running services from an existing Homebrew
- `nb services` for launchd, `nb doctor`, `nb search`, `nb link` and `unlink`, `pin` and `unpin`, `autoremove`, `deps` and `uses`, `reinstall` and `postinstall`
- Post-install steps from Homebrew's API, and caveats after install, upgrade and reinstall
- Source builds of core formulae in an environment like Homebrew's
- Deprecated formulae and casks warn; disabled ones are refused

### Changed
- Installs into its own prefix (`/opt/nextbrew` on macOS, `/home/nextbrew/.nextbrew` by default on Linux), next to any existing Homebrew

### Fixed
- `nb init` puts the directory the installer used (`NEXTBREW_BIN`) on `PATH`
