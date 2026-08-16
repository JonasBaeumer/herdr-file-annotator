# herdr-file-annotator

Agent-summoned, **blocking** diff review inside [herdr](https://herdr.dev).

Your coding agent reaches a checkpoint and calls one MCP tool — `review_changes`.
A review pane opens beside it in your herdr workspace showing only what changed.
The agent is **frozen** until you scroll the diff, leave feedback, and pick a
verdict. The tool call then returns your review as structured JSON the agent can
act on.

This is the inverse of [herdr-reviewr](https://github.com/persiyanov/herdr-reviewr)
(human-initiated, agent keeps running); both can coexist. The interaction model
is inspired by [annot](https://github.com/denolehov/annot), rebuilt as a native
herdr citizen.

## How it works

One Rust binary, two modes:

```
┌─ agent pane ─────────────┐   ┌─ review pane (this plugin) ─┐
│ Claude Code              │   │ colored, scrollable diff     │
│  └─ herdr-annotator mcp  │   │ a approve · r request        │
│     (MCP stdio server)   │   │ changes · q cancel           │
└──────────┬───────────────┘   └──────────────▲───────────────┘
           │  herdr plugin pane open --env ANNOT_SOCKET=…
           └──────────────────────────────────┘
              verdict + annotations flow back over the unix
              socket and become the blocked tool call's result
```

- `herdr-annotator mcp` — MCP stdio server registered with your agent. Its
  `review_changes` tool binds a unix socket, opens the review pane via the herdr
  CLI, and blocks until the pane answers. A dead or closed pane maps to a
  `cancelled` verdict, so the agent can never be wedged.
- `herdr-annotator pane` — the review TUI, declared as a herdr plugin pane
  entrypoint. Launched by herdr, never by hand.

## Install

Requires herdr ≥ 0.8.0.

```sh
herdr plugin install JonasBaeumer/herdr-file-annotator
```

Downloads a prebuilt binary for macOS (arm64/x86_64) and Linux (x86_64/arm64, static musl)
matching the release, verifies its SHA-256, and falls back to building from source with cargo
if no matching prebuilt exists.

Then register the MCP server with your agent (Claude Code shown). `herdr plugin install`
places the plugin under a herdr-managed directory (e.g.
`~/.config/herdr/plugins/github/herdr-file-annotator-<hash>`) rather than your working
directory, so look it up with `herdr plugin list` (or `herdr plugin list --plugin
jonasbaeumer.file-annotator --json` for the exact `plugin_root` field), then:

```sh
claude mcp add annotator -- "<plugin_root>/bin/herdr-annotator" mcp
```

Finally, tell the agent when to ask for review — e.g. in `CLAUDE.md`:

> Before marking any task complete, call the `review_changes` tool and act on
> the returned annotations. Do not proceed on a `request_changes` or `reject`
> verdict without addressing the feedback.

## Install (local development)

Requires a Rust toolchain and herdr ≥ 0.8.0.

```sh
git clone https://github.com/JonasBaeumer/herdr-file-annotator
cd herdr-file-annotator
./scripts/dev-link.sh          # builds, symlinks bin/, runs `herdr plugin link`
```

`herdr plugin link` skips the `[[build]]` step (`scripts/fetch-or-build.sh`), so
`dev-link.sh`'s own `cargo build` is what produces `bin/herdr-annotator` here.

## Configuration

Optional `config.toml` in the plugin's config dir (find it with `herdr plugin config-dir jonasbaeumer.file-annotator`):

| Key | Default | Meaning |
|-----|---------|---------|
| `placement` | `"split"` | `split` (beside the agent) or `tab` |
| `direction` | `"right"` | Split direction: `right` or `down` |
| `focus` | `true` | Move keyboard focus to the review pane when it opens |
| `accept_timeout_secs` | `20` | How long the agent waits for the pane to appear |
| `review_timeout_secs` | unset | If set, a review left open this long returns a `cancelled` verdict |

## The `review_changes` tool

| Argument      | Type   | Meaning                                                            |
|---------------|--------|--------------------------------------------------------------------|
| `baseline`    | string | Git rev to diff against. Omit for all uncommitted changes vs HEAD. |
| `note`        | string | Message shown to the reviewer ("please check the retry logic").    |
| `working_dir` | string | Repo to review. Defaults to the server's working directory.        |

Result (JSON in the tool response):

```json
{
  "version": 1,
  "verdict": "request_changes",
  "summary": "retry loop still swallows the error",
  "annotations": [
    { "file": "src/portal.rs", "lines": { "start": 112, "end": 118 },
      "side": "new", "tag": "fix", "comment": "handle the None case" }
  ]
}
```

## Status

Released and feature-complete for the core loop: agent-summoned blocking
review, two-pane syntax-highlighted diff viewer with full mouse support,
line-anchored annotations (range select, tags, inline comments), a config
file, and prebuilt, checksum-verified binaries for macOS and Linux via
`herdr plugin install`. Listed on the herdr marketplace.

See the [releases](https://github.com/JonasBaeumer/herdr-file-annotator/releases)
for per-version changes; ongoing work happens in pull requests.

## License

MIT. No code from annot (AGPL-3.0) is used; it is a behavioral reference only.
