// Lazily evaluated pack segment: yaml_parse, yaml_stringify; see pack.rs and pack_boot.js.
// Every helper has one catalogue line below (`//@ name | signature | purpose | usage`) that
// `help()` serves; the segment body must define each listed name on globalThis. Keep lines free of `|`.
//@ yaml_parse | yaml_parse(text, {all?}) | parse a YAML 1.2 document (Kubernetes, compose, GitHub Actions, Helm values) | Block and flow maps and sequences, plain and quoted scalars, literal and folded block scalars (indent and chomp indicators), comments, anchors with aliases, the << merge key, and --- multi-document text. Core typing: true/false, null/~, ints, floats, .inf, .nan; yes/no/on/off stay strings; integers beyond 2^53 become BigInt; map keys are always strings. Several documents throw unless {all: true}, which returns an array (a trailing empty document is dropped). Anything outside the subset throws SyntaxError with the line number rather than guessing: tags (!), complex keys (?), tab indentation, directives, duplicate keys, an anchor on a key, an alias as a key, a single-pair flow entry. Caps: 4 MiB, nesting 64, 200000 expanded nodes counting alias expansion (RangeError). Example: yaml_parse('a: [1, two]\nb: &x {c: 1}\nd: *x\n').d.c === 1
//@ yaml_stringify | yaml_stringify(value) | block-style YAML text for a plain JS value | Maps, arrays, strings, finite and non-finite numbers, BigInt, booleans, null and Date (as a string). Keys keep insertion order, undefined entries are skipped, indent is 2, strings that would read back as another type or contain special characters are double-quoted and multi-line text uses a literal block. yaml_parse(yaml_stringify(v)) gives back v. Functions, symbols and class instances throw TypeError; nesting over 64 (also a cycle) throws RangeError. Example: yaml_stringify({replicas: 3, ports: [80, 443]}) gives 'replicas: 3\nports:\n  - 80\n  - 443\n'
(function (g) {
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const MAX_TEXT = 4 * 1024 * 1024;
  const MAX_DEPTH = 64;
  const MAX_NODES = 200000;
  const MAX_OUT_NODES = 500000;
  const MAX_INT_DIGITS = 300;
  const SAFE = 9007199254740991n;
  const setOwn = (obj, key, value) =>
    Object.defineProperty(obj, key, { value, writable: true, configurable: true, enumerable: true });
  const hasOwn = (obj, key) => Object.prototype.hasOwnProperty.call(obj, key);
  const isWs = (c) => c === " " || c === "\t";
  const isDigit = (c) => c >= "0" && c <= "9";
  const FLOW_STOP = ",[]{}";

  // YAML 1.2 core schema typing of a plain scalar; hand scanned, no regular expressions.
  const bigToNumber = (b) => (b >= -SAFE && b <= SAFE ? Number(b) : b);
  const resolvePlain = (s) => {
    const n = s.length;
    if (n === 0) return null;
    switch (s) {
      case "~": case "null": case "Null": case "NULL": return null;
      case "true": case "True": case "TRUE": return true;
      case "false": case "False": case "FALSE": return false;
      case ".nan": case ".NaN": case ".NAN": return NaN;
      case ".inf": case ".Inf": case ".INF": return Infinity;
      default:
    }
    const c0 = s[0];
    if (!(isDigit(c0) || c0 === "+" || c0 === "-" || c0 === ".")) return s;
    const signed = c0 === "+" || c0 === "-";
    if (signed) {
      const body = s.slice(1);
      if (body === ".inf" || body === ".Inf" || body === ".INF") return c0 === "-" ? -Infinity : Infinity;
    }
    if (!signed && n > 2 && c0 === "0" && (s[1] === "x" || s[1] === "o")) {
      const hex = s[1] === "x";
      for (let k = 2; k < n; k++) {
        const ch = s[k];
        const ok = hex ? isDigit(ch) || (ch >= "a" && ch <= "f") || (ch >= "A" && ch <= "F") : ch >= "0" && ch <= "7";
        if (!ok) return s;
      }
      if (n - 2 > MAX_INT_DIGITS) throw new RangeError("yaml_parse: integer literal is too long");
      return bigToNumber(BigInt(s));
    }
    let i = signed ? 1 : 0;
    let digits = 0;
    while (i < n && isDigit(s[i])) { i++; digits++; }
    let isFloat = false;
    if (s[i] === ".") {
      isFloat = true;
      i++;
      let frac = 0;
      while (i < n && isDigit(s[i])) { i++; frac++; }
      if (digits === 0 && frac === 0) return s;
    } else if (digits === 0) {
      return s;
    }
    if (s[i] === "e" || s[i] === "E") {
      i++;
      if (s[i] === "+" || s[i] === "-") i++;
      let exp = 0;
      while (i < n && isDigit(s[i])) { i++; exp++; }
      if (exp === 0) return s;
      isFloat = true;
    }
    if (i !== n) return s;
    if (isFloat) return Number(s);
    if (digits > MAX_INT_DIGITS) throw new RangeError("yaml_parse: integer literal is too long");
    if (digits <= 15) {
      const v = Number(s);
      return v === 0 ? 0 : v;
    }
    return bigToNumber(BigInt(s));
  };

  const ESCAPES = { 0: "\0", a: "\x07", b: "\b", t: "\t", "\t": "\t", n: "\n", v: "\v", f: "\f", r: "\r", e: "\x1b", " ": " ", '"': '"', "/": "/", "\\": "\\", N: "\x85", _: "\xa0", L: " ", P: " " };

  const parseYaml = (text, all) => {
    const src = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
    // A regular expression split is slow in QuickJS on megabyte inputs; only CR input needs it.
    const L = (src.indexOf("\r") < 0 ? src : src.replace(/\r\n?/g, "\n")).split("\n");
    if (L.length > 1 && L[L.length - 1] === "") L.pop();
    const N = L.length;
    let i = 0;
    let nodes = 0;
    let depth = 0;
    let anchors = new Map();
    let fi = 0;
    let fc = 0;
    let flowStart = 0;

    const err = (msg, line) => {
      throw new SyntaxError("yaml_parse: " + msg + " (line " + (line + 1) + ")");
    };
    const count = () => {
      if (++nodes > MAX_NODES) {
        throw new RangeError("yaml_parse: document expands to more than " + MAX_NODES + " nodes (alias expansion cap)");
      }
    };
    const firstNonWs = (s) => {
      let k = 0;
      while (k < s.length && isWs(s[k])) k++;
      return k;
    };
    const blankOrComment = (s) => {
      const k = firstNonWs(s);
      return k === s.length || s[k] === "#";
    };
    const isMarker = (s, m) => s.startsWith(m) && (s.length === 3 || isWs(s[3]));
    const atMarker = (s) => isMarker(s, "---") || isMarker(s, "...");
    const skipBlank = () => {
      while (i < N && blankOrComment(L[i])) i++;
      return i < N && !atMarker(L[i]);
    };
    const lineIndent = (idx) => {
      const s = L[idx];
      let k = 0;
      while (s[k] === " ") k++;
      if (s[k] === "\t") err("tab characters are not allowed for indentation", idx);
      return k;
    };
    const isSeqEntry = (s, ind) => s[ind] === "-" && (ind + 1 >= s.length || isWs(s[ind + 1]));
    // True when only whitespace or a comment follows column c.
    const eolOk = (s, c) => {
      let k = c;
      while (k < s.length && isWs(s[k])) k++;
      return k >= s.length || (s[k] === "#" && (k > c || c === 0 || isWs(s[c - 1])));
    };
    const readName = (s, c, flow) => {
      let e = c;
      while (e < s.length && !isWs(s[e]) && !(flow && FLOW_STOP.indexOf(s[e]) >= 0)) e++;
      return e;
    };
    const resolveAlias = (name, line) => {
      const a = anchors.get(name);
      if (a === undefined) err("alias refers to an undefined anchor", line);
      nodes += a.size;
      if (nodes > MAX_NODES) {
        throw new RangeError("yaml_parse: document expands to more than " + MAX_NODES + " nodes (alias expansion cap)");
      }
      return a.value;
    };
    // Reads an optional &anchor before a node; tags are refused.
    const readProps = (s, c, line, flow) => {
      let anchor = null;
      while (s[c] === "&" || s[c] === "!") {
        if (s[c] === "!") err("tags (!) are not supported", line);
        if (anchor !== null) err("a node cannot have two anchors", line);
        const e = readName(s, c + 1, flow);
        if (e === c + 1) err("empty anchor name", line);
        anchor = s.slice(c + 1, e);
        c = e;
        while (isWs(s[c])) c++;
      }
      return [anchor, c];
    };
    const merge = (obj, sources, line) => {
      for (const src of sources) {
        for (const k of Object.keys(src)) {
          if (!hasOwn(obj, k)) {
            count();
            setOwn(obj, k, src[k]);
          }
        }
      }
      return line;
    };
    const mergeSources = (value, line) => {
      const one = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
      if (one(value)) return [value];
      if (Array.isArray(value) && value.every(one)) return value;
      return err("the << merge key needs a mapping or a list of mappings", line);
    };

    // A single- or double-quoted scalar starting at line li, column col; may span lines.
    const parseQuoted = (li, col) => {
      const q = L[li][col];
      const dq = q === '"';
      let out = "";
      let prot = 0;
      let line = li;
      let j = col + 1;
      for (;;) {
        const s = L[line];
        let escapedBreak = false;
        while (j < s.length) {
          const ch = s[j];
          if (ch === q) {
            if (!dq && s[j + 1] === q) {
              out += "'";
              prot = out.length;
              j += 2;
              continue;
            }
            return [out, line, j + 1];
          }
          if (dq && ch === "\\") {
            j++;
            const e = s[j];
            if (e === undefined) {
              escapedBreak = true;
              break;
            }
            if (hasOwn(ESCAPES, e)) {
              out += ESCAPES[e];
              j++;
            } else if (e === "x" || e === "u" || e === "U") {
              const len = e === "x" ? 2 : e === "u" ? 4 : 8;
              const hex = s.slice(j + 1, j + 1 + len);
              let ok = hex.length === len;
              for (let k = 0; ok && k < len; k++) {
                const h = hex[k];
                ok = isDigit(h) || (h >= "a" && h <= "f") || (h >= "A" && h <= "F");
              }
              const cp = ok ? parseInt(hex, 16) : -1;
              if (!ok || cp > 0x10ffff || (len === 8 && cp >= 0xd800 && cp <= 0xdfff)) {
                err("invalid escape sequence in a double-quoted scalar", line);
              }
              out += String.fromCodePoint(cp);
              j += 1 + len;
            } else {
              err("invalid escape sequence in a double-quoted scalar", line);
            }
            prot = out.length;
            continue;
          }
          out += ch;
          j++;
        }
        if (!escapedBreak) {
          while (out.length > prot && isWs(out[out.length - 1])) out = out.slice(0, -1);
        } else {
          prot = out.length;
        }
        line++;
        if (line >= N) err("unterminated quoted scalar starting", li);
        let blanks = 0;
        while (line < N && firstNonWs(L[line]) === L[line].length) {
          blanks++;
          line++;
        }
        if (line >= N) err("unterminated quoted scalar starting", li);
        j = firstNonWs(L[line]);
        if (isMarker(L[line], "---") || isMarker(L[line], "...")) err("document marker inside a quoted scalar", line);
        if (!escapedBreak) out += blanks === 0 ? " " : "\n".repeat(blanks);
        else out += "\n".repeat(blanks);
        prot = out.length;
      }
    };

    // Returns {quoted, colon} when the line has a block-mapping key at column c, else null.
    const scanKey = (s, c) => {
      const q = s[c];
      if (q === '"' || q === "'") {
        let j = c + 1;
        for (; j < s.length; j++) {
          if (q === '"' && s[j] === "\\") j++;
          else if (s[j] === q) {
            if (q === "'" && s[j + 1] === "'") j++;
            else break;
          }
        }
        if (j >= s.length) return null;
        let k = j + 1;
        while (isWs(s[k])) k++;
        return s[k] === ":" && (k + 1 >= s.length || isWs(s[k + 1])) ? { quoted: true, colon: k } : null;
      }
      for (let j = c; j < s.length; j++) {
        const ch = s[j];
        if (ch === ":" && (j + 1 >= s.length || isWs(s[j + 1]))) return { quoted: false, colon: j };
        if (ch === "#" && j > c && isWs(s[j - 1])) return null;
      }
      return null;
    };
    const plainLine = (s, c) => {
      let e = s.length;
      for (let j = c; j < s.length; j++) {
        if (s[j] === "#" && j > c && isWs(s[j - 1])) {
          e = j;
          break;
        }
      }
      return s.slice(c, e).trimEnd();
    };

    const parseBlockScalar = (c, parent) => {
      const header = L[i];
      const folded = header[c] === ">";
      let chomp = "clip";
      let explicit = 0;
      let j = c + 1;
      for (let k = 0; k < 2; k++) {
        const ch = header[j];
        if (ch === "+" || ch === "-") {
          if (chomp !== "clip") err("duplicate chomping indicator", i);
          chomp = ch === "+" ? "keep" : "strip";
          j++;
        } else if (ch >= "1" && ch <= "9") {
          if (explicit) err("duplicate indentation indicator", i);
          explicit = +ch;
          j++;
        }
      }
      if (!eolOk(header, j)) err("unexpected content after a block scalar header", i);
      const spaces = (s) => {
        let k = 0;
        while (s[k] === " ") k++;
        return k;
      };
      let bi = explicit ? Math.max(parent, 0) + explicit : -1;
      if (!explicit) {
        let first = i + 1;
        while (first < N && spaces(L[first]) === L[first].length) first++;
        const ind = first < N ? spaces(L[first]) : -1;
        if (ind > parent && !(first < N && ind === 0 && atMarker(L[first]))) bi = ind;
      }
      const body = [];
      let k = i + 1;
      if (bi >= 0) {
        for (; k < N; k++) {
          const s = L[k];
          if (spaces(s) === s.length) body.push(s.length > bi ? s.slice(bi) : "");
          else if (spaces(s) >= bi && !(bi === 0 && atMarker(s))) body.push(s.slice(bi));
          else break;
        }
      }
      i = k;
      let trailing = 0;
      while (body.length > 0 && body[body.length - 1] === "") {
        body.pop();
        trailing++;
      }
      let out = "";
      if (!folded) {
        out = body.join("\n");
      } else {
        let prev = "";
        let blanks = 0;
        for (const line of body) {
          if (line === "") {
            blanks++;
            continue;
          }
          const kind = isWs(line[0]) ? "s" : "n";
          if (prev === "") out += "\n".repeat(blanks);
          else if (prev === "n" && kind === "n") out += blanks === 0 ? " " : "\n".repeat(blanks);
          else out += "\n".repeat(blanks + 1);
          out += line;
          prev = kind;
          blanks = 0;
        }
      }
      if (body.length > 0) {
        if (chomp === "clip") out += "\n";
        else if (chomp === "keep") out += "\n".repeat(trailing + 1);
      } else if (chomp === "keep") {
        out = "\n".repeat(trailing);
      }
      count();
      return out;
    };

    const parsePlain = (c, parent) => {
      const first = L[i];
      const ch = first[c];
      if ("%@`,]}".indexOf(ch) >= 0) err("a plain scalar cannot start with '" + ch + "'", i);
      let text = plainLine(first, c);
      i++;
      let blanks = 0;
      for (; i < N; i++) {
        const s = L[i];
        const k = firstNonWs(s);
        if (k === s.length) {
          blanks++;
          continue;
        }
        if (s[k] === "#" || k <= parent || (k === 0 && atMarker(s))) break;
        if (scanKey(s, k) !== null) err("mapping values are not allowed in this context", i);
        text += blanks === 0 ? " " : "\n".repeat(blanks);
        blanks = 0;
        text += plainLine(s, k);
      }
      count();
      return resolvePlain(text);
    };

    // ---- flow collections (may span lines); position kept in fi/fc ----
    const flowSkip = () => {
      for (;;) {
        const s = L[fi];
        while (fc < s.length && isWs(s[fc])) fc++;
        if (fc < s.length && s[fc] === "#" && (fc === 0 || isWs(s[fc - 1]))) fc = s.length;
        if (fc < s.length) return;
        if (fi + 1 >= N) err("unterminated flow collection starting", flowStart);
        fi++;
        fc = 0;
        if (atMarker(L[fi])) err("document marker inside a flow collection", fi);
      }
    };
    const flowPlain = (isKey) => {
      let text = "";
      for (;;) {
        const s = L[fi];
        let j = fc;
        let ended = false;
        for (; j < s.length; j++) {
          const ch = s[j];
          if (FLOW_STOP.indexOf(ch) >= 0) {
            ended = true;
            break;
          }
          if (ch === ":" && (j + 1 >= s.length || isWs(s[j + 1]) || FLOW_STOP.indexOf(s[j + 1]) >= 0)) {
            ended = true;
            break;
          }
          if (ch === "#" && j > fc && isWs(s[j - 1])) {
            ended = true;
            break;
          }
        }
        text += (text === "" ? s.slice(fc, j) : s.slice(fc, j)).trimEnd();
        fc = j;
        if (ended || isKey) return text;
        // The scalar reached the end of the line: fold a continuation line if one clearly follows.
        let look = fi + 1;
        let blanks = 0;
        while (look < N && firstNonWs(L[look]) === L[look].length) {
          blanks++;
          look++;
        }
        if (look >= N || atMarker(L[look])) return text;
        const k = firstNonWs(L[look]);
        const ch = L[look][k];
        if (ch === "#" || ":,]}[{".indexOf(ch) >= 0) return text;
        text += blanks === 0 ? " " : "\n".repeat(blanks);
        fi = look;
        fc = k;
      }
    };
    const flowNode = () => {
      if (++depth > MAX_DEPTH) throw new RangeError("yaml_parse: nesting deeper than " + MAX_DEPTH);
      try {
        flowSkip();
        const startNodes = nodes;
        const [anchor, c] = readProps(L[fi], fc, fi, true);
        fc = c;
        flowSkip();
        const s = L[fi];
        const ch = s[fc];
        let value;
        if (ch === "*") {
          if (anchor !== null) err("an alias cannot have an anchor", fi);
          const e = readName(s, fc + 1, true);
          const name = s.slice(fc + 1, e);
          fc = e;
          return resolveAlias(name, fi);
        } else if (ch === "[") {
          fc++;
          value = [];
          count();
          for (;;) {
            flowSkip();
            if (L[fi][fc] === "]") {
              fc++;
              break;
            }
            const item = flowNode();
            flowSkip();
            const nx = L[fi][fc];
            if (nx === ":") err("single-pair entries in flow sequences are not supported", fi);
            value.push(item);
            if (nx === ",") fc++;
            else if (nx === "]") {
              fc++;
              break;
            } else err("expected ',' or ']' in a flow sequence", fi);
          }
        } else if (ch === "{") {
          fc++;
          value = {};
          count();
          const merges = [];
          let sawMerge = false;
          for (;;) {
            flowSkip();
            if (L[fi][fc] === "}") {
              fc++;
              break;
            }
            const k0 = L[fi][fc];
            let key;
            let plainKey = false;
            const keyLine = fi;
            if (k0 === '"' || k0 === "'") {
              const r = parseQuoted(fi, fc);
              key = r[0];
              fi = r[1];
              fc = r[2];
            } else if (k0 === "[" || k0 === "{" || (k0 === "?" && isWs(L[fi][fc + 1] || " "))) {
              err("complex mapping keys are not supported", fi);
            } else if ("&*!|>%@`,".indexOf(k0) >= 0) {
              err("unsupported syntax at the start of a flow mapping key", fi);
            } else {
              key = flowPlain(true);
              plainKey = true;
              if (key === "") err("empty key in a flow mapping", fi);
            }
            flowSkip();
            let v = null;
            if (L[fi][fc] === ":") {
              fc++;
              flowSkip();
              const c3 = L[fi][fc];
              v = c3 === "," || c3 === "}" ? null : flowNode();
              flowSkip();
            }
            if (plainKey && key === "<<") {
              if (sawMerge) err("duplicate mapping key", keyLine);
              sawMerge = true;
              for (const m of mergeSources(v, keyLine)) merges.push(m);
            } else {
              if (hasOwn(value, key)) err("duplicate mapping key", keyLine);
              setOwn(value, key, v);
            }
            const nx = L[fi][fc];
            if (nx === ",") fc++;
            else if (nx === "}") {
              fc++;
              break;
            } else err("expected ',' or '}' in a flow mapping", fi);
          }
          merge(value, merges, fi);
        } else if (ch === '"' || ch === "'") {
          const r = parseQuoted(fi, fc);
          fi = r[1];
          fc = r[2];
          value = r[0];
          count();
        } else {
          if ("&|>%@`".indexOf(ch) >= 0) err("unsupported syntax in a flow collection", fi);
          const t = flowPlain(false);
          if (t === "") err("empty flow entry", fi);
          value = resolvePlain(t);
          count();
        }
        if (anchor !== null) anchors.set(anchor, { value, size: nodes - startNodes });
        return value;
      } finally {
        depth--;
      }
    };
    const parseFlowAt = (c) => {
      fi = i;
      fc = c;
      flowStart = i;
      const v = flowNode();
      const s = L[fi];
      let k = fc;
      while (isWs(s[k])) k++;
      if (s[k] === ":" && (k + 1 >= s.length || isWs(s[k + 1]))) {
        err("a flow collection used as a mapping key is not supported", fi);
      }
      if (!eolOk(s, fc)) err("unexpected content after a flow collection", fi);
      i = fi + 1;
      return v;
    };

    // ---- block collections ----
    const parseBlockMapping = (indent) => {
      const obj = {};
      const merges = [];
      let sawMerge = false;
      count();
      for (;;) {
        const line = L[i];
        if (line[indent] === "?" && (indent + 1 >= line.length || isWs(line[indent + 1]))) err("complex mapping keys are not supported", i);
        const k = scanKey(line, indent);
        if (k === null) err("expected a mapping key followed by ':'", i);
        const keyLine = i;
        let key;
        let plainKey = false;
        if (k.quoted) {
          key = parseQuoted(i, indent)[0];
        } else {
          key = line.slice(indent, k.colon).trimEnd();
          const f = key[0];
          if (key === "") err("empty mapping key", i);
          if (f === "*") err("an alias used as a mapping key is not supported", i);
          if (f === "&" || f === "!") err("an anchor or tag on a mapping key is not supported", i);
          if (f === "[" || f === "{") err("a flow collection used as a mapping key is not supported", i);
          if (f === "?" && key.length === 1) err("complex mapping keys are not supported", i);
          if ("|>%@`".indexOf(f) >= 0) err("a mapping key cannot start with '" + f + "'", i);
          plainKey = true;
        }
        let vc = k.colon + 1;
        while (isWs(line[vc])) vc++;
        const value = parseNode(indent, true, false, vc);
        if (plainKey && key === "<<") {
          if (sawMerge) err("duplicate mapping key", keyLine);
          sawMerge = true;
          for (const m of mergeSources(value, keyLine)) merges.push(m);
        } else {
          if (hasOwn(obj, key)) err("duplicate mapping key", keyLine);
          setOwn(obj, key, value);
        }
        if (!skipBlank()) break;
        const ind = lineIndent(i);
        if (ind < indent) break;
        if (ind > indent) err("bad indentation of a mapping entry", i);
        if (isSeqEntry(L[i], ind)) err("unexpected sequence entry inside a mapping", i);
      }
      merge(obj, merges, i);
      return obj;
    };
    const parseBlockSequence = (indent) => {
      const arr = [];
      count();
      for (;;) {
        const line = L[i];
        let vc = indent + 1;
        while (isWs(line[vc])) vc++;
        arr.push(parseNode(indent, false, true, vc));
        if (!skipBlank()) break;
        const ind = lineIndent(i);
        if (ind < indent) break;
        if (ind > indent) err("bad indentation of a sequence entry", i);
        if (!isSeqEntry(L[i], ind)) break;
      }
      return arr;
    };

    // Parses the node starting at line i, column c. `parent` is the indent of the enclosing block
    // collection (-1 at the root); inlineOk says whether a block collection may start on this line.
    const parseNode = (parent, seqOk, inlineOk, c) => {
      if (++depth > MAX_DEPTH) throw new RangeError("yaml_parse: nesting deeper than " + MAX_DEPTH);
      try {
        const startNodes = nodes;
        let [anchor, col] = readProps(L[i], c, i, false);
        let line = L[i];
        let inline = true;
        let value;
        if (col >= line.length || (line[col] === "#" && (col === 0 || isWs(line[col - 1])))) {
          i++;
          let found = skipBlank();
          if (found) {
            const ind = lineIndent(i);
            found = ind > parent || (seqOk && ind === parent && isSeqEntry(L[i], ind));
            col = ind;
          }
          if (!found) {
            count();
            if (anchor !== null) anchors.set(anchor, { value: null, size: nodes - startNodes });
            return null;
          }
          line = L[i];
          inline = false;
        }
        const ch = line[col];
        if (ch === "*") {
          if (anchor !== null) err("an alias cannot have an anchor", i);
          const e = readName(line, col + 1, false);
          const name = line.slice(col + 1, e);
          let k = e;
          while (isWs(line[k])) k++;
          if (line[k] === ":" && (k + 1 >= line.length || isWs(line[k + 1]))) err("an alias used as a mapping key is not supported", i);
          if (!eolOk(line, e)) err("unexpected content after an alias", i);
          const v = resolveAlias(name, i);
          i++;
          return v;
        }
        if (ch === "-" && (col + 1 >= line.length || isWs(line[col + 1]))) {
          if (inline && !inlineOk) err("a block sequence is not allowed here", i);
          value = parseBlockSequence(col);
        } else if (ch === "?" && (col + 1 >= line.length || isWs(line[col + 1]))) {
          err("complex mapping keys are not supported", i);
        } else if (ch === "[" || ch === "{") {
          value = parseFlowAt(col);
        } else if (ch === "|" || ch === ">") {
          value = parseBlockScalar(col, parent);
        } else if (scanKey(line, col) !== null) {
          if (inline && !inlineOk) err("mapping values are not allowed here", i);
          if (inline && anchor !== null) err("an anchor before a mapping key is not supported; put it on its own line", i);
          value = parseBlockMapping(col);
        } else if (ch === '"' || ch === "'") {
          const r = parseQuoted(i, col);
          if (!eolOk(L[r[1]], r[2])) err("unexpected content after a quoted scalar", r[1]);
          value = r[0];
          i = r[1] + 1;
          count();
        } else {
          value = parsePlain(col, parent);
        }
        if (anchor !== null) anchors.set(anchor, { value, size: nodes - startNodes });
        return value;
      } finally {
        depth--;
      }
    };

    // ---- documents ----
    const docs = [];
    for (;;) {
      while (i < N && blankOrComment(L[i])) i++;
      if (i >= N) break;
      const s = L[i];
      if (s[0] === "%") err("directives (%YAML, %TAG) are not supported", i);
      if (isMarker(s, "...")) {
        i++;
        continue;
      }
      anchors = new Map();
      nodes = 0;
      depth = 0;
      let value;
      let empty = false;
      if (isMarker(s, "---")) {
        let c = 3;
        while (isWs(s[c])) c++;
        if (c < s.length && !eolOk(s, c)) {
          value = parseNode(-1, false, false, c);
        } else {
          i++;
          if (skipBlank()) value = parseNode(-1, false, true, lineIndent(i));
          else {
            value = null;
            empty = true;
          }
        }
      } else {
        value = parseNode(-1, false, true, lineIndent(i));
      }
      while (i < N && blankOrComment(L[i])) i++;
      if (i < N) {
        if (isMarker(L[i], "...")) i++;
        else if (!isMarker(L[i], "---")) err("unexpected content after the document root", i);
      }
      docs.push({ value, empty });
    }
    while (docs.length > 0 && docs[docs.length - 1].empty) docs.pop();
    if (all) return docs.map((d) => d.value);
    if (docs.length > 1) throw new SyntaxError("yaml_parse: the text has " + docs.length + " documents; pass {all: true}");
    return docs.length === 0 ? null : docs[0].value;
  };

  define("yaml_parse", function yaml_parse(text, options) {
    if (typeof text !== "string") throw new TypeError("yaml_parse: text must be a string");
    if (text.length > MAX_TEXT) throw new RangeError("yaml_parse: input exceeds " + MAX_TEXT + " characters");
    if (options !== undefined && (options === null || typeof options !== "object")) {
      throw new TypeError("yaml_parse: options must be an object like {all: true}");
    }
    return parseYaml(text, !!(options && options.all));
  });

  // ---- stringify ----
  const quote = (s) => {
    let out = '"';
    for (let k = 0; k < s.length; k++) {
      const ch = s[k];
      const c = s.charCodeAt(k);
      if (ch === '"') out += '\\"';
      else if (ch === "\\") out += "\\\\";
      else if (ch === "\n") out += "\\n";
      else if (ch === "\t") out += "\\t";
      else if (ch === "\r") out += "\\r";
      else if (c < 0x20 || (c >= 0x7f && c <= 0x9f) || c === 0xfeff) out += "\\x" + (c < 16 ? "0" : "") + c.toString(16);
      else if (c === 0x2028 || c === 0x2029) out += "\\u" + c.toString(16);
      else if (c >= 0xd800 && c <= 0xdbff) {
        const d = s.charCodeAt(k + 1);
        if (d >= 0xdc00 && d <= 0xdfff) {
          out += ch + s[k + 1];
          k++;
        } else out += "\\u" + c.toString(16);
      } else if (c >= 0xdc00 && c <= 0xdfff) out += "\\u" + c.toString(16);
      else out += ch;
    }
    return out + '"';
  };
  const FIRST_OK = "_/.$~+()^";
  const plainSafe = (s) => {
    const n = s.length;
    if (n === 0 || s.startsWith("...")) return false;
    const f = s[0];
    if (!((f >= "a" && f <= "z") || (f >= "A" && f <= "Z") || isDigit(f) || FIRST_OK.indexOf(f) >= 0 || s.charCodeAt(0) >= 0xa0)) return false;
    const last = s[n - 1];
    if (isWs(last) || last === ":") return false;
    for (let k = 0; k < n; k++) {
      const c = s.charCodeAt(k);
      if (c < 0x20 || (c >= 0x7f && c <= 0x9f) || c === 0x2028 || c === 0x2029 || c === 0xfeff) return false;
      if (c >= 0xd800 && c <= 0xdfff) return false;
      if (s[k] === ":" && isWs(s[k + 1])) return false;
      if (s[k] === "#" && k > 0 && isWs(s[k - 1])) return false;
    }
    return resolvePlain(s) === s;
  };
  const keyText = (k) => (plainSafe(k) && k !== "<<" ? k : quote(k));
  const literalOk = (s) => {
    if (s.indexOf("\n") < 0) return false;
    let trailing = 0;
    while (trailing < s.length && s[s.length - 1 - trailing] === "\n") trailing++;
    if (trailing === s.length || s[0] === " " || s[0] === "\n") return false;
    for (let k = 0; k < s.length; k++) {
      const c = s.charCodeAt(k);
      if (c === 10 || c === 9) continue;
      if (c < 0x20 || (c >= 0x7f && c <= 0x9f) || c === 0x2028 || c === 0x2029 || c === 0xfeff || (c >= 0xd800 && c <= 0xdfff)) return false;
    }
    return true;
  };

  define("yaml_stringify", function yaml_stringify(value) {
    let emitted = 0;
    const kind = (v) => {
      if (v === null) return "null";
      switch (typeof v) {
        case "string": case "boolean": case "bigint": return typeof v;
        case "number": return "number";
        case "object": {
          const tag = Object.prototype.toString.call(v);
          if (tag === "[object Array]") return "array";
          if (tag === "[object Date]") return "date";
          if (tag === "[object Object]") {
            const proto = Object.getPrototypeOf(v);
            if (proto === null || Object.getPrototypeOf(proto) === null) return "object";
          }
          break;
        }
        default:
      }
      throw new TypeError("yaml_stringify: unsupported value (only plain objects, arrays and primitives)");
    };
    const numberText = (v) => (v !== v ? ".nan" : v === Infinity ? ".inf" : v === -Infinity ? "-.inf" : Object.is(v, -0) ? "-0.0" : String(v));
    // Returns {text} for a one-line scalar (or empty collection), or {block: lines, head} for
    // literal blocks and non-empty collections, with lines unindented relative to `ind`.
    const emit = (v, ind, depth, topLevel) => {
      if (depth > MAX_DEPTH) throw new RangeError("yaml_stringify: nesting deeper than " + MAX_DEPTH + " (or a cycle)");
      if (++emitted > MAX_OUT_NODES) throw new RangeError("yaml_stringify: more than " + MAX_OUT_NODES + " nodes");
      const k = kind(v);
      switch (k) {
        case "null": return { text: "null" };
        case "boolean": return { text: v ? "true" : "false" };
        case "bigint": return { text: v.toString() };
        case "number": return { text: numberText(v) };
        case "date": {
          const t = v.getTime();
          if (t !== t) throw new RangeError("yaml_stringify: invalid Date");
          return { text: v.toISOString() };
        }
        case "string": {
          if (plainSafe(v)) return { text: v };
          if (!topLevel && literalOk(v)) {
            let trailing = 0;
            while (v[v.length - 1 - trailing] === "\n") trailing++;
            const body = v.slice(0, v.length - trailing).split("\n");
            const lines = body.map((l) => (l === "" ? "" : " ".repeat(ind) + l));
            for (let t = 1; t < trailing; t++) lines.push("");
            return { head: "|" + (trailing === 0 ? "-" : trailing === 1 ? "" : "+"), lines };
          }
          return { text: quote(v) };
        }
        default:
      }
      const pad = " ".repeat(ind);
      const lines = [];
      if (k === "array") {
        if (v.length === 0) return { text: "[]" };
        for (const raw of v) {
          const item = raw === undefined ? null : raw;
          const r = emit(item, ind + 2, depth + 1, false);
          if (r.text !== undefined) lines.push(pad + "- " + r.text);
          else if (r.head !== undefined) {
            lines.push(pad + "- " + r.head);
            for (const l of r.lines) lines.push(l);
          } else {
            lines.push(pad + "- " + r.lines[0].slice(ind + 2));
            for (let j = 1; j < r.lines.length; j++) lines.push(r.lines[j]);
          }
        }
        return { lines };
      }
      const keys = Object.keys(v).filter((key) => v[key] !== undefined);
      if (keys.length === 0) return { text: "{}" };
      for (const key of keys) {
        const head = pad + keyText(key) + ":";
        const r = emit(v[key], ind + 2, depth + 1, false);
        if (r.text !== undefined) lines.push(head + " " + r.text);
        else if (r.head !== undefined) {
          lines.push(head + " " + r.head);
          for (const l of r.lines) lines.push(l);
        } else {
          lines.push(head);
          for (const l of r.lines) lines.push(l);
        }
      }
      return { lines };
    };
    const r = emit(value, 0, 0, true);
    return (r.text !== undefined ? r.text : r.lines.join("\n")) + "\n";
  });
})(globalThis);
