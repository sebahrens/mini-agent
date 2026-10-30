(function (g) {
  const BIG_KEY = "\u0000big";
  const LIMIT = 9007199254740991n;
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  define("splitLines", function splitLines(text) {
    if (typeof text !== "string") throw new TypeError("splitLines: text must be a string");
    const out = text.split(/\r\n|\n/);
    if (out[out.length - 1] === "") out.pop();
    return out;
  });
  define("json_exact", function json_exact(text) {
    if (typeof text !== "string") throw new TypeError("json_exact: text must be a string");
    let out = "";
    const n = text.length;
    let i = 0;
    while (i < n) {
      const c = text[i];
      if (c === '"') {
        let j = i + 1;
        while (j < n && text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
        out += text.slice(i, j + 1);
        i = j + 1;
      } else if (c === "-" || (c >= "0" && c <= "9")) {
        let j = i + 1;
        while (j < n && /[0-9.eE+\-]/.test(text[j])) j++;
        const tok = text.slice(i, j);
        if (/^-?[0-9]{16,}$/.test(tok)) {
          const big = BigInt(tok);
          out += big > LIMIT || big < -LIMIT ? '{"\\u0000big":"' + tok + '"}' : tok;
        } else {
          out += tok;
        }
        i = j;
      } else {
        out += c;
        i++;
      }
    }
    return JSON.parse(out, (key, value) =>
      value !== null && typeof value === "object" && typeof value[BIG_KEY] === "string"
        ? BigInt(value[BIG_KEY])
        : value);
  });
  const MAX_TEXT = 4 * 1024 * 1024;
  const checkText = (name, text) => {
    if (typeof text !== "string") throw new TypeError(name + ": text must be a string");
    if (text.length > MAX_TEXT) throw new RangeError(name + ": input exceeds " + MAX_TEXT + " characters");
  };
  const setOwn = (obj, key, value) =>
    Object.defineProperty(obj, key, { value, writable: true, configurable: true, enumerable: true });
  const hasOwn = (obj, key) => Object.prototype.hasOwnProperty.call(obj, key);
  define("csv_parse", function csv_parse(text, options) {
    checkText("csv_parse", text);
    const header = !!(options && options.header);
    const rows = [];
    let row = [];
    let field = "";
    let any = false;
    let closed = false;
    const n = text.length;
    let i = text.charCodeAt(0) === 0xfeff ? 1 : 0;
    const endRow = () => {
      row.push(field);
      if (row.length > 1 || row[0] !== "" || any) rows.push(row);
      row = [];
      field = "";
      any = false;
      closed = false;
    };
    while (i < n) {
      const c = text[i];
      if (c === '"' && !any) {
        any = true;
        i++;
        for (;;) {
          if (i >= n) throw new SyntaxError("csv_parse: unterminated quoted field");
          if (text[i] === '"') {
            if (text[i + 1] === '"') { field += '"'; i += 2; } else { i++; break; }
          } else field += text[i++];
        }
        closed = true;
      } else if (c === ",") {
        row.push(field);
        field = "";
        any = false;
        closed = false;
        i++;
      } else if (c === "\n" || c === "\r") {
        i += c === "\r" && text[i + 1] === "\n" ? 2 : 1;
        endRow();
      } else {
        if (closed) throw new SyntaxError("csv_parse: text after closing quote");
        field += c;
        any = true;
        i++;
      }
    }
    if (field !== "" || any || row.length) endRow();
    if (!header) return rows;
    if (!rows.length) return [];
    const names = rows[0];
    return rows.slice(1).map((r) => {
      const o = {};
      names.forEach((name, k) => setOwn(o, name, k < r.length ? r[k] : ""));
      return o;
    });
  });
  define("toml_parse", function toml_parse(text) {
    checkText("toml_parse", text);
    const n = text.length;
    let i = 0;
    const fail = (msg) => {
      let line = 1;
      for (let k = 0; k < i && k < n; k++) if (text[k] === "\n") line++;
      throw new SyntaxError("toml_parse: " + msg + " (line " + line + ")");
    };
    const blank = () => { while (text[i] === " " || text[i] === "\t") i++; };
    const comment = () => { if (text[i] === "#") while (i < n && text[i] !== "\n") i++; };
    const ws = () => {
      for (;;) {
        blank();
        comment();
        if (text[i] === "\n" || text[i] === "\r") i++; else break;
      }
    };
    const eol = () => {
      blank();
      comment();
      if (i < n && text[i] !== "\n" && text[i] !== "\r") fail("expected end of line");
    };
    const ESC = { b: "\b", t: "\t", n: "\n", f: "\f", r: "\r", '"': '"', "\\": "\\" };
    const basic = (multi) => {
      let out = "";
      for (;;) {
        if (i >= n) fail("unterminated string");
        const c = text[i];
        if (c === '"') {
          if (!multi) { i++; return out; }
          if (text.startsWith('"""', i) && text[i + 3] !== '"') { i += 3; return out; }
          out += c;
          i++;
        } else if (c === "\\") {
          const e = text[i + 1];
          if (e !== undefined && hasOwn(ESC, e)) { out += ESC[e]; i += 2; }
          else if (e === "u" || e === "U") {
            const len = e === "u" ? 4 : 8;
            const hex = text.slice(i + 2, i + 2 + len);
            if (hex.length !== len || !/^[0-9a-fA-F]+$/.test(hex)) fail("bad unicode escape");
            out += String.fromCodePoint(parseInt(hex, 16));
            i += 2 + len;
          } else if (multi && e !== undefined && /[ \t\r\n]/.test(e)) {
            i++;
            while (i < n && /[ \t\r\n]/.test(text[i])) i++;
          } else fail("bad escape");
        } else if (c === "\n" && !multi) fail("newline in string");
        else { out += c; i++; }
      }
    };
    const literal = (multi) => {
      let end = text.indexOf(multi ? "'''" : "'", i);
      if (end < 0) fail("unterminated string");
      if (multi) while (text[end + 3] === "'") end++;
      const s = text.slice(i, end);
      i = end + (multi ? 3 : 1);
      if (!multi && s.includes("\n")) fail("newline in string");
      return s;
    };
    const string = () => {
      const q = text[i];
      const multi = text.startsWith(q + q + q, i);
      i += multi ? 3 : 1;
      if (multi) {
        if (text[i] === "\r") i++;
        if (text[i] === "\n") i++;
      }
      return q === '"' ? basic(multi) : literal(multi);
    };
    const key = () => {
      const parts = [];
      for (;;) {
        blank();
        if (text[i] === '"' || text[i] === "'") {
          const q = text[i++];
          parts.push(q === '"' ? basic(false) : literal(false));
        } else {
          const m = /^[A-Za-z0-9_-]+/.exec(text.slice(i, i + 256));
          if (!m) fail("expected key");
          parts.push(m[0]);
          i += m[0].length;
        }
        blank();
        if (text[i] === ".") i++; else return parts;
      }
    };
    const scalar = () => {
      const m = /^[^\s,\]\}#]+/.exec(text.slice(i, i + 64));
      if (!m) fail("expected value");
      let tok = m[0];
      if (/^\d{4}-\d{2}-\d{2}/.test(tok) || /^\d{2}:\d{2}:\d{2}/.test(tok)) {
        const t = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?/.exec(text.slice(i, i + 40));
        if (t) tok = t[0];
        i += tok.length;
        return tok;
      }
      i += tok.length;
      if (tok === "true") return true;
      if (tok === "false") return false;
      if (/^[+-]?(inf|nan)$/.test(tok)) return tok.endsWith("nan") ? NaN : tok[0] === "-" ? -Infinity : Infinity;
      const clean = tok.replace(/_/g, "");
      if (/^[+-]?(0|[1-9]\d*)$/.test(clean)) {
        const big = BigInt(clean);
        return big > 9007199254740991n || big < -9007199254740991n ? big : Number(clean);
      }
      if (/^0x[0-9a-fA-F]+$|^0o[0-7]+$|^0b[01]+$/.test(clean)) {
        const big = BigInt(clean);
        return big > 9007199254740991n ? big : Number(big);
      }
      if (/^[+-]?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?$/.test(clean)) return Number(clean);
      fail("invalid value '" + tok + "'");
    };
    const value = (depth) => {
      if (depth > 64) fail("nesting too deep");
      const c = text[i];
      if (c === '"' || c === "'") return string();
      if (c === "[") {
        i++;
        const arr = [];
        for (;;) {
          ws();
          if (text[i] === "]") { i++; return arr; }
          arr.push(value(depth + 1));
          ws();
          if (text[i] === ",") i++;
          else if (text[i] !== "]") fail("expected ',' or ']'");
        }
      }
      if (c === "{") {
        i++;
        const obj = {};
        for (;;) {
          blank();
          if (text[i] === "}") { i++; return obj; }
          assign(obj, depth + 1);
          blank();
          if (text[i] === ",") i++;
          else if (text[i] !== "}") fail("expected ',' or '}'");
        }
      }
      return scalar();
    };
    const descend = (root, path) => {
      let cur = root;
      for (const k of path) {
        let next = hasOwn(cur, k) ? cur[k] : setOwn(cur, k, {})[k];
        if (Array.isArray(next)) {
          if (!next.length) fail("cannot extend empty array '" + k + "'");
          next = next[next.length - 1];
        }
        if (next === null || typeof next !== "object") fail("key '" + k + "' is not a table");
        cur = next;
      }
      return cur;
    };
    const assign = (table, depth) => {
      const path = key();
      blank();
      if (text[i] !== "=") fail("expected '='");
      i++;
      blank();
      const last = path.pop();
      const target = descend(table, path);
      if (hasOwn(target, last)) fail("duplicate key '" + last + "'");
      setOwn(target, last, value(depth));
    };
    const root = {};
    let current = root;
    for (;;) {
      ws();
      if (i >= n) return root;
      if (text[i] === "[") {
        const isArr = text[i + 1] === "[";
        i += isArr ? 2 : 1;
        const path = key();
        blank();
        if (!text.startsWith(isArr ? "]]" : "]", i)) fail("expected end of table header");
        i += isArr ? 2 : 1;
        const last = path.pop();
        const parent = descend(root, path);
        if (isArr) {
          if (!hasOwn(parent, last)) setOwn(parent, last, []);
          if (!Array.isArray(parent[last])) fail("'" + last + "' is not an array of tables");
          current = {};
          parent[last].push(current);
        } else if (!hasOwn(parent, last)) current = setOwn(parent, last, {})[last];
        else if (parent[last] !== null && typeof parent[last] === "object" && !Array.isArray(parent[last])) current = parent[last];
        else fail("'" + last + "' is not a table");
        eol();
      } else {
        assign(current, 0);
        eol();
      }
    }
  });
})(globalThis);
