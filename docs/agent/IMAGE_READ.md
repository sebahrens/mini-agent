# Image reads

With the default `multimodal` feature, `read` recognizes PNG, JPEG, GIF and WebP
from the bytes of the file admitted by the existing read permission path. It
returns one image block and a text summary containing dimensions and byte size.
Names and extensions do not decide the format. Images require a regular file,
at most 5 MiB, dimensions from 1 to 8000 pixels, and no text `offset`/`limit`.
An image read never establishes the raw full-read proof required for a text
overwrite. Text, notebook and CRC reading retain their existing behavior.

## Showing a scan to the agent

The default build accepts native image content; it is not a text-only runtime.
Use a vision-capable model and one of these entrypoints:

- In the TUI, `/add scan.png` (also JPEG, GIF or WebP), then enter a question or
  press Enter to send the image alone. File images and clipboard images use the
  same ordered, inline user-image format and survive canonical save/reload.
  They share the existing limits: 2 MiB per image, 4 MiB total and eight images.
  An oversized or invalid submission retains the staged image and text draft.
- Paste a clipboard image into an idle TUI prompt. Clipboard capture remains an
  explicit operator action; it does not run a model request until submission.
- From headless mode, ask the agent to `read` the workspace image, for example
  `mini-agent -p "Read scan.png and transcribe the page"`. This is a normal agent
  file read with the read permission and image limits described above, not an
  automatic attachment or permission grant.
- ACP clients can submit inline image content blocks. The inline bytes are
  authoritative; a supplied URI is display metadata and is never fetched.

A scanned PDF is a different input format. The default build does not enable
`pdf`, and `read` does not rasterize PDF pages or perform OCR. The optional `pdf`
feature enables a separate native document attachment surface, subject to the
selected provider's document support; it does not convert a scan into images.
Export the needed pages as PNG/JPEG to use the image paths above. No local
image renderer or OCR service is started implicitly.

Before a TUI prompt with pending attachments is sent, a local context preflight
estimates each attachment: an image by its header dimensions
(`ceil(w/28) * ceil(h/28)` tokens, at most 4,784), a PDF at 3,000 tokens per page,
another document by its text, and audio at a flat 2,048 tokens. A payload that
cannot fit even after compaction is rejected locally; otherwise the estimate feeds
the compaction decision. Provider-reported usage replaces it once a response
arrives.

The actual captured model/transport decides admission. An exact model declared
text-only in the embedded catalogue returns `model has no vision`. Unknown
custom model capabilities retain normal provider error handling; they are not
claimed to have been verified. Missing catalogue input metadata remains unknown,
including after catalogue refresh. Gemini refuses GIF. A build without `multimodal`
retains validated saved image data but refuses image reads and image-bearing
history before provider dispatch. No image URL or imported file reference is
opened by the canonical image decoder.

Anthropic sends native image tool results. OpenAI Responses sends a correlated
`function_call_output` with `input_text` and `input_image` output parts. Chat
Completions, OpenRouter, Ollama and Gemini preserve all original text tool
results and append literal user image content after the whole tool-call batch.
Each bridge image identifies its exact call; incomplete or ambiguous batches
are refused before HTTP. This avoids dropping images from text-only tool roles
and does not mutate the canonical Session history. Gemini uses this ordinary
image-input path because native multimodal function responses require newer
models.

### Image tool results by route

Every route reuses one of the converters above; none has its own image path.
The wire shapes are covered by `image_read64_actual_provider_bodies_keep_image_bytes_and_batch_correlation`
and `image_tool_results_on_copilot_azure_bedrock_vertex_routes` in
`src/tests/image_read_tests.rs`, which capture each request body on a loopback
server without network access.

