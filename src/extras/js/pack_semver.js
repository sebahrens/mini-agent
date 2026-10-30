// Lazily evaluated pack segment: semver_cmp, semver_sat; see pack.rs and pack_boot.js.
// Every helper has one catalogue line below (`//@ name | signature | purpose | usage`) that
// `help()` serves; the segment body must define each listed name on globalThis. Keep lines free of `|`.
//@ semver_cmp | semver_cmp(a, b) | compare two SemVer 2.0 versions: -1, 0 or 1 | Strict MAJOR.MINOR.PATCH[-prerelease][+build] with an optional leading v; build metadata is ignored; a prerelease sorts below its release and prerelease identifiers compare numerically (numbers) or by ASCII (text), numbers below text, a shorter list below a longer one with the same start. Numbers may exceed 2^53. Leading zeros, empty identifiers and anything longer than 256 chars throw SyntaxError. Example: semver_cmp('1.0.0-alpha.10', '1.0.0-alpha.9') === 1 and semver_cmp('1.0.0-rc.1', '1.0.0') === -1
//@ semver_sat | semver_sat(version, range, style?) | true if a version satisfies an npm or cargo range | Ranges: alternatives separated by two vertical bars, space (or comma) separated comparators, > >= < <= =, caret ^ and tilde ~, x-ranges (1.x, 1.2.*, *), hyphen ranges (1.2.3 - 2.3.4). style is 'cargo' (default: a bare 1.2.3 or 1.2 means ^1.2.3 or ^1.2) or 'npm' (a bare 1.2.3 is exactly that version and 1.2 means 1.2.x). A prerelease version satisfies a set only if one of its comparators names a prerelease of the same MAJOR.MINOR.PATCH (the npm rule). Not PEP 440. Invalid input throws SyntaxError; range max 1024 chars, 64 sets, 64 comparators per set. Example: semver_sat('1.4.2', '^1.2.3') === true and semver_sat('2.1.0', '~2.0.1') === false
(function (g) {
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const MAX_VERSION = 256;
  const MAX_RANGE = 1024;
  const MAX_SETS = 64;
  const MAX_COMPARATORS = 64;

  const isDigits = (s) => {
    if (s.length === 0) return false;
    for (let i = 0; i < s.length; i++) {
      const c = s.charCodeAt(i);
      if (c < 48 || c > 57) return false;
    }
    return true;
  };
  const isIdent = (s) => {
    if (s.length === 0) return false;
    for (let i = 0; i < s.length; i++) {
      const c = s.charCodeAt(i);
      if (!((c >= 48 && c <= 57) || (c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 45)) return false;
    }
    return true;
  };
  // A numeric component is canonical when it has no leading zero (a lone 0 is fine).
  const isNumeric = (s) => isDigits(s) && (s.length === 1 || s.charCodeAt(0) !== 48);
  const wildcard = (s) => s === "x" || s === "X" || s === "*";
  const fail = (name, what) => {
    throw new SyntaxError(name + ": " + what);
  };

  // Splits "core[-pre][+build]" into {core: string[], pre: string[]|null}; null on malformed text.
  const split = (s) => {
    const plus = s.indexOf("+");
    if (plus >= 0) {
      const build = s.slice(plus + 1).split(".");
      if (!build.every(isIdent)) return null;
      s = s.slice(0, plus);
    }
    const dash = s.indexOf("-");
    let pre = null;
    if (dash >= 0) {
      pre = s.slice(dash + 1).split(".");
      for (const id of pre) {
        if (!isIdent(id) || (isDigits(id) && !isNumeric(id))) return null;
      }
      s = s.slice(0, dash);
    }
    return { core: s.split("."), pre };
  };
  const parseVersion = (name, which, text) => {
    if (typeof text !== "string") throw new TypeError(name + ": " + which + " must be a string");
    let s = text.trim();
    if (s.length > MAX_VERSION) fail(name, which + " exceeds " + MAX_VERSION + " characters");
    if (s.charCodeAt(0) === 118 || s.charCodeAt(0) === 86) s = s.slice(1);
    const parts = split(s);
    if (parts === null || parts.core.length !== 3 || !parts.core.every(isNumeric)) {
      fail(name, which + " is not a valid SemVer 2.0 version (MAJOR.MINOR.PATCH[-pre][+build])");
    }
    return { v: parts.core, pre: parts.pre };
  };
  const cmpNum = (a, b) => (a.length !== b.length ? (a.length < b.length ? -1 : 1) : a < b ? -1 : a > b ? 1 : 0);
  const cmpVersion = (x, y) => {
    for (let i = 0; i < 3; i++) {
      const c = cmpNum(x.v[i], y.v[i]);
      if (c !== 0) return c;
    }
    if (x.pre === null || y.pre === null) return x.pre === y.pre ? 0 : x.pre === null ? 1 : -1;
    const n = Math.min(x.pre.length, y.pre.length);
    for (let i = 0; i < n; i++) {
      const a = x.pre[i];
      const b = y.pre[i];
      if (a === b) continue;
      const an = isDigits(a);
      const bn = isDigits(b);
      if (an && bn) return cmpNum(a, b);
      if (an !== bn) return an ? -1 : 1;
      return a < b ? -1 : 1;
    }
    return x.pre.length === y.pre.length ? 0 : x.pre.length < y.pre.length ? -1 : 1;
  };
  define("semver_cmp", function semver_cmp(a, b) {
    return cmpVersion(parseVersion("semver_cmp", "a", a), parseVersion("semver_cmp", "b", b));
  });

  const bump = (s) => (BigInt(s) + 1n).toString();
  const ver = (M, m, p, pre) => ({ v: [M, m, p], pre: pre || null });
  // A partial version: n numeric components before the first wildcard or the end.
  const parsePartial = (name, s) => {
    if (s.charCodeAt(0) === 118 || s.charCodeAt(0) === 86) s = s.slice(1);
    const parts = split(s);
    if (parts === null || parts.core.length > 3) fail(name, "invalid version in range");
    let n = 0;
    let wild = false;
    for (const c of parts.core) {
      if (wildcard(c)) wild = true;
      else if (wild || !isNumeric(c)) fail(name, "invalid version in range");
      else n++;
    }
    if (parts.pre !== null && (n !== 3 || parts.core.length !== 3)) fail(name, "a prerelease needs a full MAJOR.MINOR.PATCH in range");
    const c = parts.core;
    return { n, wild, v: [c[0], c[1] || "0", c[2] || "0"], pre: parts.pre };
  };
  const NEVER = { never: true };
  const lower = (p) => ({ op: ">=", ver: ver(p.v[0], p.v[1], p.v[2], p.pre) });
  const below = (M, m, p) => ({ op: "<", ver: ver(M, m, p) });
  const above = (M, m, p) => ({ op: ">=", ver: ver(M, m, p) });
  // Expands one operator plus partial into plain comparators ({op, ver}) or NEVER.
  const expand = (name, op, p, style) => {
    const [M, m, q] = p.v;
    const n = p.n;
    if (op === "") op = p.wild || style === "npm" ? "=" : "^";
    if (op === "=") {
      if (n === 3) return [{ op: "=", ver: ver(M, m, q, p.pre) }];
      if (n === 0) return [];
      if (n === 1) return [above(M, "0", "0"), below(bump(M), "0", "0")];
      return [above(M, m, "0"), below(M, bump(m), "0")];
    }
    if (op === "^") {
      if (n === 0) return [];
      let up;
      if (M !== "0" || n === 1) up = M !== "0" ? below(bump(M), "0", "0") : below("1", "0", "0");
      else if (m !== "0" || n === 2) up = m !== "0" ? below("0", bump(m), "0") : below("0", "1", "0");
      else up = q !== "0" ? below("0", "0", bump(q)) : below("0", "0", "1");
      return [lower(p), up];
    }
    if (op === "~" || op === "~>") {
      if (n === 0) return [];
      return [lower(p), n === 1 ? below(bump(M), "0", "0") : below(M, bump(m), "0")];
    }
    if (op === ">") {
      if (n === 0) return NEVER;
      if (n === 3) return [{ op: ">", ver: ver(M, m, q, p.pre) }];
      return [n === 2 ? above(M, bump(m), "0") : above(bump(M), "0", "0")];
    }
    if (op === ">=") return n === 0 ? [] : [lower(p)];
    if (op === "<") {
      if (n === 0) return NEVER;
      return [n === 3 ? { op: "<", ver: ver(M, m, q, p.pre) } : below(M, m, "0")];
    }
    // "<="
    if (n === 0) return [];
    if (n === 3) return [{ op: "<=", ver: ver(M, m, q, p.pre) }];
    return [n === 2 ? below(M, bump(m), "0") : below(bump(M), "0", "0")];
  };
  const OPS = ["<=", ">=", "~>", "<", ">", "=", "^", "~"];
  const splitOp = (token) => {
    for (const op of OPS) if (token.startsWith(op)) return [op, token.slice(op.length)];
    return ["", token];
  };
  const tokens = (text, style) => {
    const out = [];
    let start = -1;
    for (let i = 0; i <= text.length; i++) {
      const c = i < text.length ? text.charCodeAt(i) : 32;
      const sep = c === 32 || c === 9 || c === 10 || c === 13 || (style === "cargo" && c === 44);
      if (sep) {
        if (start >= 0) out.push(text.slice(start, i));
        start = -1;
      } else if (start < 0) {
        start = i;
      }
    }
    return out;
  };
  const parseSet = (name, text, style) => {
    const toks = tokens(text, style);
    if (toks.length > MAX_COMPARATORS * 3) fail(name, "more than " + MAX_COMPARATORS + " comparators in one set");
    const set = [];
    let never = false;
    const add = (list) => {
      if (list === NEVER) never = true;
      else for (const c of list) set.push(c);
    };
    if (toks.length === 0) {
      if (style === "cargo") fail(name, "empty comparator set");
      return set;
    }
    let count = 0;
    for (let i = 0; i < toks.length; i++) {
      if (++count > MAX_COMPARATORS) fail(name, "more than " + MAX_COMPARATORS + " comparators in one set");
      let [op, rest] = splitOp(toks[i]);
      if (rest === "" && op !== "") {
        if (i + 1 >= toks.length) fail(name, "operator without a version");
        rest = toks[++i];
        if (splitOp(rest)[0] !== "") fail(name, "operator followed by another operator");
      }
      if (toks[i + 1] === "-") {
        if (op !== "" || i + 2 >= toks.length) fail(name, "malformed hyphen range");
        const from = parsePartial(name, rest);
        const to = parsePartial(name, toks[i + 2]);
        if (splitOp(toks[i + 2])[0] !== "") fail(name, "malformed hyphen range");
        i += 2;
        add(from.n === 0 ? [] : [lower(from)]);
        add(expand(name, "<=", to, style));
        continue;
      }
      add(expand(name, op, parsePartial(name, rest), style));
    }
    return never ? NEVER : set;
  };
  const parseRange = (name, text, style) => {
    if (style !== "cargo" && style !== "npm") throw new RangeError(name + ": style must be 'cargo' or 'npm'");
    if (typeof text !== "string") throw new TypeError(name + ": range must be a string");
    if (text.length > MAX_RANGE) fail(name, "range exceeds " + MAX_RANGE + " characters");
    const sets = text.split("||");
    if (sets.length > MAX_SETS) fail(name, "more than " + MAX_SETS + " || alternatives");
    return sets.map((s) => parseSet(name, s, style));
  };
  const test = (c, v) => {
    const d = cmpVersion(v, c.ver);
    return c.op === "=" ? d === 0 : c.op === ">" ? d > 0 : c.op === ">=" ? d >= 0 : c.op === "<" ? d < 0 : d <= 0;
  };
  define("semver_sat", function semver_sat(version, range, style) {
    const v = parseVersion("semver_sat", "version", version);
    const sets = parseRange("semver_sat", range, style === undefined ? "cargo" : style);
    for (const set of sets) {
      if (set === NEVER) continue;
      if (!set.every((c) => test(c, v))) continue;
      // A prerelease is only admitted by a comparator on the same MAJOR.MINOR.PATCH with a prerelease.
      if (v.pre !== null && !set.some((c) => c.ver.pre !== null && cmpNum(c.ver.v[0], v.v[0]) === 0 && cmpNum(c.ver.v[1], v.v[1]) === 0 && cmpNum(c.ver.v[2], v.v[2]) === 0)) continue;
      return true;
    }
    return false;
  });
})(globalThis);
