# herdr-file-annotator

**Maximize agentic development but don't lose touch with the actual codebase.**

Working with coding agents usually forces a choice: adopt an IDE-style tool
and leave your terminal workflow behind, or stay in the terminal and squint
at raw diffs and verbose agent prose in chat. This [herdr](https://herdr.dev)
plugin removes the choice, when you and your agent need to meet over actual
code, a review pane opens right in your workspace, and closes when you're
done.

![Demo: the agent opens a review pane, walks through the diff while the pane follows, and collects two line-anchored annotations](docs/demo.gif)

**Annotate the code, not the prompt.** No more dictating "change file X, line
Z…" into the prompt. Select the lines in the pane, write the note, tag it
(`fix` / `verify` / `question` / `nit`) — it flows back to the agent as
structured, line-anchored output it acts on directly.

**Let the agent walk you through its changes.** In the era of 1000-line
diffs, reading everything doesn't scale, and agent prose summaries are a
poor substitute for the code itself. Here the agent drives the pane: it
jumps you to exactly the parts worth seeing while explaining in chat. Ask
questions, ask for the next spot — you only see what you need to see.

**Diff or finished state, just a toggle away.** Reviewing the changes is the default;
press `t` when you just want to read the final version of the file, no diff
noise.

**A real sign-off gate, if you need it.** In blocking mode the agent is
*frozen* until your verdict (approve, request changes, or cancel) so
nothing runs past your review.

Also in the box: full mouse support, syntax-highlighted diffs, a `?`
key-reference overlay, one-command install with checksum-verified binaries,
and an agent that can never be wedged by a closed pane or timeout.

## Quick start

Requires herdr ≥ 0.8.0.

```sh
# 1. Install the plugin
herdr plugin install JonasBaeumer/herdr-file-annotator

# 2. Register the MCP server with your agent (Claude Code shown) —
#    find <plugin_root> via: herdr plugin list --plugin jonasbaeumer.file-annotator --json
claude mcp add annotator -- "<plugin_root>/bin/herdr-annotator" mcp
```

Then tell the agent when to ask for review — e.g. in `CLAUDE.md`:

> Before marking any task complete, call the `review_changes` tool and act on
> the returned annotations. Do not proceed on a `request_changes` or `reject`
> verdict without addressing the feedback.

That's the whole setup.

## Docs

- [Controls](docs/controls.md) — every key and mouse action in the pane
- [MCP tools](docs/mcp-tools.md) — the four tools, the verdict format, and
  guided walkthroughs
- [Configuration](docs/configuration.md) — pane placement, focus, timeouts
- [How it works](docs/architecture.md) — the two-mode binary, install
  layout, building from source

## Related projects

This is the inverse of [herdr-reviewr](https://github.com/persiyanov/herdr-reviewr)
(human-initiated, agent keeps running); both can coexist. The interaction model
is inspired by [annot](https://github.com/denolehov/annot), rebuilt as a native
herdr citizen.

## Status

Released and feature-complete for the core loop: agent-summoned blocking
review, guided walkthroughs, two-pane syntax-highlighted diff viewer with
full mouse support, line-anchored annotations, a config file, and prebuilt,
checksum-verified binaries for macOS and Linux. Listed on the herdr
marketplace.

See the [releases](https://github.com/JonasBaeumer/herdr-file-annotator/releases)
for per-version changes; ongoing work happens in pull requests.
Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. No code from annot (AGPL-3.0) is used; it is a behavioral reference only.