| Route | Converter | Image tool result on the wire | `image_read_support` capability lookup |
|---|---|---|---|
| Anthropic | Anthropic Messages | native `tool_result` image block | `anthropic` catalogue |
| OpenAI Responses | Responses | native `input_image` in `function_call_output` | `openai` catalogue |
| OpenAI Chat Completions, OpenRouter, Ollama, Gemini | Chat / Gemini | bridged user image after the tool batch | provider catalogue (Ollama: none) |
| Copilot, model name containing `codex` | Responses | native `input_image`, with `copilot-vision-request: true` | none (`copilot` is not catalogued): unknown, admitted |
| Copilot, other models (Claude included) | Chat Completions | bridged user image, with `copilot-vision-request: true` | none: unknown, admitted |
| Azure OpenAI v1, default `responses` | Responses | native `input_image` | `openai` catalogue, keyed by the deployment name |
| Azure OpenAI v1, `api_style: completions` | Chat Completions | bridged user image | `openai` catalogue, keyed by the deployment name |
| Bedrock (`/anthropic` native Messages) | Anthropic Messages | native `tool_result` image block | `anthropic` catalogue; Bedrock IDs such as `us.anthropic.claude-…-v1:0` do not match, so unknown and admitted |
| Vertex (Claude `rawPredict`) | Anthropic Messages | native `tool_result` image block | `anthropic` catalogue; `claude-…@date` IDs do not match, so unknown and admitted |

Copilot requires the vision header for any image input. Before
mini-agent-yegs4 it was sent only for top-level user images, not for images
inside tool results (patched in `patched-deps/rig-core`, see its `PATCH.md`).
There is no Gemini-on-Vertex route and no Anthropic Messages mode on Copilot or
Azure. Unknown capability keeps the normal provider error path; it is the
right outcome for Bedrock and Vertex because every catalogued Claude model
accepts images. An Azure deployment that is literally named after a text-only
OpenAI model (`gpt-4`, `gpt-3.5-turbo`, `o3-mini`) is refused as text-only,
because the deployment name is the only model identity the route has.
Per-image provider limits are enforced by the provider, not here: `read`
admits up to 5 MiB of raw image bytes (about 6.7 MiB of base64), while
Anthropic documents a 5 MB per-image API limit, which Bedrock may set lower.
An image over the provider limit is rejected by that provider's request error.

The image summary and image bytes share one canonical tool-result row. JSONL
sessions retain literal, validated base64/MIME/detail data; ordinary history
conversion restores image blocks. Terminal, Markdown, HTML and compacted text
show the summary, not base64. Private workspace instruction notices can be
acknowledged only when their final effective image result remains intact;
rewriting or spilling cannot restore a hidden notice or image.

The format codec reads finite headers without decompressing pixels: PNG IHDR
checksum, bounded JPEG segments, GIF palette and WebP RIFF/chunk lengths.
Header validation is not full compressed-stream validation; the provider may
still reject an invalid image. No bitmap allocation or image dependency is used.
Six checked-in 3 by 2 pixel fixtures cover PNG, JPEG, GIF and three WebP variants.

Existing serialized limits remain independent: native capture has a 2 MiB frame
ceiling and a 4 MiB aggregate payload ceiling; full Session JSON is capped at
8 MiB. Base64 and surrounding history consume those budgets. The 5 MiB raw-file
limit does not promise native replay of a 5 MiB image. Oversized native capture
retains accepted accounting and refuses unsafe replay through the existing
native failure path. Canonical save validates image rows before artifact
creation and refuses an oversized snapshot.

Subagent children account for images separately from text. Provider-reported
usage, which already includes image tokens, remains authoritative for
`task_max_cost_units`. When a provider omits usage, and for the child context
guard, the local request estimate charges each literal image by Claude's patch
formula `ceil(w/28) * ceil(h/28)` from its header dimensions (capped at 4,784
tokens, the high-resolution tier) instead of its base64 length; an image whose
header cannot be read is charged at four pixels per decoded byte under the
same formula and cap, and an image reference without bytes at the cap. A child
also admits at most 4 MiB of encoded image data into its history in total, the
same aggregate as the native capture ceiling: a later image result past that
ceiling keeps its text summary and replaces its image with an omission notice.
The implementation is `src/extras/subagents/image_cost.rs`.

The opt-in inline image lane for `task` results (`[subagents]
task_inline_images`, default 0) reuses this envelope for the parent: at most 5
images and 1.5 MiB of base64 per call, so a task result stays inside the 2 MiB
frame and 4 MiB aggregate ceilings above, no image over 1,568 px on its longer
side (nothing is resampled), and each image charged to `task_max_cost_units`
by the same patch formula. See `docs/agent/SUBAGENTS.md#inline-image-lane`.

Provider references: [Anthropic tool results](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls),
[OpenAI function outputs](https://developers.openai.com/api/reference/resources/responses/methods/create),
[OpenAI image inputs](https://developers.openai.com/api/docs/guides/images-vision),
and [Gemini multimodal function responses](https://ai.google.dev/gemini-api/docs/function-calling).
