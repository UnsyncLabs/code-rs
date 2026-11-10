# Packaging

Distribution files that are not built by `cargo tauri build` itself.

| Path | What it is |
|---|---|
| [`arch/PKGBUILD`](arch/PKGBUILD) | Arch Linux package, repackaged from the release `.deb` (built by the `Arch Linux package` job in [release.yml](../.github/workflows/release.yml)) |
| [`claude-skill/code-rs/SKILL.md`](claude-skill/code-rs/SKILL.md) | Claude Code skill that teaches the agent to use code-rs through its MCP server |

The installers for Windows, Linux and macOS are produced by the release workflow.
