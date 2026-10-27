# ADR 0024: Structured User Interaction

Status: accepted

## Decisions

### Q1 One built-in tool, host injects Prompter

- Tool name is `ask_user`, implemented in `radian-coding`. The host attaches it with `with_prompter(Arc<dyn Prompter>)`. A host without a Prompter **does not register** this tool.
- `Prompter`, `QuestionRequest`, `QuestionAnswer`, and validation live in `radian-coding`; the host implements the trait. The `Prompter` shape matches `Approver`: `fn ask(&self, QuestionRequest) -> BoxFuture<'_, Result<QuestionAnswer, PromptError>>`.
- Approval and questions **share transport** (the same channel into UI and the same pending queue, ADR 0023 U7), but **never share result types**.
- The tool races the Prompter answer against `cx.cancellation` with `select!`. A cancellation first means `ToolFailure { code: "cancelled" }`. It **never** builds a fake answer.
- `concurrency: ToolConcurrency::Exclusive`: it runs alone in its batch after earlier calls finish. One session holds at most one pending question. Two `ask_user` calls in one batch are asked one by one.

### Q2 Question shape

```json
{
  "questions": [
    {
      "id": "loader",
      "prompt": "How should the UI extension be loaded?",
      "kind": "single",
      "options": [
        { "id": "builtin", "label": "Built-in Rust components", "description": "Recommended for the first release", "recommended": true },
        { "id": "process", "label": "External process plugins" }
      ],
      "allow_other": true,
      "required": true,
      "header": "loader"
    }
  ]
}
```

- Limits (host validates; over limit means `invalid_arguments`, no truncation): at most 8 questions per request; at most 12 options per question; `id` unique in one request and matching `[a-z0-9_-]{1,32}`; `prompt` at most 500 characters, `label` at most 80, `description` at most 200, `header` at most 16 display widths (CJK counts 2; defaults to `id`). The `text` kind carries no `options`.
- The host adds `request_id` (unique in this process) and `origin: SessionId` to the request. The UI title shows the source session.
- `recommended: bool` shows ` (Recommended)`. Single choice allows at most one; more than one is `invalid_arguments`.
- The tool description tells the model: `header` is two or three words; mark only the truly recommended item as recommended.

### Q3 Answer shape and landing

`ToolOutput.structured`:

```json
{
  "request_id": "q-17",
  "answers": [
    { "id": "loader", "selected": ["builtin"] },
    { "id": "name", "text": "radian" },
    { "id": "tiers", "selected": ["safe", "workspace"], "other": "a custom tier", "note": "…" }
  ]
}
```

- `ToolOutput.content` also carries a human-readable text render (one line per question "question → answer").
- An answer lands as `ToolExecutionSettled.result`; the question is `ToolExecutionStarted.arguments`. No new runtime event, no inbox, no `InputSource` exists. An answer is a tool result, not user input.
- Each question can carry `note` (at most 500 characters). It omits when empty.

### Q4 Lifecycle and end states

| Case | Result | Who guarantees it |
|---|---|---|
| User completes all required questions and submits | `ToolOutput` (Q3) | UI layer |
| User presses Esc and explicitly declines | `ToolFailure { code: "user_declined" }` | UI layer |
| Turn cancels | `ToolFailure { code: "cancelled" }`, settled per ADR 0002 E5 | Race of tool with `cx.cancellation` |
| UI exits and Prompter side drops | `ToolFailure { code: "prompter_gone" }` | oneshot sender drop |
| Session closes while pending | Queue completes with abort result | Host queue |
| Process crashes while pending | No result; on reopen ADR 0002 `repair` gives a "Started only" call `outcome_unknown_after_crash` | Existing recovery rule |
| Late answer arrives | Silently ignored, never resubmitted | oneshot |
| Tool timeout | **None**. Waiting is user time, not a tool error | Decision |
| Whole-turn timeout (`--turn-timeout-secs`) | Turn cancels at expiry; this tool gets `cancelled` like all tools | Driver; it never pauses the clock for waiting on a user |

Multiple questions in one request navigate one by one in one Modal. Drafts stay across questions. A pre-submit review shows all. Required unanswered questions block submit. Multiple requests in one session and requests from different sessions show in queue order, one at a time.

### Q5 No TTY and non-interactive hosts

Unattended runs **do not register** `ask_user`. The test is "a human sits at the terminal", not approval policy: `radian-tui` always registers it, and `--approval never` only affects approval. **Only `radian-tui` registers it**; line-mode CLI never registers it. No "register but always fail" exists. The host startup line states `ask_user: off (no interactive user)`.

### Q6 Isolation from permissions

- `ask_user` writes no files and runs no commands. Permission rules fix it to **allow** with no approval prompt. It still passes the same hook path as all tools.
- An answer is **data**: it changes no permission state and equals no grant for later tool calls. Every tool call the model starts from an answer still passes rules and approval as usual.
- Approval popups and question panels are two Modal types: approval keys never rebind, form keys rebind. The two never merge into one generic confirm.

### Q7 UI behavior

