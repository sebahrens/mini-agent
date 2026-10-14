# Image reads — draft codec boundary

The image-read bead is in progress. The `read_image` module currently has only
test registration; `read` retains its existing text behavior. Model capability,
permission-preserving image admission, provider transport and instruction receipt
wiring remain required before this feature is delivered.

The module detects PNG, JPEG, GIF and WebP from magic bytes and inspects format
headers for dimensions without decompressing pixels. The compressed input ceiling
is 5 MiB and each dimension must be between 1 and 8000 pixels. It verifies PNG's
IHDR checksum, bounds JPEG segment traversal and GIF palette lookup, and checks
WebP's RIFF length and padded chunk boundaries. Header inspection does not prove
that the complete compressed stream is valid; provider decoding may still reject
it. There is no bitmap allocation or image dependency.

Fixtures are six fresh 3 by 2 pixel images generated with Pillow 12.2.0: PNG,
JPEG, GIF, lossy WebP, lossless WebP and WebP with alpha. No test depends on Pillow
at runtime. Tests cover their real dimensions, byte and dimension limits, malformed
lengths/checksum and every truncated prefix without claiming full stream validation.

Final admission must read from the handle already admitted by the existing read
permission path. It must also account for serialized result and replay limits:
the current native frame/capture ceiling is 2 MiB and its total payload ceiling is
4 MiB. A 5 MiB input ceiling does not establish that such an image fits those
envelopes. This draft does not raise any of those limits or grant overwrite rights.
