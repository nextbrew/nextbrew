# nextbrew

A Homebrew-compatible package manager written in Rust. It installs
Homebrew's official bottles and casks without needing Ruby or Homebrew
itself.

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

nextbrew started as a fork of [zerobrew](https://github.com/zerobrewhq/zerobrew)
and keeps its history. zerobrew is dual-licensed under MIT and Apache 2.0,
and so is nextbrew. Parts derived from Homebrew are under the BSD 2-Clause
license in [LICENSE-HOMEBREW](./LICENSE-HOMEBREW).

## License

MIT ([LICENSE-MIT.md](./LICENSE-MIT.md)) or Apache 2.0
([LICENSE-APACHE.md](./LICENSE-APACHE.md)), at your option.
