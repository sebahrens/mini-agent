// Lazily evaluated XML segment of the helper pack; see pack.rs and pack_boot.js.
// One catalogue line per helper (`//@ name | signature | purpose | usage`); keep lines free of `|`.
//@ xml_parse | xml_parse(text) | parse strict XML into a plain tree {name, attrs, children, text} | Elements, attributes, text, CDATA; names keep their prefix (bpmn:task). Comments and processing instructions are checked and dropped. text is the element's own character data joined in order (whitespace-only text beside child elements becomes ''). Rejects DOCTYPE/DTD, undefined entities (only &lt; &gt; &amp; &quot; &apos; and &#N; &#xH; are known), duplicate attributes, mismatched tags and illegal characters with SyntaxError '... (line L, column C)'; input over 4 MiB, depth over 256, over 500k nodes or over 1000 attributes on one element give RangeError. Never fetches anything. Example: xml_parse('<a x="1"><b>t</b></a>').children[0].text === 't'
//@ xml_build | xml_build(node, opts?) | serialise an xml_parse tree back to an XML string | node is {name, attrs?, children?, text?} (text is written before the children). opts: indent (0 to 8 spaces, default compact), declaration (true adds the <?xml?> line), sort (true orders attributes by name, for canonical output). Escapes & < > and quotes, writes numeric references for tab, CR and LF in attributes, never emits CDATA, comments or DOCTYPE; throws on a bad name, a non-string attribute value, an illegal character, depth over 256, over 500k nodes or output over 16 MiB. xml_parse(xml_build(n)) deep-equals n. Example: xml_build({name: 'a', attrs: {x: '1'}, text: 'hi'}) === '<a x="1">hi</a>'
(function (g) {
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const MAX_TEXT = 4 * 1024 * 1024;
  const MAX_OUT = 16 * 1024 * 1024;
  const MAX_DEPTH = 256;
  const MAX_NODES = 500000;
  const MAX_ATTRS = 1000;
  // Characters XML 1.0 forbids outright (controls other than tab/LF/CR, U+FFFE, U+FFFF).
  const BAD_CHARS = /[\u0000-\u0008\u000B\u000C\u000E-\u001F￾￿]/;
  const WS_ONLY = /^[ \t\n]*$/;
  const ENTITIES = { lt: "<", gt: ">", amp: "&", quot: '"', apos: "'" };
  const nameStart = (c) => (c >= 97 && c <= 122) || (c >= 65 && c <= 90) || c === 95 || c === 58 || c >= 128;
  const nameChar = (c) => nameStart(c) || (c >= 48 && c <= 57) || c === 45 || c === 46;
  const isName = (s) => {
    if (typeof s !== "string" || s === "" || !nameStart(s.charCodeAt(0))) return false;
    for (let i = 1; i < s.length; i++) if (!nameChar(s.charCodeAt(i))) return false;
    return true;
  };
  const isWs = (c) => c === 32 || c === 9 || c === 10;
  const setOwn = (obj, key, value) =>
    Object.defineProperty(obj, key, { value, writable: true, enumerable: true, configurable: true });
  const validChar = (cp) =>
    cp === 9 || cp === 10 || cp === 13 || (cp >= 0x20 && cp <= 0xd7ff) || (cp >= 0xe000 && cp <= 0xfffd) || (cp >= 0x10000 && cp <= 0x10ffff);

  define("xml_parse", function xml_parse(text) {
    if (typeof text !== "string") throw new TypeError("xml_parse: text must be a string");
    if (text.length > MAX_TEXT) throw new RangeError("xml_parse: input exceeds " + MAX_TEXT + " characters");
    let src = text.charCodeAt(0) === 0xfeff ? text.slice(1) : text;
    if (src.indexOf("\r") >= 0) src = src.split("\r\n").join("\n").split("\r").join("\n");
    const n = src.length;
    // Positions are turned into line/column only when an error is raised.
    const where = (at) => {
      let line = 1;
      let last = -1;
      for (let p = src.indexOf("\n"); p >= 0 && p < at; p = src.indexOf("\n", p + 1)) {
        line++;
        last = p;
      }
      return " (line " + line + ", column " + (at - last) + ")";
    };
    const fail = (msg, at) => {
      throw new SyntaxError("xml_parse: " + msg + where(at));
    };
    const limit = (msg, at) => {
      throw new RangeError("xml_parse: " + msg + where(at));
    };
    const bad = BAD_CHARS.exec(src);
    if (bad) fail("illegal character U+" + bad[0].charCodeAt(0).toString(16).toUpperCase().padStart(4, "0"), bad.index);
    // Replaces the five predefined entities and numeric references; anything else is an error.
    const decode = (raw, base) => {
      let amp = raw.indexOf("&");
      if (amp < 0) return raw;
      let out = "";
      let from = 0;
      while (amp >= 0) {
        const semi = raw.indexOf(";", amp);
        if (semi < 0 || semi - amp > 10) fail("malformed entity reference", base + amp);
        const body = raw.slice(amp + 1, semi);
        let ch;
        if (Object.prototype.hasOwnProperty.call(ENTITIES, body)) ch = ENTITIES[body];
        else if (body.charCodeAt(0) === 35) {
          const hex = body.charCodeAt(1) === 120;
          const digits = body.slice(hex ? 2 : 1);
          if (!(hex ? /^[0-9a-fA-F]{1,6}$/ : /^[0-9]{1,7}$/).test(digits)) fail("malformed character reference", base + amp);
          const cp = parseInt(digits, hex ? 16 : 10);
          if (!validChar(cp)) fail("character reference to a forbidden character", base + amp);
          ch = String.fromCodePoint(cp);
        } else fail("undefined entity reference (only the five predefined entities are supported)", base + amp);
        out += raw.slice(from, amp) + ch;
        from = semi + 1;
        amp = raw.indexOf("&", from);
      }
      return out + raw.slice(from);
    };
    const readName = (at) => {
      let j = at;
      if (j < n && nameStart(src.charCodeAt(j))) {
        j++;
        while (j < n && nameChar(src.charCodeAt(j))) j++;
      }
      return j;
    };
    const skipWs = (at) => {
      let j = at;
      while (j < n && isWs(src.charCodeAt(j))) j++;
      return j;
    };
    const stack = []; // frames {node, parts, name, at}
    let root = null;
    let nodes = 0;
    let i = 0;
    const text_ = (a, b) => {
      const raw = src.slice(a, b);
      const frame = stack[stack.length - 1];
      if (!frame) {
        if (!WS_ONLY.test(raw)) {
          let k = a;
          while (isWs(src.charCodeAt(k))) k++;
          fail(root ? "text after the root element" : "text before the root element", k);
        }
        return;
      }
      const cdata = raw.indexOf("]]>");
      if (cdata >= 0) fail("']]>' is not allowed in character data", a + cdata);
      frame.parts.push(decode(raw, a));
    };
    const close = (frame) => {
      const node = frame.node;
      const joined = frame.parts.length === 1 ? frame.parts[0] : frame.parts.join("");
      node.text = node.children.length && WS_ONLY.test(joined) ? "" : joined;
      stack.pop();
    };
    while (i < n) {
      const lt = src.indexOf("<", i);
      const end = lt < 0 ? n : lt;
      if (end > i) text_(i, end);
      if (lt < 0) break;
      i = lt;
      const c1 = src.charCodeAt(i + 1);
      if (c1 === 33) {
        // <!
        if (src.startsWith("<!--", i)) {
          const stop = src.indexOf("-->", i + 4);
          if (stop < 0) fail("unterminated comment", i);
          const body = src.slice(i + 4, stop);
          if (body.indexOf("--") >= 0 || body.endsWith("-")) fail("'--' is not allowed inside a comment", i);
          i = stop + 3;
        } else if (src.startsWith("<![CDATA[", i)) {
          const frame = stack[stack.length - 1];
          if (!frame) fail("CDATA outside the root element", i);
          const stop = src.indexOf("]]>", i + 9);
          if (stop < 0) fail("unterminated CDATA section", i);
          frame.parts.push(src.slice(i + 9, stop));
          i = stop + 3;
        } else if (src.startsWith("<!DOCTYPE", i)) {
          fail("DOCTYPE/DTD declarations are not allowed", i);
        } else fail("unsupported markup declaration", i);
      } else if (c1 === 63) {
        // <?
        const tEnd = readName(i + 2);
        if (tEnd === i + 2) fail("processing instruction without a target", i);
        const target = src.slice(i + 2, tEnd);
        if (target.toLowerCase() === "xml" && (target !== "xml" || i !== 0)) {
          fail("the XML declaration is only allowed at the very start", i);
        }
        const stop = src.indexOf("?>", tEnd);
        if (stop < 0) fail("unterminated processing instruction", i);
        if (stop > tEnd && !isWs(src.charCodeAt(tEnd))) fail("malformed processing instruction", i);
        i = stop + 2;
      } else if (c1 === 47) {
        // </name>
        const nEnd = readName(i + 2);
        const frame = stack[stack.length - 1];
        if (!frame) fail("end tag without a start tag", i);
        if (src.slice(i + 2, nEnd) !== frame.name) fail("mismatched end tag", i);
        const gt = skipWs(nEnd);
        if (src.charCodeAt(gt) !== 62) fail("malformed end tag", i);
        close(frame);
        i = gt + 1;
      } else {
        // <name attr="v" ...> or <name .../>
        const nEnd = readName(i + 1);
        if (nEnd === i + 1) fail("invalid element name", i);
        if (!stack.length && root) fail("more than one root element", i);
        if (stack.length >= MAX_DEPTH) limit("nesting deeper than " + MAX_DEPTH + " elements", i);
        if (++nodes > MAX_NODES) limit("more than " + MAX_NODES + " nodes", i);
        const name = src.slice(i + 1, nEnd);
        const attrs = {};
        let attrCount = 0;
        let j = nEnd;
        let selfClose = false;
        for (;;) {
          const k = skipWs(j);
          const c = src.charCodeAt(k);
          if (c === 62) {
            j = k + 1;
            break;
          }
          if (c === 47 && src.charCodeAt(k + 1) === 62) {
            selfClose = true;
            j = k + 2;
            break;
          }
          if (k >= n) fail("unterminated start tag", i);
          if (k === j) fail("expected whitespace, '>' or '/>' in a start tag", k);
          const aEnd = readName(k);
          if (aEnd === k) fail("invalid attribute name", k);
          const aName = src.slice(k, aEnd);
          const eq = skipWs(aEnd);
          if (src.charCodeAt(eq) !== 61) fail("attribute without '='", k);
          const q = skipWs(eq + 1);
          const quote = src.charCodeAt(q);
          if (quote !== 34 && quote !== 39) fail("attribute value must be quoted", q);
          const vEnd = src.indexOf(quote === 34 ? '"' : "'", q + 1);
          if (vEnd < 0) fail("unterminated attribute value", q);
          if (++attrCount > MAX_ATTRS) limit("more than " + MAX_ATTRS + " attributes on one element", k);
          if (++nodes > MAX_NODES) limit("more than " + MAX_NODES + " nodes", k);
          if (Object.prototype.hasOwnProperty.call(attrs, aName)) fail("duplicate attribute", k);
          const rawValue = src.slice(q + 1, vEnd);
          const lt2 = rawValue.indexOf("<");
          if (lt2 >= 0) fail("'<' is not allowed in an attribute value", q + 1 + lt2);
          setOwn(attrs, aName, decode(rawValue.replace(/[\t\n]/g, " "), q + 1));
          j = vEnd + 1;
        }
        const node = { name, attrs, children: [], text: "" };
        const parent = stack[stack.length - 1];
        if (parent) parent.node.children.push(node);
        else root = node;
        const frame = { node, parts: [], name, at: i };
        stack.push(frame);
        if (selfClose) close(frame);
        i = j;
      }
    }
    if (stack.length) fail("unclosed element", stack[stack.length - 1].at);
    if (!root) fail("no root element", n);
    return root;
  });

  const ESC_TEXT = { "&": "&amp;", "<": "&lt;", ">": "&gt;", "\r": "&#13;" };
  const ESC_ATTR = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "\t": "&#9;", "\n": "&#10;", "\r": "&#13;" };

  define("xml_build", function xml_build(node, opts) {
    const o = opts === undefined ? {} : opts;
    if (o === null || typeof o !== "object") throw new TypeError("xml_build: opts must be an object");
    const indent = o.indent === undefined ? 0 : o.indent;
    if (!Number.isInteger(indent) || indent < 0 || indent > 8) throw new RangeError("xml_build: indent must be an integer from 0 to 8");
    const sort = o.sort === true;
    const out = [];
    let size = 0;
    let nodes = 0;
    const push = (s) => {
      size += s.length;
      if (size > MAX_OUT) throw new RangeError("xml_build: output exceeds " + MAX_OUT + " characters");
      out.push(s);
    };
    const clean = (s, what) => {
      if (BAD_CHARS.test(s)) throw new RangeError("xml_build: " + what + " contains a character XML forbids");
      return s;
    };
    const emit = (el, depth) => {
      if (el === null || typeof el !== "object" || Array.isArray(el)) throw new TypeError("xml_build: a node must be an object");
      if (depth >= MAX_DEPTH) throw new RangeError("xml_build: nesting deeper than " + MAX_DEPTH + " elements");
      if (++nodes > MAX_NODES) throw new RangeError("xml_build: more than " + MAX_NODES + " nodes");
      if (!isName(el.name)) throw new TypeError("xml_build: node.name is not a valid XML name");
      const attrs = el.attrs === undefined ? {} : el.attrs;
      if (attrs === null || typeof attrs !== "object" || Array.isArray(attrs)) throw new TypeError("xml_build: attrs must be an object");
      const kids = el.children === undefined ? [] : el.children;
      if (!Array.isArray(kids)) throw new TypeError("xml_build: children must be an array");
      const text = el.text === undefined ? "" : el.text;
      if (typeof text !== "string") throw new TypeError("xml_build: text must be a string");
      let keys = Object.keys(attrs);
      if (keys.length > MAX_ATTRS) throw new RangeError("xml_build: more than " + MAX_ATTRS + " attributes on one element");
      if (sort) keys = keys.sort();
      let head = "<" + el.name;
      for (const key of keys) {
        if (!isName(key)) throw new TypeError("xml_build: attribute name is not a valid XML name");
        const v = attrs[key];
        if (typeof v !== "string" && typeof v !== "number" && typeof v !== "boolean") {
          throw new TypeError("xml_build: attribute values must be strings, numbers or booleans");
        }
        nodes++;
        head += " " + key + '="' + clean(String(v), "an attribute value").replace(/[&<>"\t\n\r]/g, (m) => ESC_ATTR[m]) + '"';
      }
      if (text === "" && kids.length === 0) {
        push(head + "/>");
        return;
      }
      push(head + ">");
      if (text !== "") push(clean(text, "text").replace(/[&<>\r]/g, (m) => ESC_TEXT[m]));
      // Pretty-printing would add whitespace that parses back into the text, so it only applies
      // to elements without character data.
      const pretty = indent > 0 && text === "";
      for (const kid of kids) {
        if (pretty) push("\n" + " ".repeat(indent * (depth + 1)));
        emit(kid, depth + 1);
      }
      if (pretty && kids.length) push("\n" + " ".repeat(indent * depth));
      push("</" + el.name + ">");
    };
    if (o.declaration === true) push('<?xml version="1.0" encoding="UTF-8"?>' + (indent > 0 ? "\n" : ""));
    emit(node, 0);
    return out.join("");
  });
})(globalThis);
