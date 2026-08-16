# AGENTS.md

## Review guidelines

Review strictly. Assume the diff was partly written by an AI agent and that the
author has not read it as carefully as they should have. Your job is to catch
what a fast human reviewer would wave through.

### Business logic — restate it, then confirm it

Much of this codebase encodes business rules directly in code, and those rules
are rarely written down anywhere else. Before reviewing style or structure,
work out **what rule the diff is asserting about the domain** and check whether
the author meant to assert it.

For each changed piece of logic:

1. Read the code and derive the business rule it implements, in one sentence,
   in domain terms — not a description of the control flow. "A user with no
   active subscription still keeps read access for 30 days after cancellation",
   not "the `if` checks `cancelledAt` against `now - 30d`".
2. Check that rule against everything else available: the PR description, the
   linked issue, adjacent code, existing tests, naming, comments, docs.
3. If the rule is unambiguous and consistent with the rest, say nothing.
4. **If you cannot determine the intended rule with confidence, or the code
   implies a rule that is not stated anywhere, or two parts of the diff imply
   different rules — raise it as P1 and ask the author to confirm.** Do not
   guess, and do not assume the code is correct because it is self-consistent.

Write these as a stated interpretation plus a direct question, so the author
can answer yes/no rather than reverse-engineer their own code:

> **P1 — confirm intended:** As written, an order with `status = PENDING` and
> a past `expiresAt` is still counted toward the customer's credit limit,
> because the filter on line 84 only excludes `CANCELLED`. Is that intended,
> or should expired-pending orders be released? The test on line 210 only
> covers the `CANCELLED` case, so either behaviour would pass.

Always flag these, even when the code looks deliberate:

- A boundary that could reasonably go either way and isn't pinned by a test:
  inclusive vs exclusive comparison, `>` vs `>=`, whether "within 30 days"
  includes day 30, rounding direction, timezone or day-boundary assumptions.
- Empty, zero, negative, and null cases that fall through to a default. Is the
  default the intended business answer, or an accident of the control flow?
- Ordering and precedence between rules: when two conditions both apply
  (a discount and a cap, a manual override and an automatic rule), the code
  picks one. Confirm the priority is the intended one.
- A rule that changed in this diff without the PR description saying it should.
  Behaviour changes smuggled in alongside a refactor are P1 by default —
  state the old behaviour, the new behaviour, and ask which is wanted.
- Special cases keyed on a specific ID, name, tier, or region. Ask whether the
  carve-out is intended and where it is documented.
- A magic number or threshold with no name and no source. Ask where the value
  comes from and give it a named constant.
- The same domain rule expressed in two places that could drift apart. Name
  both locations.

Once a rule is confirmed, ask for it to be recorded — as a named constant, a
test case that pins the boundary, or a one-line comment stating the rule and
why. A comment that restates the code is slop; a comment that states the
business rule the code cannot express is not.

### P0 — must block

- Logic that is wrong on a realistic input: off-by-one, inverted condition,
  wrong operator precedence, wrong variable used, swapped arguments.
- Unhandled error paths. Every fallible call either handles the failure or
  deliberately propagates it. Silent `catch {}` / `catch(e) { /* ignore */ }` /
  `let _ = fallible()` is a defect unless a comment justifies it.
- Concurrency bugs: unawaited promises, shared mutable state without
  synchronization, races between check and use.
- Resource leaks: files, sockets, DB connections, locks, subscriptions,
  timers not released on every exit path including the error path.
- Secrets, tokens, keys, or credentials in source, config, tests, or fixtures.
- PII or secret material written to logs, traces, or error messages.
- Unvalidated external input reaching a query, shell command, filesystem path,
  deserializer, or template.
- Breaking changes to a public API, CLI flag, config key, DB schema, or wire
  format with no migration path and no note in the PR description.
- Authentication or authorization checks that are missing, bypassable, or
  applied inconsistently across sibling routes/handlers.

### P1 — must be addressed before merge

- **AI slop.** Flag these explicitly, they are not nitpicks:
  - Comments that restate the code (`// increment counter` above `i += 1`),
    or narrate the change (`// Added error handling here`).
  - Defensive scaffolding for conditions that cannot occur given the types or
    the call sites — null checks on non-nullable values, `try/catch` around
    code that cannot throw, redundant guard clauses.
  - Abstractions with exactly one caller: a wrapper, interface, factory,
    strategy, or config object introduced for flexibility nobody asked for.
  - Invented configuration: new options, env vars, or feature flags not
    required by the change.
  - Duplicated logic that a nearby existing helper already covers. Say which
    helper.
  - Generic naming that hides intent: `data`, `result`, `handler`, `process`,
    `manager`, `utils`, `helper`, `temp`, `newX`.
  - Docstrings or README prose padded with filler ("robust", "seamlessly",
    "comprehensive", "leverages", "ensures that") that carries no information.
  - Emoji, decorative section banners, or marketing tone in code comments,
    commit messages, or docs.
  - Dead code, commented-out blocks, unused imports, unused parameters, or
    leftover debug prints/`console.log`.
- **Tests.** New behavior needs a test. Bug fixes need a regression test that
  fails without the fix. Flag tests that assert only that the code ran, mock
  the very thing under test, or duplicate an existing case with a new name.
- **Error messages** must say what failed and with what input. Reject
  `throw new Error("error")` and equivalents.
- **Consistency with the surrounding code.** New code should use the module's
  existing error type, logging facility, config access pattern, and naming
  conventions rather than introducing a parallel one.
- **Types.** Reject `any`, unchecked casts, and `unwrap()`/`!`/`as` used to
  silence a type or option rather than because the invariant is proven. If an
  invariant is proven, it needs a one-line comment saying why.
