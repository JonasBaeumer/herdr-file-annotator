# Agent setup

The plugin's MCP server is a plain stdio server — any MCP-capable agent can
use it. The only real requirement is that the agent runs **inside a herdr
pane**, because that's where the review pane opens next to it.

Every variant below needs the path to the installed plugin; find it with:

```sh
herdr plugin list --plugin jonasbaeumer.file-annotator --json
```

and use its root as `<plugin_root>` in the snippets.

## Claude Code

```sh
claude mcp add annotator -- "<plugin_root>/bin/herdr-annotator" mcp
```

## Codex CLI

```sh
codex mcp add annotator -- "<plugin_root>/bin/herdr-annotator" mcp
```

or directly in `~/.codex/config.toml`:

```toml
[mcp_servers.annotator]
command = "<plugin_root>/bin/herdr-annotator"
args = ["mcp"]
```

## Gemini CLI

```sh
gemini mcp add annotator "<plugin_root>/bin/herdr-annotator" mcp
```

Note: Gemini registers project-scoped by default; add `-s user` to register
it once for all projects.

## Cursor CLI

The Cursor CLI (`agent`, formerly `cursor-agent`) shares the editor's MCP
config. Add to `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (project):

```json
{
  "mcpServers": {
    "annotator": {
      "command": "<plugin_root>/bin/herdr-annotator",
      "args": ["mcp"]
    }
  }
}
```

Check it's picked up with `agent mcp list`.

## Others

Anything that can launch a stdio MCP server works the same way: command
`<plugin_root>/bin/herdr-annotator`, single argument `mcp`. (One notable
exception: [pi](https://github.com/badlogic/pi-mono) has no MCP support —
it would need a pi extension wrapping the binary instead.)

## Telling the agent when to ask for review

Put a standing instruction in your agent's context file — `CLAUDE.md`
(Claude Code), `AGENTS.md` (Codex, Cursor), `GEMINI.md` (Gemini CLI):

> Before marking any task complete, call the `review_changes` tool and act on
> the returned annotations. Do not proceed on a `request_changes` or `reject`
> verdict without addressing the feedback.

For guided walkthroughs instead of (or on top of) the blocking gate, see
[MCP tools — guided walkthroughs](mcp-tools.md#guided-walkthroughs).
