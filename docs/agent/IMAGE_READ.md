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

Provider references: [Anthropic tool results](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls),
[OpenAI function outputs](https://developers.openai.com/api/reference/resources/responses/methods/create),
[OpenAI image inputs](https://developers.openai.com/api/docs/guides/images-vision),
and [Gemini multimodal function responses](https://ai.google.dev/gemini-api/docs/function-calling).