- **Docs and typos.** Treat typos, stale examples, and out-of-date docs in
  user-facing files (README, CONTRIBUTING, public docstrings, CLI help) as P1.
- **Dependencies.** A new dependency needs a justification in the PR
  description. Flag additions that duplicate an existing dependency or pull in
  a large tree for one function.
- **Performance regressions that are structural**, not micro: an N+1 query, a
  loop that reloads the same data, an O(n²) pass over an unbounded collection,
  a synchronous call on a hot async path.

### How to write the review

- Lead with the most serious issue. Do not open with a summary of what the PR
  does — the author knows.
- Every comment names the concrete failure: the input, the sequence, or the
  caller that breaks. "Consider handling errors" is not a review comment;
  "if `fetchUser` rejects here the request hangs, since `next` is never
  called" is.
- Business-logic questions follow the same rule. State your reading of the
  rule and the specific line it comes from, then ask one closed question.
  "Is this logic correct?" is not a review comment.
- Uncertainty about intent is a legitimate finding, not a gap in your review.
  Raising it is better than picking the more likely reading and staying quiet.
  But raise it once, on the line that decides the rule — not on every line
  that touches it.
- Suggest the fix when it is short. Do not paste large rewrites.
- Do not comment on formatting the linter or formatter already enforces.
- No praise, no "LGTM overall", no summary of strengths. If there is nothing
  to flag, say so in one line.

### Rust specifics

- **P0** — `println!`/`print!` anywhere reachable from `mcp::run`. stdout
  carries newline-delimited JSON-RPC frames only; every log in `mcp.rs` and
  `herdr.rs` goes to stderr via `eprintln!`. One stray stdout write corrupts
  the transport for the whole session.
- **P0** — a `Command` whose `output()` result is used without checking
  `status.success()`. `output()` returning `Ok` only means the process ran;
  `diff::load` and `herdr::open_review_pane` both branch on the status and
  surface `stderr`. New subprocess calls must do the same.
- **P0** — a field added to or removed from `ReviewRequest`, `ReviewResult`,
  `Annotation`, or `LineRange` without either `#[serde(default)]` or a bump of
  `PROTOCOL_VERSION`. Server and pane are separate processes that can be built
  from different commits; `receive_request` rejects on version mismatch only.
- **P0** — a return or `?` between `UnixListener::bind` and the terminal
  restore / socket cleanup in `mcp::run_review`. The current code removes the
  socket before propagating the error; an early exit leaks a socket file into
  the temp dir on every failed review.
- **P0** — an exit path in the ratatui/crossterm pane that does not leave raw
  mode and the alternate screen. This includes panics and the error path, not
  just the normal quit. A pane that dies in raw mode leaves the user's
  terminal unusable.
- **P0** — `config::load` gaining a path that returns `Err` or panics. Its
  contract is that a missing, unreadable, malformed, or invalid config falls
  back to `Config::default()` after one stderr warning. A bad config file must
  never stop the MCP server from starting.
- **P1** — a new config key added to `Config` without the matching field in
  `RawConfig`, a branch in `validate`, a default in `Default`, and a test.
  `RawConfig` is `#[serde(deny_unknown_fields)]`, so a key documented but not
  declared makes every config file carrying it fall back to full defaults.
- **P1** — `?` on an IO, subprocess, or socket call without `.context(...)` /
  `.with_context(...)`. The convention throughout is that the error names the
  resource: "binding handoff socket {path}", "spawning herdr CLI".
- **P1** — a new dependency that pulls in a C library. `syntect` is pinned to
  `default-features = false, features = ["default-fancy"]` specifically to
  avoid oniguruma. Re-enabling default features counts.
- **P1** — a parser in `diff.rs` that returns `None` on malformed input
  without a comment saying why dropping that input is correct. `parse_hunk`,
  `parse_hunk_header`, and `parse_range` all silently discard; a new one that
  does the same hides git output the reviewer then never sees.

### Business logic in this repo

- `Verdict::Cancelled` collapses four different outcomes: the user quit
  without deciding, the pane crashed, the socket hit EOF, and the review
  timed out (`protocol.rs`, `exchange` maps both EOF and `WouldBlock`/
  `TimedOut` to it). Nothing says whether the calling agent should treat
  Cancelled as "human declined, stop" or "review unavailable, proceed". A
  diff that adds a fifth cause, or that makes the agent act on Cancelled,
  needs the intended reading stated.
- `review_timeout: None` is the default (`config.rs`), and it means block
  forever. An absent config key therefore grants unlimited blocking of the
  agent, while `review_timeout_secs = 0` is rejected as invalid. Confirm that
  absent means unlimited rather than "use a sane cap", and flag any change
  that alters which of the two an unset key resolves to.
- `accept_timeout` and `review_timeout` cover different phases — pane
  connecting versus human deciding — and only the first has a default bound.
  A pane that connects but never renders is covered by neither unless
  `review_timeout` is set. Confirm which timeout is meant to own that state
  before extending either.
- `LineRange { start, end }` has no stated inclusivity, and `Side::Old` vs
  `Side::New` decides which file's numbering the range indexes. The
  round-trip test asserts `start == 3` and nothing about `end`, so an
  off-by-one at either boundary passes today. Any diff touching range
  construction or rendering must state whether `end` is inclusive.
- `Annotation.tag` is an `Option<String>` whose vocabulary — "verify", "fix",
  "question", "nit" — exists only in a doc comment and is never validated.
  Confirm what an absent tag means (untagged, or defaulted to one of the
  four) and whether an unrecognised tag is an error or passed through.
- `Verdict::RequestChanges` and `Verdict::Reject` are distinct variants with
  no stated difference in consequence. If a diff makes the agent branch on
  one of them, ask what separates them — otherwise the distinction is
  decoration and the two will drift apart in meaning.
