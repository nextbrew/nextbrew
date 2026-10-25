# Changelog

All notable changes to nextbrew will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
