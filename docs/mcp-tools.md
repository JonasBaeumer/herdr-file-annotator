# MCP tools

Five tools, one shared review protocol. `review_changes` blocks the agent
until a verdict lands. `show_changes` + `goto` + `focus` + `collect_review`
don't: they let the agent open the pane, narrate the diff in chat while
pushing navigation, and come back for the verdict whenever it's ready.
Annotations work exactly the same way in both modes, and the reviewer can
also just finish the review in the pane at any time, without waiting to be
asked.

| Tool | Blocks? | Arguments | Returns |
|------|---------|-----------|---------|
| `review_changes` | Yes | `baseline?`, `note?`, `working_dir?` | Verdict + annotations (below), once the human decides. |
| `show_changes` | No | `baseline?`, `note?`, `working_dir?` | `{"opened": true, "working_dir": "..."}` immediately. |
| `goto` | No | `file` (repo-relative, new/post-change side), `line` (1-based), `view` (optional: `diff` or `source`) | Confirmation text; navigation is advisory. |
| `focus` | No | `file`, `regions` (array of 1-based inclusive `{start, end}`; empty clears) | Confirmation text; advisory like `goto`. |
| `collect_review` | No, polls | `wait_seconds?` (0–120, default 0) | Verdict + annotations once landed, else `{"status": "pending", "open_for_secs": N}`. |

`baseline`, `note`, and `working_dir` mean the same thing for `review_changes`
and `show_changes`:

| Argument      | Type   | Meaning                                                            |
|---------------|--------|--------------------------------------------------------------------|
| `baseline`    | string | Git rev to diff against. Omit for all uncommitted changes vs HEAD. |
| `note`        | string | Message shown to the reviewer ("please check the retry logic").    |
| `working_dir` | string | Repo to review. Defaults to the server's working directory.        |

Only one review — blocking or guided — can be open at a time: `review_changes`
and `show_changes` each refuse to start a second one, and `goto` / `focus` /
`collect_review` refuse to run without one already open.

## Verdict result

JSON in the tool response from `review_changes`, or from a `collect_review`
call once a verdict has landed:

```json
{
  "version": 2,
  "verdict": "request_changes",
  "summary": "retry loop still swallows the error",
  "annotations": [
    { "file": "src/portal.rs", "lines": { "start": 112, "end": 118 },
      "side": "new", "tag": "fix", "comment": "handle the None case" }
  ]
}
```

`verdict` is one of `approve`, `request_changes`, `reject`, or `cancelled`.
The pane's finish keys produce `approve`, `request_changes`, and
`cancelled`; `reject` is also part of the wire schema, so consumers must
accept it and should treat it as a hard no — do not proceed with the
change. Each annotation carries the file, an inclusive 1-based line range,
the diff side, an optional tag (`fix` / `verify` / `question` / `nit`), and
the reviewer's comment. A note saved without choosing a tag omits the `tag`
field entirely — treat it as an untagged comment.

## Timeouts

`review_changes` blocks for as long as the config's `review_timeout_secs`
allows (unset = forever). The non-blocking tools have no such timeout —
nothing is blocked, so there's nothing to time out; the review stays open
until the reviewer finishes in the pane (or closes it). `collect_review`
only ends the review when a verdict has actually landed — a `pending` result
leaves the pane open, and the agent simply collects again later.

A dead or closed pane always maps to a `cancelled` verdict, so the agent can
never be wedged by a closed pane or timeout.

## Guided walkthroughs

Use `show_changes` instead of `review_changes` when you want the agent to
talk you through a diff rather than hand it over cold and wait:

1. **Open it, non-blocking.** `show_changes(working_dir=..., note="new retry
   logic")` opens the review pane and returns immediately — the agent keeps
   talking in chat instead of freezing on a verdict.
2. **Navigate while explaining.** The agent calls
   `goto(file="src/retry.rs", line=42)` as it describes each piece; the pane
   jumps to follow along. Passing `view: "source"` shows the full file for
   context, `view: "diff"` returns to the change. For a long file, `focus`
   (below) folds it down to just the regions under discussion. You annotate
   as usual — annotations work exactly as they do in blocking mode.
3. **Collect the verdict when ready.** `collect_review()` checks once;
   `wait_seconds` polls for a bit instead of hand-rolling a retry loop.
   Nothing landed yet returns `{"status": "pending", ...}` — that's normal,
   the agent just calls it again later. You can also finish in the pane on
   your own schedule at any point; a pane closed without a decision surfaces
   as a normal `cancelled` verdict, not an error.

![The review pane mid-walkthrough: agent-driven navigation landed on the retry loop, with two annotations already left inline](img/annotations-inline.png)

A prompt that triggers this end to end, with no special setup:

> Open the changes with show_changes and walk me through them file by file —
> use goto to jump me to each part as you explain it, then collect my review
> at the end.

## Focus: fold a file to what matters

`focus(file, regions)` folds the source view of one file down to just the
named line regions — for walkthroughs of long files where only a few
functions matter. Everything between the regions collapses into
`⋯ N lines folded ⋯` pills; the pane switches to that file's source view
and lands on the first region.

- `regions` is an array of 1-based inclusive `{start, end}` ranges on the
  post-change file, in the order the agent wants to discuss them. Gaps of
  one or two lines between regions stay visible.
- Calling `focus` again with new regions refocuses; an **empty** `regions`
  array clears the agent's focus (the reviewer's cursor stays put). Folds the
  reviewer made by hand with `f` are untouched by this — `F` clears those.
- The reviewer is never trapped: pills expand with `Enter` or a click, `F`
  reveals the whole file, and a later `goto` into a folded stretch expands
  it automatically. The reviewer can also fold regions by hand with `f` —
  see [Controls — folding](controls.md#folding-source-view).
- Advisory like `goto`: unknown files and out-of-range regions are
  normalized or ignored by the pane, never an error. Annotations hidden by
  a fold surface as a note badge on the pill and ride back with the verdict
  regardless.
