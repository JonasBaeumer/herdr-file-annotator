# Configuration

All configuration is optional — the defaults give a right-hand split pane
beside the agent with no review timeout.

Create `config.toml` in the plugin's config directory; find it with:

```sh
herdr plugin config-dir jonasbaeumer.file-annotator
```

| Key | Default | Meaning |
|-----|---------|---------|
| `placement` | `"split"` | `split` (beside the agent) or `tab` |
| `direction` | `"right"` | Split direction: `right` or `down` |
| `focus` | `true` | Move keyboard focus to the review pane when it opens |
| `accept_timeout_secs` | `20` | How long the agent waits for the pane to appear |
| `review_timeout_secs` | unset | If set, a review left open this long returns a `cancelled` verdict |

Example — open reviews as a tab, and auto-cancel anything left open for an
hour:

```toml
placement = "tab"
review_timeout_secs = 3600
```

The config is read when the MCP server starts, so changes apply after
restarting your agent (or reconnecting its MCP servers — `/mcp` in Claude
Code).
