---
description: "HashEdit — zerostack's CRC-32 tagged, compare-and-swap file editing protocol."
---

# HashEdit

HashEdit is zerostack's line-oriented edit mode. It does not maintain an anchor
dictionary, reconcile moved lines, or keep per-session document state. A read
returns ordinary one-based line numbers plus CRC-32 tags calculated from the
current line text, and a file-level CRC-32 calculated from the complete
LF-normalized file. An edit succeeds only while those values still match.

Select the mode with either:

```text
/editsys hashedit
mini-agent --edit-system hashedit
```

The default `similarity` mode instead accepts SEARCH/REPLACE blocks.

## Read format

In HashEdit mode, `read` produces a header and tagged lines:

```text
File: src/example.rs (3 lines total, lines 1-3) [CRC: 15cf136a]

  1|9f5b4ca9 fn first() {}
  2|cbfe5127 fn second() {}
  3|00000000
```

The exact hexadecimal values depend on the bytes. Each line tag is the
eight-character lowercase CRC-32 of that line without its newline. The header
CRC covers the complete UTF-8 file after CRLF pairs have been normalized to LF.
Line numbers are one-based and are current positions, not stable identities.

Copy tagged lines directly from this output. Leading alignment spaces and the
displayed line content are accepted, but the parser uses only the `N|TAG`
prefix. The stored file is independently re-read and checked before mutation.

## Edit request

The HashEdit form of the `edit` tool accepts:

```json
{
  "path": "src/example.rs",
  "file_crc": "15cf136a",
  "edits": [
    {
      "line": "  2|cbfe5127 fn second() {}",
      "text": "fn replacement() {}"
    }
  ]
}
```

Required fields are:

| Field | Meaning |
| --- | --- |
| `path` | File to edit. Normal workspace and permission checks still apply. |
| `file_crc` | Eight-character CRC copied from the read header. |
| `edits` | One or more replacement operations applied atomically. |
| `edits[].text` | Replacement text. An empty string replaces the selected bytes with nothing. |

Each operation must contain exactly one selector:

- `line`: one copied `N|TAG content` line; or
- `lines`: copied newline-separated tagged lines describing one contiguous,
  strictly ascending range.

There are no `insert_before`, `insert_after`, or persistent-anchor operations.
Insertions are expressed by replacing a selected line or range with text that
includes both the retained content and the new content.

## Validation and atomicity

Before writing, the tool:

1. opens the target through the same stable workspace/path boundary used by
   other file tools and reads it as UTF-8;
2. normalizes CRLF pairs to LF for editing;
3. compares the complete normalized content with `file_crc`;
4. verifies every selected line is in range and its tag matches its current
   content;
5. requires a range selector to list every line in a strictly ascending,
   gap-free span;
6. rejects overlapping edit ranges; and
7. applies all ranges from the end of the file toward the beginning, then
   publishes the result through the atomic file-write path.

Any failed check rejects the entire call before publication. A stale file CRC
therefore forces a re-read even if the selected line itself did not change.
This deliberate whole-file compare-and-swap rule prevents an edit from silently
combining with an unobserved concurrent change elsewhere in the file.

## Newlines and file types

HashEdit accepts UTF-8 text only. Invalid UTF-8 is rejected rather than decoded
lossily. If the input contains CRLF, the final edited content is written with
CRLF; otherwise it is written with LF. As with the other edit mode, the file
tool is not a binary patch mechanism.

Because the implementation detects whether *any* CRLF pair exists and then
normalizes the whole file, a mixed-EOL file is not preserved byte-for-byte.
That limitation is tracked separately from this protocol reference.

## Failure recovery

Typical failures are actionable:

- `File CRC mismatch`: another writer changed the file, or the CRC came from a
  different read. Re-read and rebuild the request.
- `Tag mismatch`: the selected line no longer has the content represented by
  its copied tag. Re-read it.
- `Line ... is out of range`: line numbers shifted. Re-read the file.
- `tagged lines must be contiguous` or `ascending`: copy the complete range in
  file order, or use separate non-overlapping edits.
- `overlap`: combine the operations into one replacement or select disjoint
  ranges.

CRC-32 is an integrity tag for optimistic concurrency, not a cryptographic
authenticator. Security authorization continues to come from the file
capability and permission system.