- **While pending the transcript keeps one line**: `Ask N question(s) · answering below` (one level weak). It shows no stems or options. Questions appear only in the panel.
- **After answer (or decline, or cancel)**, the transcript shows the complete block: stems, selected items, `Other` and note text as-is, and the declined or cancelled state line. The same entry updates; no new entry appears.
- **The block is an open box**: `╭─ ask ───…`, each line `│ `, `╰──` (marker column, `frame` color). Content wraps at inner width (CJK by display width).
- **Panel title**: when the source is this same session, only `Ask`. When the source is another session, `Ask · <source>`.
- **Question block content**: each question separates with a labeled divider `[<id>] · <kind unless single> · options:<n>` (one level weak). Stems use emphasis color. Option descriptions start a new line, indent 3 spaces, one level weak, with `↳` prefix. After answer the same block marks selected items; `Other` and notes show answer text as-is. Decline and cancel show as one state line.
- **Option marks**: single, multi, and `Other` all use `○`/`◉`; marks never bold. The `ascii` level uses `( )`/`(*)` and `->`. Single versus multi differs only in the bottom hint.
- **Bottom Ask panel**: boxed, with the title above. First row is a tab row: one tab per question (text is `header`; last tab is `Submit`). The current tab shows reverse highlight. It never scrolls horizontally. Each tab truncates to 16 display widths. Answered questions show a done mark. Below the tab row are the current stem, divider, and option list: cursor row starts with `❯`. The `recommended` item adds ` (Recommended)`. Descriptions start a new line indented 3 spaces, one level weak. When `allow_other` exists, the last item is `Other (type your own)`; selecting it edits in place. A `text` question is directly an edit area. Bottom key hints change with context: single `⏎ select · n note`, multi `space toggle · ⏎ next`, `Submit` tab `⏎ submit`, plus `↑/↓ move · ⇥/←/→ question · esc cancel · alt-h hide`. Narrow terminals omit from right to left. The `Submit` tab shows one row per question. Unanswered required questions show red and block submit.
- **Focus fields and question switching**: with no field focused, `←` and `→` move to the previous or next question. When an `Other` row, a `text` question, or a note is focused, `←` and `→` move the text cursor. `text` questions, notes, and selected `Other` rows treat space as text. An unselected `Other` row still uses space to select it.
- **Keys**: `↑` and `↓` move. `⏎` **only confirms and advances to the next question, never selects**. After the last question it enters `Submit`. It advances even with zero selections (that question counts as unanswered). `Space` is the **only select key**: single selects, multi toggles, never advances. On `Other` `Space` selects and starts input at once (space in input is a space, `⏎` confirms and advances, `↑` and `↓` keep text on leave). `Tab` and `→`, `Shift-Tab` and `←` switch questions (cursor when a field is focused). `n` writes a note. `esc` declines (asks once to confirm). `alt-h` collapses the panel to one line; press again to restore.

### Q8 The ACP host maps permissions and questions to `session/request_permission`

- The ACP host (ADR 0049) registers `ask_user` and uses the session's pending queue for both
  approvals and questions.
- Every queue entry becomes one client request named `session/request_permission`. A
  permission entry offers the three outcomes ADR 0012 already has: `allow_once`
  (`AllowOnce`), `allow_always` (`AllowForSession`) and `reject_once`
  (`Deny { reason }`). A `cancelled` outcome denies with its reason and cancels the turn.
- An `ask_user` entry is asked one question at a time. The question's options become the
  permission options and the selected option becomes the answer; a synthetic "Decline"
  option is `PromptError::Declined`. `request_permission` carries one selection and no free
  text, so a `text` question, a free-text `Other` answer and a multi-selection answer are not
  representable on this surface; a text question fails the tool with `prompter_error` rather
  than fabricating an answer.
- The queue's abort outcomes are unchanged: a closing session denies an approval and gives a
  question `PromptError::Gone`.

## No Changes to `radian-core`

`ToolContext.cancellation`, `ToolOutput.structured`, `ToolConcurrency::Exclusive`, `ToolExecutionStarted` and `Settled`, and per-await `before_tool_call` all already exist. `ask_user` is a normal `ToolSession` member. Core never knows it waits on a human.

## Constraints and Costs

- No tool timeout exists: an unanswered question keeps that turn in the tool call until cancel or exit. "Waiting on a human" is not an error.
- Whole-turn timeout never pauses for waiting on a human.
- Without a TTY the model never sees this tool. The prompt tells it to decide alone in non-interactive mode.
- Answers live only in `ToolExecutionSettled.result` with no independent index. Listing "what users answered before" scans the journal.

## Verification

`radian-coding/tests/ask.rs`, `specs.rs`, and `journal.rs` cover schema, limits, `header`, `recommended`, `note`, and landing shape. `radian-host` `pending` tests cover multi-session order, session-close abort, and late-answer ignore. `radian-tui/tests/question.rs`, `render.rs`, and `pty.rs` cover form keys, panel and transcript records. `radian-permissions` covers fixed `allow` for `ask_user`. Fixture `ask-user.jsonl` passes the reference checker.
