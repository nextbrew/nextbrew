<p align="center">
  <a href="https://nextbrew.dev">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="site/public/brand/mark-dark.svg">
      <img src="site/public/brand/mark.svg" alt="nextbrew" width="96">
    </picture>
  </a>
</p>

<h1 align="center">nextbrew</h1>

<p align="center">Homebrew, but fast. A Homebrew-compatible package manager written in Rust.</p>

<p align="center"><a href="https://nextbrew.dev">Website</a> · <a href="https://nextbrew.dev/en/docs">Docs</a> · <a href="https://nextbrew.dev/ja">日本語</a></p>

It installs Homebrew's official bottles and casks without needing Ruby or
Homebrew itself, and runs alongside an existing Homebrew.

## Install

```bash
curl -fsSL https://nextbrew.dev/install | bash
```

> Status: early development. Expect breaking changes before 1.0.

## Quick start

```bash
nb install jq                   # install a formula
nb install cask:ghostty         # install a cask (app, DMG, zip)
nb bundle                       # install from a Brewfile
nb uninstall jq
nb outdated
nb upgrade
nbx jq --version                # run without linking
```

## Build from source

```bash
cargo build --release
./target/release/nb --version
```

## Credits

nextbrew is not affiliated with Homebrew. Parts derived from Homebrew are
under the BSD 2-Clause license in [LICENSE-HOMEBREW](./LICENSE-HOMEBREW).

## License

MIT ([LICENSE-MIT.md](./LICENSE-MIT.md)) or Apache 2.0
([LICENSE-APACHE.md](./LICENSE-APACHE.md)), at your option.
