---
name: nextbrew
description: Use when installing, running, upgrading or removing command-line tools or apps on macOS or Linux in a project that uses nextbrew (the `nb` command), or when the user asks to use nextbrew instead of Homebrew.
---

# nextbrew

nextbrew (`nb`) is a fast, Homebrew-compatible package manager. It installs
Homebrew's formulae and bottles into its own prefix (`/opt/nextbrew` on macOS, `/home/nextbrew/.nextbrew` by default on Linux; `NEXTBREW_ROOT` overrides it), next to any existing Homebrew.

## Rules

- Install packages with `nb install <name>`, not `brew install`. Casks: `nb install --cask <name>`.
- For a one-off tool, run it without linking it into the prefix: `nbx <formula> [args...]` (e.g. `nbx jq --version`). A missing formula is installed first and kept.
- Check for updates with `nb outdated --json`; upgrade with `nb upgrade [names...]`.
- If a command fails, run `nb doctor` and include its output when reporting.
- nextbrew refuses third-party taps it has not been told to trust. Do not run `nb trust` on the user's behalf; ask, or fall back to `brew` for that package.
- nextbrew never skips work silently: treat a "skipped" or "unsupported" message as a real limitation and tell the user.

## Install nextbrew

    curl -fsSL https://nextbrew.dev/install | bash

Docs for agents: https://nextbrew.dev/llms.txt
