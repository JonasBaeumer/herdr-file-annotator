# Contributing

Thanks for your interest in improving herdr-file-annotator!

## Workflow

1. **Fork** the repository and create a feature branch from `main`.
2. Make your changes. Keep commits focused; one logical change per commit.
3. Run the checks locally before pushing:
   ```sh
   cargo test
   cargo build --release
   ```
4. **Open a pull request** against `main`. Direct pushes to `main` are disabled.
5. CI (tests on Linux and macOS) must pass, and a maintainer review is
   required before merging. Workflow runs from first-time contributors need
   maintainer approval before they start.

## Development setup

Requires a Rust toolchain, [herdr](https://herdr.dev) ≥ 0.8.0, and macOS or Linux.

```sh
git clone https://github.com/<your-fork>/herdr-file-annotator
cd herdr-file-annotator
./scripts/dev-link.sh   # builds and links the checkout as a local herdr plugin
```

Register the MCP server with your agent to test the full loop end to end
(see [docs/agents.md](docs/agents.md)), or run the protocol-level harness:

```sh
python3 tests/e2e/mcp_client.py <some-git-repo-with-changes> "test note"
```

which opens a review pane in your live herdr session and prints the verdict
JSON when you close it.

## Guidelines

- `cargo build` must stay warning-free; new logic wants unit tests
  (see `src/ui.rs` and `src/diff.rs` for the pattern — pure state/parsing
  functions with focused tests, no TUI snapshot tests).
- The handoff protocol (`src/protocol.rs`) is versioned; changing the wire
  format needs a `PROTOCOL_VERSION` bump and a compatibility note in the PR.
- User-facing behavior changes should update the docs (keybindings in
  [docs/controls.md](docs/controls.md), config keys in
  [docs/configuration.md](docs/configuration.md), tool schema in
  [docs/mcp-tools.md](docs/mcp-tools.md)).
- The pane must never wedge the agent: every new failure path needs to
  resolve to a returned verdict (see the cancelled/timeout handling).

## License

By contributing you agree that your contributions are licensed under the
MIT license. Note: [annot](https://github.com/denolehov/annot) is AGPL-3.0 —
do not copy code from it into this repository.
