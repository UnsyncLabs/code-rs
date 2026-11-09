---
name: code-rs
description: Work with the code-rs editor through its MCP server - read the open workspace and diagnostics, propose edits with a diff, audit extensions before installing, and check shell commands with the code-rs guardian before running them. Use when the user mentions code-rs or when the code-rs MCP server is connected.
---

# code-rs MCP

code-rs (a VS Code fork on Tauri) exposes a local MCP server. It is **off by default**:
the user enables it in Settings > code-rs MCP, and the MCP panel (`Ctrl+Alt+Shift+M`)
shows how to connect. Clients that use a command run `code-rs --mcp`.

## Rules

- **Read before you change.** `editor_state`, `file_read`, `file_list` and `diagnostics_get`
  tell you what the user is looking at and what is broken.
- **Change files only with `edit_propose`.** The user sees a diff and accepts or rejects;
  accepted edits can be undone. Prefer `oldText`/`newText` with enough context to be unique.
  A rejection is an answer: do not retry the same change.
- **Extensions:** run `extensions_audit` (and `assistant_review_extension` when a reviewer is
  configured) before `extensions_install`. Record your opinion with `extensions_review_submit`
  so the user sees it in the install dialog. Static findings are signals, not proof.
- **Shell commands:** call `command_assess` before running anything destructive, also in your
  own terminal. With `terminal_run`, give a `reason`; the user's approval mode decides whether
  they are asked, and high risk commands normally always ask. Never try to split or disguise a
  command to get a lower risk.
- **Messages from the user:** the user can send you a message from code-rs ("Ask the AI
  (MCP)..."). Check `user_messages` at the start of a task and when the user says they sent
  you something.
- **Ask, don't guess:** use `ui_ask` for decisions only the user can make, `ui_status` for
  progress on long tasks.
- Files outside the workspace folders are not accessible, and an untrusted workspace is
  read-only.

## Prompts

The server also provides MCP prompts: `review-extension`, `fix-diagnostics`, `safe-command`.
