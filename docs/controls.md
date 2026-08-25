# Controls

Everything in the review pane is keyboard-driven, with full mouse support as
an alternative. Press `?` in the pane for the context-aware key reference —
it shows exactly the keys that work in your current view. (While a comment
or summary box is open, `?` is typed as text like any other character; close
the box first.)

![The ? key reference overlay, listing finish, file-list, and diff keys](img/help-overlay.png)

## File list

| Keys | Action |
|---|---|
| `j` / `k` | move selection down / up |
| `g` / `G` | jump to first / last file |
| `l` / `Enter` / `Tab` | open the selected file in the diff view |

Mouse: wheel moves the selection, click opens a file.

## Diff view

| Keys | Action |
|---|---|
| `j` / `k` | move the cursor line |
| `←` / `→` (or `H` / `L`) | pan long lines horizontally, `0` resets |
| `d` / `u` | half page down / up |
| `n` / `p` | next / previous hunk |
| `g` / `G` | jump to top / bottom |
| `h` / `Tab` | back to the file list |

Mouse: wheel scrolls, horizontal wheel pans, click places the cursor, drag
selects a range.

Long lines clip with `‹` / `…` markers at the edges; pan to see the rest.

## Annotating

| Keys | Action |
|---|---|
| `v` | start a line-range selection (extend with `j` / `k`) |
| `c` | open the comment box for the selection (or the cursor line) |
| `Ctrl-T` | cycle the tag: none / `verify` / `fix` / `question` / `nit` |
| `Enter` | save the annotation |
| `Esc` | back out without saving |
| `c` on an annotated line | edit the existing annotation |
| `x` | delete the annotation under the cursor |

Mouse: drag over the lines, then press `c`.

![The comment box open on a selected line, with commit, tag, and cancel chips](img/annotate-box.png)

Saved annotations appear inline, woven between the code lines, so you can
see your notes in place while you keep reading:

![Two saved annotations rendered inline between diff lines, one tagged fix and one tagged question](img/annotations-inline.png)

## Layout and views

| Keys | Action |
|---|---|
| `b` | hide / show the file list column |
| `[` / `]` | shrink / widen the file list — more room for the code (side-by-side layout only; `0`-width isn't a thing, that's what `b` is for) |
| `z` | zoom the review pane full-screen (and back) |
| `t` | toggle diff view ↔ full source view of the file |
| `?` | key-reference overlay |

Mouse: drag the divider between the file list and the code to resize the
split directly (same clamps as `[` / `]`).

The `t` toggle is global: it switches the reading mode for the whole
review, so files you navigate to open in the same view. Reviewing the diff
is the default; the source view shows the finished state of the file with
no diff noise. Annotations work in both views.

![Source view: the full finished file with syntax highlighting and no diff markers](img/source-view.png)

## Folding (source view)

Long file, but only a few parts matter right now? Collapse the rest into
`⋯ N lines folded ⋯` pills — by hand with the keys below, or agent-driven:
during a guided walkthrough the agent can call its `focus` tool to fold a
file down to the regions it's explaining (see
[MCP tools](mcp-tools.md#focus-fold-a-file-to-what-matters)). Both kinds
share the same behavior, and folding only exists in source view — the diff
already shows just its hunks.

| Keys | Action |
|---|---|
| `f` | fold the selection (`v` + movement first), or — with no selection — the indentation block under the cursor line: `f` on a `def`/`fn` header tucks the body away, the header stays visible |
| `Enter` (on a pill) | expand that fold |
| `F` | unfold everything in this file |

Mouse: clicking a pill expands it.

The block detection is indentation-based and language-agnostic (a
base-indent line opening with `)`, `]`, or `}` still belongs to the block,
so multi-line signatures fold correctly). It deliberately doesn't track any
language's real grammar — for a layout it doesn't recognize, such as a Rust
`where` clause at the header's own indent, select the lines yourself and
fold with `v` + `f`.

Pills are real cursor stops; movement skips the hidden lines. Very short
stretches (one or two lines) never fold — a pill would cost as much space
as it saves. Annotations hidden by a fold aren't lost: the pill shows a
note badge (`⋯ 24 lines folded · 1 note ⋯`), and expanding brings them
back inline. An agent `goto` into a folded stretch expands it
automatically, so you can never be pointed at something you can't see.

## Finishing the review

| Keys | Action |
|---|---|
| `a` | approve |
| `r` | request changes — opens a summary box first |
| `q` | cancel the review |
| `Esc` | clear an active selection; with nothing selected, **cancel the review** |
| `Ctrl-C` | cancel the review — works everywhere, even while typing in a box |

Cancelling is destructive: pending annotations are discarded and the agent
receives a plain `cancelled` verdict. `Esc` only cancels from the normal
browsing state — inside a comment or summary box it just closes the box (see
the annotate table above); `Ctrl-C` cancels from anywhere.

Annotations ride back to the agent on both `a` and `r` verdicts. Closing the
pane without a verdict surfaces to the agent as a normal `cancelled` result,
never an error or a hang.

![Requesting changes: the summary input in the status bar before sending the verdict](img/verdict-summary.png)
