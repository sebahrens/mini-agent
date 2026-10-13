# Clarifying questions

`ask_user_question` asks the user for missing task information. It is available
only to the ordinary main agent when tools are enabled and the tool allowlist
includes it. Children, side questions, advisors and format repairs cannot use
this clarification channel. Ask at most once in a user turn, batching missing
choices in that call; transport retries, context relief, generated phases and
automatic goal rounds preserve that slot. New submitted user text or approval to
execute a reviewed plan starts a fresh slot.

Use either `{question, options?, allow_free_text?}` or
`{questions: [{question, options?, allow_free_text?}, ...]}`. Each option has
`label` and an optional `description`. A batch contains one to four questions,
each with at most eight options. Free text defaults to allowed. Empty questions,
duplicate option labels, oversized strings and malformed batches are rejected
before requesting an answer. Runtime limits are UTF-8 bytes: question 2,048,
label 128, description 512, batch 16,384 and each free-text answer 4,096.

In the TUI, the prompt displays its exact question ID and numbered options above
the input. Choose a displayed number, or press `t` to type another answer when
free text is offered; press Enter to submit text. Each question in the batch is
shown in sequence. The ordinary input draft stays separate; held choice-key repeats do not answer
the next question. Escape or Ctrl+C
cancels the exact pending reply and stops its owning run. A cancelled or stale
reply cannot answer another turn. Approval keys have no permission meaning in
this prompt.

ACP uses the existing option-selection request to present exact advertised IDs.
Selections are task answers; they never enter the permission-decision or session
grant path. The option-only protocol provides no free-text answer payload.
Selecting “Answer in a new user message” parks the turn with its bounded question
and any preceding answers. The next ordinary user message begins after that old
turn and reply retire; it is not an invented free-text response extension. ACP
retains pending data with its existing in-memory session history. ACP clients may
automatically select options according to their settings; the
agent decodes only an exact advertised ID as task data. See
[the ACP v1 permission request contract](https://agentclientprotocol.com/protocol/v1/tool-calls#requesting-permission).

Headless mode has no interactive reply channel. `headless_questions = "error"`
is the default: the tool returns an error telling the model to state a low-risk
assumption and continue within existing authority, or explain an unsafe blocker.
With `headless_questions = "fail"`, the turn stops as `awaiting_user` and exits
22. Active goal execution parks as awaiting user input. Previously observed
usage, cost, history and completed effects remain recorded; this state takes
precedence over verification, transport retries and queued starts. No following
model request or tool effect is admitted by the parked main turn.

Pending question records contain a UUID, bounded questions/options and validated
partial answers. Restoring a record restores data, never a live reply sender,
permission grant or callback. An ordinary new user prompt supplies the missing
information and gets a fresh invocation identity.

`src/question.rs` owns validation and per-turn identity;
`src/agent/tools/ask_user.rs` owns the tool;
`src/ui/question_handler.rs` owns the TUI answer overlay. The typed
`UserRequest::Question` transport is separate from `UserRequest::Permission`.
Existing tool permissions and financial admission remain authoritative.
