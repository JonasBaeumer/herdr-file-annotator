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
| `notify_on_verdict` | `true` | Nudge the agent (a short prompt typed into its pane) when a non-blocking review finishes with no `collect_review` waiting — see [MCP tools](mcp-tools.md#automatic-continuation-the-verdict-nudge) |
| `wrap_lines` | `false` | Start the pane with long lines wrapped to the pane width instead of clipped-and-pannable — `w` toggles it live either way, see [Controls](controls.md#diff-view) |
| `[keys]` | all defaults | Remap the pane's keybindings by action name — see [Custom keybindings](#custom-keybindings) below |

Example — open reviews as a tab, and auto-cancel anything left open for an
hour:

```toml
placement = "tab"
review_timeout_secs = 3600
```

The config is read when the MCP server starts, so changes apply after
restarting your agent (or reconnecting its MCP servers — `/mcp` in Claude
Code). The exceptions are `wrap_lines` and `[keys]`: the review pane reads
them when it opens, so a change applies from the next review without a
restart.

## Custom keybindings

Every character key in the pane can be remapped in the `[keys]` table.
Name the action, give it a key:

```toml
[keys]
comment = "a"      # single character; case means shift ("G" ≠ "g")
approve = "ctrl+y" # or ctrl+<letter> (a-z; ctrl+i and ctrl+m are reserved)
wrap = "W"
```

A remapped action releases its default key. Actions live in a context —
the file list, the diff pane, or both — and two actions can share a key
only if their contexts never overlap. Any invalid entry (unknown action,
reserved key, collision) prints one warning and the pane falls back to
ALL default bindings, so a typo can never half-apply.

Deliberately not remappable, so the escape hatches keep working under any
config: `ctrl+c` (cancel, everywhere), `esc`, `enter`, `tab`, the arrow
keys and `pgup`/`pgdn` (fixed aliases of the movement and pan actions),
and everything typed while a comment or summary box is open. The `?`
overlay always shows your active bindings.

| Action | Default | Context | Does |
|---|---|---|---|
| `approve` | `a` | global | approve the review |
| `request_changes` | `r` | global | request changes (opens the summary box) |
| `cancel` | `q` | global | cancel the review |
| `help` | `?` | global | toggle the key-reference overlay |
| `toggle_files` | `b` | global | show / hide the file list |
| `zoom` | `z` | global | zoom the pane |
| `files_narrower` | `[` | global | shrink the file list |
| `files_wider` | `]` | global | widen the file list |
| `down` | `j` | global | move down |
| `up` | `k` | global | move up |
| `top` | `g` | global | jump to the first row |
| `bottom` | `G` | global | jump to the last row |
| `open` | `l` | files | open the file / toggle a folder |
| `half_page_down` | `d` | diff | half page down |
| `half_page_up` | `u` | diff | half page up |
| `next_hunk` | `n` | diff | next hunk |
| `prev_hunk` | `p` | diff | previous hunk |
| `pan_left` | `H` | diff | pan left |
| `pan_right` | `L` | diff | pan right |
| `pan_reset` | `0` | diff | reset the pan |
| `wrap` | `w` | diff | wrap long lines / back to clip-and-pan |
| `to_files` | `h` | diff | focus the file list |
| `select` | `v` | diff | start / clear a line selection |
| `comment` | `c` | diff | open the comment box |
| `delete_annotation` | `x` | diff | delete the annotation under the cursor |
| `toggle_view` | `t` | diff | toggle diff / source view |
| `fold` | `f` | diff | fold the selection / block (source view) |
| `unfold_all` | `F` | diff | unfold the file (source view) |
