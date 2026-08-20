# How it works

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
  `review_changes` tool binds a unix socket, opens the review pane via the
  herdr CLI, and blocks until the pane answers. A dead or closed pane maps
  to a `cancelled` verdict, so the agent can never be wedged.
- `herdr-annotator pane` — the review TUI, declared as a herdr plugin pane
  entrypoint. Launched by herdr, never by hand.

In guided (non-blocking) mode the same socket stays open for the life of the
review: `goto` navigation flows agent → pane over it, and the verdict flows
back whenever the reviewer decides — collected by `collect_review` instead
of a blocked tool call.

## Install layout

The plugin installs under a herdr-managed directory (e.g.
`~/.config/herdr/plugins/github/herdr-file-annotator-<hash>`); installs
download a prebuilt binary for macOS (arm64/x86_64) or Linux (x86_64/arm64,
static musl), verify its SHA-256, and fall back to building from source with
cargo if no matching prebuilt exists.

## Building from source

Requires a Rust toolchain and herdr ≥ 0.8.0.

```sh
git clone https://github.com/JonasBaeumer/herdr-file-annotator
cd herdr-file-annotator
./scripts/dev-link.sh          # builds, symlinks bin/, runs `herdr plugin link`
```

`herdr plugin link` skips the `[[build]]` step (`scripts/fetch-or-build.sh`),
so `dev-link.sh`'s own `cargo build` is what produces `bin/herdr-annotator`
here. See [CONTRIBUTING.md](../CONTRIBUTING.md) for the fork-and-PR workflow
and testing conventions.
