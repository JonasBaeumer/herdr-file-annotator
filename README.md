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

## Install (local development)

Requires a Rust toolchain and herdr ≥ 0.8.0.

```sh
git clone https://github.com/JonasBaeumer/herdr-file-annotator
cd herdr-file-annotator
./scripts/dev-link.sh          # builds, symlinks bin/, runs `herdr plugin link`
```

Then register the MCP server with your agent (Claude Code shown):

```sh
claude mcp add annotator -- "$PWD/bin/herdr-annotator" mcp
```

Finally, tell the agent when to ask for review — e.g. in `CLAUDE.md`:

> Before marking any task complete, call the `review_changes` tool and act on
> the returned annotations. Do not proceed on a `request_changes` or `reject`
> verdict without addressing the feedback.

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

**M1 (walking skeleton)** — the full blocking loop works end-to-end: colored
scrollable diff (including untracked files), verdict keys, optional one-line
summary on `request_changes`. Line-anchored annotations (`v` select, `c`
comment, tags) are next; see the roadmap in the project plan. Release packaging
for `herdr plugin install` comes after that.

## License

MIT. No code from annot (AGPL-3.0) is used; it is a behavioral reference only.
