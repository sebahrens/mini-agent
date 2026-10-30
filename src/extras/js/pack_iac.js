// Lazily evaluated pack segment: infrastructure-as-code readers. See pack.rs and pack_boot.js.
// One catalogue line per helper (`//@ name | signature | purpose | usage`); keep lines free of `|`.
//@ dockerfile_parse | dockerfile_parse(text) | parse a Dockerfile into [{line, op, args}] | One entry per instruction; line is the 1-based first line, op is upper case, args the text after the op (continuations joined, comments and blank lines inside them dropped). JSON-form args of RUN CMD ENTRYPOINT COPY ADD SHELL VOLUME become a string array. FROM adds image, as (stage name or null), platform (or null), stage (0-based index); ENV ARG LABEL add pairs [[key, value]] (value null for a bare ARG name). RUN COPY ADD heredocs add heredocs [{name, body}]. Parser directives (# syntax=, # escape=) are entries with op DIRECTIVE and honour escape=`. Unknown instructions, unterminated heredocs and unterminated quotes in ENV ARG LABEL throw SyntaxError naming the line; text over 1 MiB throws RangeError. Example: dockerfile_parse('FROM node:20 AS build\nRUN a \\\n && b')[1].args === 'a  && b'
//@ hcl_blocks | hcl_blocks(text) | list the blocks of Terraform/HCL text as [{type, labels, attrs, blocks, line, end}] | Reads .tf text without evaluating anything. line and end are the 1-based first and last line (the closing brace); blocks holds nested blocks of the same shape. attrs maps each attribute name to {raw, kind, value?, line}: raw is the expression source text; kind is string, number, bool, null (value decoded), heredoc (value set unless it interpolates), template (a string with interpolation) or expr (anything else, no value). Handles comments (# // /* */), heredocs (<<EOT, <<-EOT), nested braces and interpolation in strings. Top-level attributes (.tfvars) come back as one block of type (top-level). Anything it cannot tokenise (unterminated string, heredoc or comment, unbalanced brackets, a single-quoted string, a duplicate attribute, nesting past 64 blocks) throws SyntaxError with the line; text over 1 MiB throws RangeError. Example: hcl_blocks('resource "aws_s3_bucket" "b" {\n  acl = "private"\n}')[0].attrs.acl.value === 'private'
//@ cidr | cidr(spec, other?) | describe an IPv4 or IPv6 CIDR (BigInt exact), optionally compared with a second one | spec is 'a.b.c.d/n', 'x::y/n' or a bare address (a single host). Returns {version, address, prefix, cidr (normalised network/prefix), network, broadcast (the last address, also for IPv6), size, netmask (IPv4 only)}; size is a number, or a decimal string above 2^53. With other, adds overlaps, contains (spec holds all of other) and within (other holds all of spec), all false across IP versions. Host bits are allowed (10.0.0.5/24 has network 10.0.0.0); IPv6 prints in compressed lower-case form. Strict input: IPv4 octets without leading zeros, no whitespace or zone ids; anything else throws SyntaxError. Example: cidr('10.0.0.0/22', '10.0.2.0/24').contains === true
//@ ip_in | ip_in(cidr, ip) | true if every address of ip (an address or a CIDR) lies inside cidr | Same parsing and strictness as cidr; an IPv4 address is never inside an IPv6 CIDR (so ::ffff:1.2.3.4 is not in 1.2.3.0/24). Example: ip_in('192.168.0.0/16', '192.168.7.9') === true
//@ scan_secrets | scan_secrets(text) | report likely secrets in text as [{rule, line, preview}], never the value itself | Report only: preview is the first min(4, length/3) characters of the match plus its length ('AKIA... (20 chars)'), so output is safe to print or log. Rules: aws_access_key_id, github_token, github_fine_grained_token, gitlab_token, slack_token, google_api_key, stripe_secret_key, npm_token, anthropic_api_key, openai_api_key, pypi_token, jwt, pem_private_key (header line), url_credentials (user:pass@host), generic_secret_assignment (password, secret, token, api_key, access_key, private_key = a value of 8 or more characters; placeholders such as ${X}, var.x, <x>, xxxx and http URLs are skipped). Sorted by position, at most 1000 findings (then a truncated entry); words over 16384 characters are skipped; text over 4 MiB throws RangeError. A heuristic aid, not proof of absence. Example: scan_secrets('k=AKIAABCDEFGHIJKLMNOP')[0].rule === 'aws_access_key_id'
(function (g) {
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const IAC_MAX_TEXT = 1024 * 1024;
  const clip = (s, n) => (s.length > n ? s.slice(0, n) + "..." : s);
  const checkText = (fn, text) => {
    if (typeof text !== "string") throw new TypeError(fn + ": text must be a string");
    if (text.length > IAC_MAX_TEXT) throw new RangeError(fn + ": text exceeds " + IAC_MAX_TEXT + " characters");
  };

  // ---- dockerfile_parse ----
  const DOCKER_OPS = new Set([
    "FROM", "RUN", "CMD", "LABEL", "MAINTAINER", "EXPOSE", "ENV", "ADD", "COPY", "ENTRYPOINT", "VOLUME",
    "USER", "WORKDIR", "ARG", "ONBUILD", "STOPSIGNAL", "HEALTHCHECK", "SHELL",
  ]);
  const DOCKER_JSON_OPS = new Set(["RUN", "CMD", "ENTRYPOINT", "COPY", "ADD", "SHELL", "VOLUME"]);
  const DOCKER_HEREDOC_OPS = new Set(["RUN", "COPY", "ADD"]);
  const DOCKER_MAX_HEREDOCS = 64;
  const isSpace = (c) => c === " " || c === "\t";
  const dockerErr = (line, msg) => new SyntaxError("dockerfile_parse: line " + line + ": " + msg);

  // Split `key=value key2="v 2"` words with shell-like quoting; returns [[key, value-or-null]].
  const dockerPairs = (args, esc, line, op) => {
    const words = [];
    let i = 0;
    while (i < args.length) {
      while (i < args.length && isSpace(args[i])) i++;
      if (i >= args.length) break;
      let text = "";
      let eq = -1;
      let quote = "";
      for (; i < args.length; i++) {
        const c = args[i];
        if (quote) {
          if (c === quote) quote = "";
          else if (c === esc && quote === '"' && i + 1 < args.length) text += args[++i];
          else text += c;
        } else if (c === '"' || c === "'") quote = c;
        else if (c === esc && i + 1 < args.length) text += args[++i];
        else if (isSpace(c)) break;
        else {
          if (c === "=" && eq < 0) eq = text.length;
          text += c;
        }
      }
      if (quote) throw dockerErr(line, "unterminated " + quote + " quote in " + op);
      words.push({ text, eq });
    }
    return words.map((w) => (w.eq < 0 ? [w.text, null] : [w.text.slice(0, w.eq), w.text.slice(w.eq + 1)]));
  };

  define("dockerfile_parse", function dockerfile_parse(text) {
    checkText("dockerfile_parse", text);
    const lines = (text.charCodeAt(0) === 0xfeff ? text.slice(1) : text).split("\n");
    for (let i = 0; i < lines.length; i++) {
      const l = lines[i];
      if (l.length && l.charCodeAt(l.length - 1) === 13) lines[i] = l.slice(0, -1);
    }
    let esc = "\\";
    let directives = true;
    let stage = -1;
    const out = [];
    let i = 0;
    while (i < lines.length) {
      const trimmed = lines[i].trim();
      if (directives) {
        const m = /^#\s*([A-Za-z][A-Za-z0-9_-]*)\s*=\s*(.*)$/.exec(trimmed);
        if (m && ["syntax", "escape", "check"].includes(m[1].toLowerCase())) {
          const key = m[1].toLowerCase();
          const value = m[2].trim();
          if (key === "escape") {
            if (value !== "\\" && value !== "`") throw dockerErr(i + 1, "escape directive must be \\ or `");
            esc = value;
          }
          out.push({ line: i + 1, op: "DIRECTIVE", args: key + "=" + value });
          i++;
          continue;
        }
      }
      directives = false;
      if (trimmed === "" || trimmed[0] === "#") {
        i++;
        continue;
      }
      const first = i + 1;
      let buf = "";
      for (;;) {
        const cur = lines[i].trimEnd();
        i++;
        if (cur.endsWith(esc)) {
          buf += cur.slice(0, -1);
          while (i < lines.length && (lines[i].trim() === "" || lines[i].trim()[0] === "#")) i++;
          if (i >= lines.length) break;
        } else {
          buf += lines[i - 1];
          break;
        }
      }
      const body = buf.trim();
      let cut = 0;
      while (cut < body.length && !/\s/.test(body[cut])) cut++;
      const op = body.slice(0, cut).toUpperCase();
      if (!DOCKER_OPS.has(op)) throw dockerErr(first, "unknown instruction '" + clip(body.slice(0, cut), 40) + "'");
      const rest = body.slice(cut).trim();
      const entry = { line: first, op, args: rest };
      if (DOCKER_JSON_OPS.has(op) && rest[0] === "[") {
        try {
          const parsed = JSON.parse(rest);
          if (Array.isArray(parsed) && parsed.every((x) => typeof x === "string")) entry.args = parsed;
        } catch (e) {
          // not JSON form: stays shell form text
        }
      }
      if (op === "FROM") {
        const words = rest.split(/\s+/).filter((w) => w !== "");
        let platform = null;
        while (words.length && words[0].startsWith("--")) {
          const flag = words.shift();
          if (flag.startsWith("--platform=")) platform = flag.slice(11);
        }
        const image = words.shift();
        if (image === undefined) throw dockerErr(first, "FROM needs an image");
        let as = null;
        if (words.length) {
          if (words.length !== 2 || words[0].toLowerCase() !== "as") {
            throw dockerErr(first, "FROM expects 'image [AS name]'");
          }
          as = words[1];
        }
        stage++;
        entry.image = image;
        entry.as = as;
        entry.platform = platform;
        entry.stage = stage;
      } else if (op === "ENV" || op === "LABEL" || op === "ARG") {
        const firstWord = rest.split(/\s/, 1)[0];
        if (op !== "ARG" && rest !== "" && !firstWord.includes("=")) {
          entry.pairs = [[firstWord, rest.slice(firstWord.length).trim()]];
        } else {
          entry.pairs = dockerPairs(rest, esc, first, op);
        }
      }
      if (DOCKER_HEREDOC_OPS.has(op) && typeof entry.args === "string" && entry.args.includes("<<")) {
        const heredocs = [];
        const markers = /<<(-?)(?:"([^"\s]+)"|'([^'\s]+)'|([A-Za-z_][A-Za-z0-9_]*))/g;
        for (const m of entry.args.matchAll(markers)) {
          if (m.index > 0 && entry.args[m.index - 1] === "<") continue;
          if (heredocs.length >= DOCKER_MAX_HEREDOCS) throw dockerErr(first, "more than " + DOCKER_MAX_HEREDOCS + " heredocs");
          const name = m[2] || m[3] || m[4];
          const strip = m[1] === "-";
          const bodyLines = [];
          let closed = false;
          while (i < lines.length) {
            const raw = lines[i++];
            const cmp = strip ? raw.replace(/^\t+/, "") : raw;
            if (cmp === name) {
              closed = true;
              break;
            }
            bodyLines.push(cmp);
          }
          if (!closed) throw dockerErr(first, "unterminated heredoc '" + clip(name, 40) + "'");
          heredocs.push({ name, body: bodyLines.length ? bodyLines.join("\n") + "\n" : "" });
        }
        if (heredocs.length) entry.heredocs = heredocs;
      }
      out.push(entry);
    }
    return out;
  });

  // ---- hcl_blocks ----
  const HCL_MAX_NEST = 256;
  const HCL_MAX_BLOCK_DEPTH = 64;
  const HCL_PLAIN = /[^"\\$%\n]+/y;
  const HCL_NUMBER = /^-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?$/;
  const isIdStart = (c) => (c >= "a" && c <= "z") || (c >= "A" && c <= "Z") || c === "_";
  const isIdChar = (c) => isIdStart(c) || (c >= "0" && c <= "9") || c === "-";

  // Decode the escapes of a complete double-quoted token; null when it interpolates or is not
  // exactly one string literal.
  const hclDecodeString = (raw) => {
    if (raw.length < 2 || raw[0] !== '"') return null;
    let out = "";
    let j = 1;
    while (j < raw.length) {
      const c = raw[j];
      if (c === '"') return j === raw.length - 1 ? out : null;
      if (c === "\\") {
        const e = raw[j + 1];
        const simple = { n: "\n", r: "\r", t: "\t", '"': '"', "\\": "\\" }[e];
        if (simple !== undefined) {
          out += simple;
          j += 2;
        } else if (e === "u" || e === "U") {
          const len = e === "u" ? 4 : 8;
          const hex = raw.slice(j + 2, j + 2 + len);
          if (hex.length !== len || !/^[0-9a-fA-F]+$/.test(hex)) return null;
          const cp = parseInt(hex, 16);
          if (cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) return null;
          out += String.fromCodePoint(cp);
          j += 2 + len;
        } else return null;
      } else if ((c === "$" || c === "%") && raw[j + 1] === c && raw[j + 2] === "{") {
        out += c + "{";
        j += 3;
      } else if ((c === "$" || c === "%") && raw[j + 1] === "{") return null;
      else {
        out += c;
        j++;
      }
    }
    return null;
  };

  const hclLiteral = (raw) => {
    if (raw === "true" || raw === "false") return { kind: "bool", value: raw === "true" };
    if (raw === "null") return { kind: "null", value: null };
    if (HCL_NUMBER.test(raw)) {
      const v = Number(raw);
      return Number.isFinite(v) ? { kind: "number", value: v } : { kind: "expr" };
    }
    if (raw[0] === '"') {
      const v = hclDecodeString(raw);
      return v === null ? { kind: "template" } : { kind: "string", value: v };
    }
    if (raw.startsWith("<<")) {
      const m = /^<<(-?)([A-Za-z_][A-Za-z0-9_-]*)[ \t\r]*\n/.exec(raw);
      if (!m) return { kind: "expr" };
      let lines = raw.slice(m[0].length).split("\n");
      lines = lines.slice(0, -1).map((l) => (l.endsWith("\r") ? l.slice(0, -1) : l));
      if (m[1] === "-") {
        let indent = Infinity;
        for (const l of lines) {
          if (l.trim() === "") continue;
          indent = Math.min(indent, l.length - l.trimStart().length);
        }
        if (indent !== Infinity && indent > 0) lines = lines.map((l) => l.slice(Math.min(indent, l.length - l.trimStart().length)));
      }
      const body = lines.length ? lines.join("\n") + "\n" : "";
      const plain = body.split("$${").join("").split("%%{").join("");
      if (plain.includes("${") || plain.includes("%{")) return { kind: "heredoc" };
      return { kind: "heredoc", value: body.split("$${").join("${").split("%%{").join("%{") };
    }
    return { kind: "expr" };
  };

  define("hcl_blocks", function hcl_blocks(text) {
    checkText("hcl_blocks", text);
    const src = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
    const n = src.length;
    const starts = [0];
    for (let p = src.indexOf("\n"); p >= 0; p = src.indexOf("\n", p + 1)) starts.push(p + 1);
    const lineAt = (pos) => {
      let lo = 0;
      let hi = starts.length - 1;
      while (lo < hi) {
        const mid = (lo + hi + 1) >> 1;
        if (starts[mid] <= pos) lo = mid;
        else hi = mid - 1;
      }
      return lo + 1;
    };
    const fail = (pos, msg) => new SyntaxError("hcl_blocks: line " + lineAt(pos) + ": " + msg);
    let i = 0;

    // Skip spaces and comments; newlines too when `newlines` is set.
    const skipTrivia = (newlines) => {
      for (;;) {
        const c = src[i];
        if (c === " " || c === "\t" || c === "\r") i++;
        else if (c === "\n") {
          if (!newlines) return;
          i++;
        } else if (c === "#" || (c === "/" && src[i + 1] === "/")) {
          const k = src.indexOf("\n", i);
          i = k < 0 ? n : k;
        } else if (c === "/" && src[i + 1] === "*") {
          const e = src.indexOf("*/", i + 2);
          if (e < 0) throw fail(i, "unterminated /* comment");
          i = e + 2;
        } else return;
      }
    };

    const scanInterp = (p, strStart, depth) => {
      let j = p;
      let braces = 1;
      for (;;) {
        if (j >= n) throw fail(strStart, "unterminated template interpolation");
        const c = src[j];
        if (c === '"') j = scanString(j, depth);
        else if (c === "{") {
          if (++braces > HCL_MAX_NEST) throw fail(strStart, "interpolation nested too deeply");
          j++;
        } else if (c === "}") {
          j++;
          if (--braces === 0) return j;
        } else j++;
      }
    };
    // p is at the opening quote; returns the index after the closing quote.
    const scanString = (p, depth) => {
      if (depth > HCL_MAX_NEST) throw fail(p, "strings nested too deeply");
      let j = p + 1;
      for (;;) {
        if (j >= n) throw fail(p, "unterminated string");
        HCL_PLAIN.lastIndex = j;
        if (HCL_PLAIN.test(src)) j = HCL_PLAIN.lastIndex;
        if (j >= n) throw fail(p, "unterminated string");
        const c = src[j];
        if (c === '"') return j + 1;
        if (c === "\n") throw fail(p, "unterminated string");
        if (c === "\\") {
          if (src[j + 1] === "\n") throw fail(p, "unterminated string");
          j += 2;
        } else if (c === "$" || c === "%") {
          if (src[j + 1] === c && src[j + 2] === "{") j += 3;
          else if (src[j + 1] === "{") j = scanInterp(j + 2, p, depth + 1);
          else j++;
        } else j++;
      }
    };
    // p is at `<<`; returns the index at the end of the closing delimiter line.
    const scanHeredoc = (p) => {
      let j = p + 2;
      if (src[j] === "-") j++;
      const nameStart = j;
      if (!isIdStart(src[j] || "")) throw fail(p, "unexpected '<<' (a heredoc is <<NAME or <<-NAME)");
      while (j < n && isIdChar(src[j])) j++;
      const name = src.slice(nameStart, j);
      while (src[j] === " " || src[j] === "\t" || src[j] === "\r") j++;
      if (j < n && src[j] !== "\n") throw fail(p, "a heredoc opener must end its line");
      for (;;) {
        if (j >= n) throw fail(p, "unterminated heredoc '" + clip(name, 40) + "'");
        const lineStart = j + 1;
        let lineEnd = src.indexOf("\n", lineStart);
        if (lineEnd < 0) lineEnd = n;
        let k = lineStart;
        while (k < lineEnd && (src[k] === " " || src[k] === "\t")) k++;
        if (src.startsWith(name, k)) {
          k += name.length;
          while (k < lineEnd && (src[k] === " " || src[k] === "\t" || src[k] === "\r")) k++;
          if (k === lineEnd) return lineEnd;
        }
        if (lineEnd >= n) throw fail(p, "unterminated heredoc '" + clip(name, 40) + "'");
        j = lineEnd;
      }
    };
    const CLOSER = { "(": ")", "[": "]", "{": "}" };
    // Scan one attribute expression starting at `start`; stops at a newline or a closing brace
    // of the enclosing block when no bracket is open. Returns [end, endOfLastToken].
    const scanExpr = (start) => {
      let j = start;
      let last = start;
      const stack = [];
      for (;;) {
        if (j >= n) {
          if (stack.length) throw fail(start, "unclosed '" + stack[stack.length - 1] + "' in expression");
          return [j, last];
        }
        const c = src[j];
        if (c === "\n") {
          if (!stack.length) return [j, last];
          j++;
        } else if (c === " " || c === "\t" || c === "\r") j++;
        else if (c === "#" || (c === "/" && src[j + 1] === "/")) {
          const k = src.indexOf("\n", j);
          j = k < 0 ? n : k;
        } else if (c === "/" && src[j + 1] === "*") {
          const e = src.indexOf("*/", j + 2);
          if (e < 0) throw fail(j, "unterminated /* comment");
          j = e + 2;
        } else if (c === '"') {
          j = scanString(j, 0);
          last = j;
        } else if (c === "<" && src[j + 1] === "<") {
          j = scanHeredoc(j);
          last = j;
          if (!stack.length) return [j, last];
        } else if (c === "(" || c === "[" || c === "{") {
          if (stack.length >= HCL_MAX_NEST) throw fail(j, "expression nested too deeply");
          stack.push(CLOSER[c]);
          j++;
          last = j;
        } else if (c === ")" || c === "]" || c === "}") {
          if (!stack.length) {
            if (c === "}") return [j, last];
            throw fail(j, "unbalanced '" + c + "'");
          }
          if (stack.pop() !== c) throw fail(j, "mismatched '" + c + "'");
          j++;
          last = j;
        } else if (c === "'") throw fail(j, "unexpected single quote (HCL strings use double quotes)");
        else {
          j++;
          last = j;
        }
      }
    };

    const setKey = (obj, key, value) =>
      Object.defineProperty(obj, key, { value, writable: true, enumerable: true, configurable: true });
    const readAttr = (attrs, name, nameStart) => {
      i++; // '='
      while (src[i] === " " || src[i] === "\t") i++;
      const [end, last] = scanExpr(i);
      const raw = src.slice(i, last).trim();
      if (raw === "") throw fail(nameStart, "missing expression for '" + clip(name, 40) + "'");
      if (Object.prototype.hasOwnProperty.call(attrs, name)) {
        throw fail(nameStart, "duplicate attribute '" + clip(name, 40) + "'");
      }
      const lit = hclLiteral(raw);
      const attr = { raw, kind: lit.kind };
      if ("value" in lit) attr.value = lit.value;
      attr.line = lineAt(nameStart);
      setKey(attrs, name, attr);
      i = end;
    };

    const parseBody = (nest, top) => {
      const attrs = {};
      const blocks = [];
      let firstAttr = 0;
      let lastAttr = 0;
      for (;;) {
        skipTrivia(true);
        if (i >= n) {
          if (!top) throw fail(n > 0 ? n - 1 : 0, "unclosed block (missing '}')");
          return { attrs, blocks, firstAttr, lastAttr };
        }
        if (src[i] === "}") {
          if (top) throw fail(i, "unexpected '}'");
          return { attrs, blocks };
        }
        if (!isIdStart(src[i])) throw fail(i, "unexpected character '" + clip(src[i], 1) + "'");
        const nameStart = i;
        while (i < n && isIdChar(src[i])) i++;
        const name = src.slice(nameStart, i);
        skipTrivia(false);
        if (src[i] === "=" && src[i + 1] !== "=") {
          if (!firstAttr) firstAttr = lineAt(nameStart);
          readAttr(attrs, name, nameStart);
          lastAttr = lineAt(i > 0 ? i - 1 : 0);
          continue;
        }
        const labels = [];
        while (src[i] !== "{") {
          if (src[i] === '"') {
            const e = scanString(i, 0);
            const label = hclDecodeString(src.slice(i, e));
            if (label === null) throw fail(i, "a block label must be a plain string");
            labels.push(label);
            i = e;
          } else if (i < n && isIdStart(src[i])) {
            const s = i;
            while (i < n && isIdChar(src[i])) i++;
            labels.push(src.slice(s, i));
          } else throw fail(i, "expected '=' or a block body after '" + clip(name, 40) + "'");
          if (labels.length > 16) throw fail(i, "more than 16 block labels");
          skipTrivia(false);
        }
        i++; // '{'
        if (nest + 1 > HCL_MAX_BLOCK_DEPTH) throw fail(i, "blocks nested deeper than " + HCL_MAX_BLOCK_DEPTH);
        const body = parseBody(nest + 1, false);
        const end = lineAt(i);
        i++; // '}'
        skipTrivia(false);
        if (i < n && src[i] !== "\n" && src[i] !== "}") throw fail(i, "expected a newline after the closing brace");
        blocks.push({ type: name, labels, attrs: body.attrs, blocks: body.blocks, line: lineAt(nameStart), end });
      }
    };

    const top = parseBody(0, true);
    if (Object.keys(top.attrs).length) {
      top.blocks.unshift({ type: "(top-level)", labels: [], attrs: top.attrs, blocks: [], line: top.firstAttr, end: top.lastAttr });
    }
    return top.blocks;
  });

  // ---- cidr / ip_in (IPv4 and IPv6 on BigInt) ----
  const IP_MAX_SPEC = 100;
  const hex16 = (v) => v.toString(16);
  const parseV4 = (s) => {
    const parts = s.split(".");
    if (parts.length !== 4) return null;
    let v = 0n;
    for (const p of parts) {
      if (!/^(?:0|[1-9][0-9]{0,2})$/.test(p) || Number(p) > 255) return null;
      v = (v << 8n) | BigInt(p);
    }
    return v;
  };
  const parseV6 = (s) => {
    if (!/^[0-9A-Fa-f:.]+$/.test(s)) return null;
    let str = s;
    if (s.includes(".")) {
      const cut = s.lastIndexOf(":");
      const v4 = parseV4(s.slice(cut + 1));
      if (cut < 0 || v4 === null) return null;
      str = s.slice(0, cut + 1) + hex16(v4 >> 16n) + ":" + hex16(v4 & 0xffffn);
    }
    if (str.includes(":::")) return null;
    const dbl = str.indexOf("::");
    if (dbl !== str.lastIndexOf("::")) return null;
    let groups;
    if (dbl >= 0) {
      const head = str.slice(0, dbl) === "" ? [] : str.slice(0, dbl).split(":");
      const tail = str.slice(dbl + 2) === "" ? [] : str.slice(dbl + 2).split(":");
      if (head.length + tail.length > 7) return null;
      groups = head.concat(new Array(8 - head.length - tail.length).fill("0"), tail);
    } else groups = str.split(":");
    if (groups.length !== 8) return null;
    let v = 0n;
    for (const gr of groups) {
      if (!/^[0-9A-Fa-f]{1,4}$/.test(gr)) return null;
      v = (v << 16n) | BigInt("0x" + gr);
    }
    return v;
  };
  const fmtV4 = (v) => [24n, 16n, 8n, 0n].map((sh) => String((v >> sh) & 255n)).join(".");
  const fmtV6 = (v) => {
    const groups = [];
    for (let k = 7n; k >= 0n; k--) groups.push(Number((v >> (k * 16n)) & 0xffffn));
    let bestAt = -1;
    let bestLen = 1;
    for (let k = 0; k < 8; ) {
      if (groups[k] !== 0) {
        k++;
        continue;
      }
      let e = k;
      while (e < 8 && groups[e] === 0) e++;
      if (e - k > bestLen) {
        bestAt = k;
        bestLen = e - k;
      }
      k = e;
    }
    const hexes = groups.map((x) => x.toString(16));
    if (bestAt < 0) return hexes.join(":");
    return hexes.slice(0, bestAt).join(":") + "::" + hexes.slice(bestAt + bestLen).join(":");
  };
  // Parse "addr" or "addr/prefix"; a bare address is a single host.
  const ipRange = (fn, spec) => {
    if (typeof spec !== "string") throw new TypeError(fn + ": expected an address or CIDR string");
    if (spec.length > IP_MAX_SPEC) throw new RangeError(fn + ": address longer than " + IP_MAX_SPEC + " characters");
    const bad = () => new SyntaxError(fn + ": invalid IPv4/IPv6 address or CIDR '" + clip(spec, 60) + "'");
    const slash = spec.indexOf("/");
    const addrText = slash < 0 ? spec : spec.slice(0, slash);
    const version = addrText.includes(":") ? 6 : 4;
    const bits = version === 6 ? 128 : 32;
    const addr = version === 6 ? parseV6(addrText) : parseV4(addrText);
    if (addr === null) throw bad();
    let prefix = bits;
    if (slash >= 0) {
      const p = spec.slice(slash + 1);
      if (!/^(?:0|[1-9][0-9]{0,2})$/.test(p) || Number(p) > bits) throw bad();
      prefix = Number(p);
    }
    const full = (1n << BigInt(bits)) - 1n;
    const hostMask = (1n << BigInt(bits - prefix)) - 1n;
    const network = addr & (full ^ hostMask);
    return { version, bits, addr, prefix, network, last: network | hostMask, hostMask };
  };
  const fmtIp = (r, v) => (r.version === 6 ? fmtV6(v) : fmtV4(v));
  const containsRange = (outer, inner) =>
    outer.version === inner.version && outer.network <= inner.network && inner.last <= outer.last;

  define("cidr", function cidr(spec, other) {
    const r = ipRange("cidr", spec);
    const size = r.hostMask + 1n;
    const out = {
      version: r.version,
      address: fmtIp(r, r.addr),
      prefix: r.prefix,
      cidr: fmtIp(r, r.network) + "/" + r.prefix,
      network: fmtIp(r, r.network),
      broadcast: fmtIp(r, r.last),
      size: size <= 9007199254740991n ? Number(size) : size.toString(),
    };
    if (r.version === 4) out.netmask = fmtV4(((1n << 32n) - 1n) ^ r.hostMask);
    if (other !== undefined) {
      const o = ipRange("cidr", other);
      out.overlaps = r.version === o.version && r.network <= o.last && o.network <= r.last;
      out.contains = containsRange(r, o);
      out.within = containsRange(o, r);
    }
    return out;
  });
  define("ip_in", function ip_in(cidrSpec, ipOrCidr) {
    return containsRange(ipRange("ip_in", cidrSpec), ipRange("ip_in", ipOrCidr));
  });

  // ---- scan_secrets (report only; never returns a secret value) ----
  const SECRET_MAX_TEXT = 4 * 1024 * 1024;
  const SECRET_MAX_FINDINGS = 1000;
  const SECRET_COLLECT_LIMIT = 5000;
  const SECRET_MAX_WORD = 16384;
  // Token rules run on whole dot-free words ([A-Za-z0-9_-]+); each regex is anchored, so a word
  // costs time linear in its length.
  const SECRET_WORD_RULES = [
    ["aws_access_key_id", /^(?:AKIA|ASIA|AGPA|AIDA|AROA|ANPA|ANVA|AIPA)[A-Z0-9]{16}$/],
    ["github_token", /^gh[pousr]_[A-Za-z0-9]{36,255}$/],
    ["github_fine_grained_token", /^github_pat_[A-Za-z0-9_]{22,255}$/],
    ["gitlab_token", /^glpat-[A-Za-z0-9_-]{20,}$/],
    ["slack_token", /^(?:xox[abposr]-[A-Za-z0-9-]{10,}|xapp-[A-Za-z0-9-]{10,})$/],
    ["google_api_key", /^AIza[0-9A-Za-z_-]{35}$/],
    ["stripe_secret_key", /^[sr]k_live_[0-9A-Za-z]{16,}$/],
    ["npm_token", /^npm_[A-Za-z0-9]{36}$/],
    ["anthropic_api_key", /^sk-ant-[A-Za-z0-9_-]{20,}$/],
    ["openai_api_key", /^sk-(?:proj-)?[A-Za-z0-9_-]{32,}$/],
    ["pypi_token", /^pypi-[A-Za-z0-9_-]{50,}$/],
  ];
  const SECRET_ASSIGN =
    /(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|auth[_-]?key)[A-Za-z0-9_-]{0,32}["']?[ \t]*[:=][ \t]*["']?([^\s"'#,;]{8,})/gi;
  const SECRET_PLACEHOLDER =
    /^(?:[$({<%]|(?:var|local|data|module|env|secrets?|process|os|vault|config|settings)\.|https?:\/\/|(?:x+|\*+|\.+|-+|_+|your[_-].*|example.*|placeholder.*|redacted.*|changeme.*|todo.*)$)/i;
  const secretPreview = (value) => {
    const shown = Math.min(4, Math.floor(value.length / 3));
    return value.slice(0, shown) + "... (" + value.length + " chars)";
  };

  define("scan_secrets", function scan_secrets(text) {
    if (typeof text !== "string") throw new TypeError("scan_secrets: text must be a string");
    if (text.length > SECRET_MAX_TEXT) throw new RangeError("scan_secrets: text exceeds " + SECRET_MAX_TEXT + " characters");
    const found = []; // {pos, end, rule, preview}
    const add = (pos, value, rule) => {
      if (found.length < SECRET_COLLECT_LIMIT) found.push({ pos, end: pos + value.length, rule, preview: secretPreview(value) });
    };
    // Token rules over words; JWTs over dot-separated runs.
    const words = /[A-Za-z0-9_.-]{20,}/g;
    for (let m = words.exec(text); m !== null; m = words.exec(text)) {
      const token = m[0];
      if (token.length > SECRET_MAX_WORD) continue;
      let offset = 0;
      const parts = token.split(".");
      for (const part of parts) {
        if (part.length >= 20) {
          for (const [rule, re] of SECRET_WORD_RULES) {
            if (re.test(part)) {
              add(m.index + offset, part, rule);
              break;
            }
          }
        }
        offset += part.length + 1;
      }
      if (parts.length >= 3) {
        offset = 0;
        for (let k = 0; k + 2 < parts.length; k++) {
          if (parts[k].length >= 10 && parts[k].startsWith("eyJ") && parts[k + 1].length >= 10 && parts[k + 1].startsWith("eyJ") && parts[k + 2].length >= 10) {
            add(m.index + offset, parts[k] + "." + parts[k + 1] + "." + parts[k + 2], "jwt");
            break;
          }
          offset += parts[k].length + 1;
        }
      }
    }
    // PEM private key headers.
    for (let p = text.indexOf("-----BEGIN "); p >= 0; p = text.indexOf("-----BEGIN ", p + 11)) {
      const e = text.indexOf("-----", p + 11);
      if (e < 0) break;
      const label = text.slice(p + 11, e);
      if (label.length < 60 && !label.includes("\n") && (label.endsWith("PRIVATE KEY") || label.endsWith("PRIVATE KEY BLOCK"))) {
        add(p, text.slice(p, e + 5), "pem_private_key");
      }
    }
    // user:password@ in URLs.
    for (let p = text.indexOf("://"); p >= 0; p = text.indexOf("://", p + 3)) {
      let j = p + 3;
      const limit = Math.min(text.length, p + 3 + 300);
      while (j < limit && !" \t\r\n/@:\"'".includes(text[j])) j++;
      if (text[j] !== ":" || j === p + 3) continue;
      const passStart = j + 1;
      j = passStart;
      while (j < limit && !" \t\r\n/@\"'".includes(text[j])) j++;
      if (text[j] !== "@" || j - passStart < 3) continue;
      const pass = text.slice(passStart, j);
      if (!SECRET_PLACEHOLDER.test(pass)) add(passStart, pass, "url_credentials");
    }
    // Generic secret = value assignments; skipped when the value overlaps a specific finding.
    found.sort((a, b) => a.pos - b.pos);
    const overlapsFound = (pos, end) => {
      let lo = 0;
      let hi = found.length;
      while (lo < hi) {
        const mid = (lo + hi) >> 1;
        if (found[mid].pos < pos) lo = mid + 1;
        else hi = mid;
      }
      return (lo > 0 && found[lo - 1].end > pos) || (lo < found.length && found[lo].pos < end);
    };
    const added = [];
    SECRET_ASSIGN.lastIndex = 0;
    for (let m = SECRET_ASSIGN.exec(text); m !== null; m = SECRET_ASSIGN.exec(text)) {
      const value = m[1];
      const pos = m.index + m[0].length - value.length;
      if (SECRET_PLACEHOLDER.test(value) || value.includes("${") || /[([]/.test(value)) continue;
      if (overlapsFound(pos, pos + value.length)) continue;
      added.push([pos, value]);
    }
    for (const [pos, value] of added) add(pos, value, "generic_secret_assignment");
    found.sort((a, b) => a.pos - b.pos || (a.rule < b.rule ? -1 : 1));
    const out = [];
    let line = 1;
    let nextNl = text.indexOf("\n");
    for (const f of found) {
      while (nextNl >= 0 && nextNl < f.pos) {
        line++;
        nextNl = text.indexOf("\n", nextNl + 1);
      }
      if (out.length === SECRET_MAX_FINDINGS) {
        out.push({ rule: "truncated", line, preview: "more findings omitted after " + SECRET_MAX_FINDINGS });
        break;
      }
      out.push({ rule: f.rule, line, preview: f.preview });
    }
    return out;
  });
})(globalThis);
