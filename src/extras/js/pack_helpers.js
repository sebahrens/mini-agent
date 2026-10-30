// Lazily evaluated pack of pure helpers; see pack.rs and pack_boot.js.
// Every helper has one catalogue line below (`//@ name | signature | purpose | usage`) that
// `help()` serves; the pack body must define each listed name on globalThis. Keep lines free of `|`.
//@ date_parse | date_parse(text, fmt?) | parse a date string or epoch number to an ISO-8601 UTC string, or null | Formats: ISO 8601 (date, datetime, Z or +hh:mm offset; no offset means UTC), RFC 2822 (Fri, 10 Oct 2025 13:55:36 +0200), nginx (10/Oct/2025:13:55:36 +0200), US MM/DD/YYYY [hh:mm[:ss] [AM/PM]], epoch seconds or milliseconds (10 or more digits, or a number; below 1e11 is seconds). fmt forces one of iso, rfc2822, nginx, us, epoch, epoch_s, epoch_ms; an unknown fmt throws. Never uses local time; impossible dates such as Feb 30 and unparseable text give null. Example: date_parse('10/Oct/2025:13:55:36 +0200') === '2025-10-10T11:55:36.000Z'
//@ date_fmt | date_fmt(ms, fmt?) | format epoch milliseconds in UTC | fmt: iso (default), date (YYYY-MM-DD), rfc2822, nginx, us (MM/DD/YYYY), epoch_s and epoch_ms (numbers), or a template using YYYY MM DD HH mm ss SSS. Always UTC, never local time; a non-finite or out-of-range ms throws RangeError. Example: date_fmt(Date.UTC(2025, 9, 10), 'YYYY-MM-DD HH:mm') === '2025-10-10 00:00'
//@ glob_match | glob_match(pattern, path) | true if the whole path matches the glob | Syntax: * (any run without /), ** (crosses /; as a whole segment, a/**/b also matches a/b and **/x also matches x), ? (one char but not /), [abc] [a-z] [!abc] (never matches /), {a,b} alternatives (nestable, at most 128 expansions), backslash-x is a literal x. Matching is case-sensitive, anchored to the full string and does not special-case dotfiles or ./ prefixes. Pattern max 1024 chars and path max 4096 chars (RangeError); a malformed pattern throws SyntaxError. Runs in time proportional to pattern length times path length. Example: glob_match('src/**/*.rs', 'src/a/b.rs') === true
//@ glob_filter | glob_filter(patterns, paths) | the paths selected by a glob list, in input order | patterns is a string or an array of at most 256 globs, each with the glob_match syntax. A path is kept when the last matching pattern is not negated; a leading ! negates, so ['docs/**', '!docs/a/**'] keeps docs/b.md but not docs/a/b.md. With no matching pattern a path is dropped. Example: glob_filter('**/*.md', ['a.md', 'b/c.md', 'd.txt']) gives ['a.md', 'b/c.md']
(function (g) {
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
  const DAYS_IN_MONTH = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
  const MAX_DATE_TEXT = 100;
  const MAX_MS = 8.64e15;
  const FORMATS = ["iso", "rfc2822", "nginx", "us", "epoch", "epoch_s", "epoch_ms"];
  const two = (n) => (n < 10 ? "0" : "") + n;
  const three = (n) => (n < 10 ? "00" : n < 100 ? "0" : "") + n;
  const monthIndex = (name) => MONTHS.findIndex((m) => m.toLowerCase() === name.toLowerCase());
  const isLeap = (y) => y % 4 === 0 && (y % 100 !== 0 || y % 400 === 0);
  // Date.UTC maps years 0-99 to 1900-1999 and rolls out-of-range fields over, so validate by
  // hand and set the year explicitly. Returns epoch ms or NaN. Never touches local time.
  const utcMs = (y, mo, d, h, mi, s, ms) => {
    if (mo < 1 || mo > 12 || d < 1 || h > 23 || mi > 59 || s > 59) return NaN;
    if (d > DAYS_IN_MONTH[mo - 1] + (mo === 2 && isLeap(y) ? 1 : 0)) return NaN;
    const t = new Date(0);
    t.setUTCFullYear(y, mo - 1, d);
    t.setUTCHours(h, mi, s, ms);
    return t.getTime();
  };
  const offsetMs = (zone) => {
    if (zone === undefined || /^(?:z|gmt|utc|ut)$/i.test(zone)) return 0;
    const m = /^([+-])(\d{2}):?(\d{2})?$/.exec(zone);
    if (!m) return NaN;
    const h = +m[2];
    const mi = m[3] === undefined ? 0 : +m[3];
    if (h > 23 || mi > 59) return NaN;
    return (m[1] === "-" ? -1 : 1) * (h * 60 + mi) * 60000;
  };
  const finish = (local, zone) => {
    const t = local - offsetMs(zone);
    return Number.isNaN(t) || Math.abs(t) > MAX_MS ? null : new Date(t).toISOString();
  };
  const fromEpoch = (value, unit) => {
    const n = typeof value === "number" ? value : Number(value);
    if (!Number.isFinite(n)) return null;
    const ms = unit === "s" ? n * 1000 : unit === "ms" ? n : Math.abs(n) < 1e11 ? n * 1000 : n;
    const t = Math.round(ms);
    return Math.abs(t) > MAX_MS ? null : new Date(t).toISOString();
  };
  const epochUnit = (fmt) => (fmt === "epoch_s" ? "s" : fmt === "epoch_ms" ? "ms" : "");
  const parsers = {
    iso(s) {
      const m = /^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}):(\d{2})(?::(\d{2})(?:[.,](\d{1,9}))?)?(Z|[+-]\d{2}(?::?\d{2})?)?)?$/i.exec(s);
      if (!m) return null;
      const frac = m[7] === undefined ? 0 : +(m[7] + "00").slice(0, 3);
      return finish(utcMs(+m[1], +m[2], +m[3], +(m[4] || 0), +(m[5] || 0), +(m[6] || 0), frac), m[8]);
    },
    rfc2822(s) {
      const m = /^(?:([A-Za-z]{3}),\s*)?(\d{1,2})\s+([A-Za-z]{3})\s+(\d{4})\s+(\d{2}):(\d{2})(?::(\d{2}))?\s*(GMT|UTC|UT|Z|[+-]\d{4})$/i.exec(s);
      if (!m) return null;
      const mo = monthIndex(m[3]);
      if (mo < 0) return null;
      const local = utcMs(+m[4], mo + 1, +m[2], +m[5], +m[6], +(m[7] || 0), 0);
      // A stated weekday that contradicts the date means the text is wrong: refuse it.
      if (m[1] !== undefined && !Number.isNaN(local) && DAYS[new Date(local).getUTCDay()].toLowerCase() !== m[1].toLowerCase()) return null;
      return finish(local, m[8]);
    },
    nginx(s) {
      const m = /^\[?(\d{2})\/([A-Za-z]{3})\/(\d{4}):(\d{2}):(\d{2}):(\d{2}) ([+-]\d{4})\]?$/.exec(s);
      if (!m) return null;
      const mo = monthIndex(m[2]);
      return mo < 0 ? null : finish(utcMs(+m[3], mo + 1, +m[1], +m[4], +m[5], +m[6], 0), m[7]);
    },
    us(s) {
      const m = /^(\d{1,2})\/(\d{1,2})\/(\d{4})(?:[ T](\d{1,2}):(\d{2})(?::(\d{2}))?(?: ?([AP]M))?)?$/i.exec(s);
      if (!m) return null;
      let h = +(m[4] || 0);
      if (m[7] !== undefined) {
        if (h < 1 || h > 12) return null;
        h = (h % 12) + (m[7].toUpperCase() === "PM" ? 12 : 0);
      }
      return finish(utcMs(+m[3], +m[1], +m[2], h, +(m[5] || 0), +(m[6] || 0), 0), undefined);
    },
  };
  define("date_parse", function date_parse(text, fmt) {
    if (fmt !== undefined && !FORMATS.includes(fmt)) {
      throw new RangeError("date_parse: unknown fmt '" + String(fmt).slice(0, 40) + "'; use one of " + FORMATS.join(", "));
    }
    if (typeof text === "number") {
      return fmt === undefined || fmt.startsWith("epoch") ? fromEpoch(text, epochUnit(fmt)) : null;
    }
    if (typeof text !== "string") throw new TypeError("date_parse: text must be a string or number");
    const s = text.trim();
    if (s.length === 0 || s.length > MAX_DATE_TEXT) return null;
    if (fmt === undefined) {
      if (/^-?\d{10,}(?:\.\d+)?$/.test(s)) return fromEpoch(s, "");
      for (const name of ["iso", "rfc2822", "nginx", "us"]) {
        const r = parsers[name](s);
        if (r !== null) return r;
      }
      return null;
    }
    if (fmt.startsWith("epoch")) {
      return /^-?\d+(?:\.\d+)?$/.test(s) ? fromEpoch(s, epochUnit(fmt)) : null;
    }
    return parsers[fmt](s);
  });
  const TEMPLATES = { date: "YYYY-MM-DD", us: "MM/DD/YYYY" };
  define("date_fmt", function date_fmt(ms, fmt) {
    if (typeof ms !== "number" || !Number.isFinite(ms) || Math.abs(ms) > MAX_MS) {
      throw new RangeError("date_fmt: ms must be a finite epoch-milliseconds number");
    }
    const f = fmt === undefined ? "iso" : fmt;
    if (typeof f !== "string" || f.length === 0 || f.length > 200) throw new TypeError("date_fmt: fmt must be a short string");
    if (f === "epoch_ms") return Math.trunc(ms);
    if (f === "epoch_s") return Math.floor(ms / 1000);
    const d = new Date(ms);
    if (f === "iso") return d.toISOString();
    const parts = {
      YYYY: String(d.getUTCFullYear()).padStart(4, "0"),
      MM: two(d.getUTCMonth() + 1),
      DD: two(d.getUTCDate()),
      HH: two(d.getUTCHours()),
      mm: two(d.getUTCMinutes()),
      ss: two(d.getUTCSeconds()),
      SSS: three(d.getUTCMilliseconds()),
    };
    if (f === "rfc2822") {
      return DAYS[d.getUTCDay()] + ", " + parts.DD + " " + MONTHS[d.getUTCMonth()] + " " + parts.YYYY + " " + parts.HH + ":" + parts.mm + ":" + parts.ss + " +0000";
    }
    if (f === "nginx") {
      return parts.DD + "/" + MONTHS[d.getUTCMonth()] + "/" + parts.YYYY + ":" + parts.HH + ":" + parts.mm + ":" + parts.ss + " +0000";
    }
    return (Object.prototype.hasOwnProperty.call(TEMPLATES, f) ? TEMPLATES[f] : f).replace(/YYYY|SSS|MM|DD|HH|mm|ss/g, (token) => parts[token]);
  });
  const GLOB_MAX_PATTERN = 1024;
  const GLOB_MAX_PATH = 4096;
  const GLOB_MAX_PATTERNS = 256;
  const GLOB_MAX_EXPANSIONS = 128;
  // Expand {a,b} alternatives into plain patterns (escapes and [...] classes are skipped).
  const braceExpand = (pattern, out, depth) => {
    if (depth > 8) throw new SyntaxError("glob: braces nested too deeply");
    let open = -1;
    for (let i = 0; i < pattern.length; i++) {
      const c = pattern[i];
      if (c === "\\") i++;
      else if (c === "[") {
        const end = pattern.indexOf("]", i + 2);
        if (end > 0) i = end;
      } else if (c === "{") {
        open = i;
        break;
      }
    }
    if (open < 0) {
      out.push(pattern);
      if (out.length > GLOB_MAX_EXPANSIONS) {
        throw new RangeError("glob: more than " + GLOB_MAX_EXPANSIONS + " brace expansions");
      }
      return;
    }
    let level = 0;
    let start = open + 1;
    const options = [];
    let close = -1;
    for (let i = open; i < pattern.length && close < 0; i++) {
      const c = pattern[i];
      if (c === "\\") i++;
      else if (c === "[") {
        const end = pattern.indexOf("]", i + 2);
        if (end > 0) i = end;
      } else if (c === "{") level++;
      else if (c === "}") {
        level--;
        if (level === 0) {
          options.push(pattern.slice(start, i));
          close = i;
        }
      } else if (c === "," && level === 1) {
        options.push(pattern.slice(start, i));
        start = i + 1;
      }
    }
    if (close < 0) throw new SyntaxError("glob: unmatched '{' in pattern");
    for (const option of options) {
      braceExpand(pattern.slice(0, open) + option + pattern.slice(close + 1), out, depth + 1);
    }
  };
  const globTokens = (pattern) => {
    const tokens = [];
    const n = pattern.length;
    let i = 0;
    while (i < n) {
      const c = pattern[i];
      if (c === "*") {
        let j = i;
        while (j < n && pattern[j] === "*") j++;
        const segmentStart = i === 0 || pattern[i - 1] === "/";
        if (j - i >= 2 && segmentStart && j < n && pattern[j] === "/") {
          if (!tokens.length || tokens[tokens.length - 1].t !== "dirs") tokens.push({ t: "dirs" });
          i = j + 1;
        } else if (j - i >= 2 && segmentStart && j === n) {
          tokens.push({ t: "any" });
          i = j;
        } else {
          if (!tokens.length || tokens[tokens.length - 1].t !== "star") tokens.push({ t: "star" });
          i = j;
        }
      } else if (c === "?") {
        tokens.push({ t: "one" });
        i++;
      } else if (c === "[") {
        let j = i + 1;
        const negate = pattern[j] === "!" || pattern[j] === "^";
        if (negate) j++;
        const items = [];
        let closed = false;
        let first = true;
        while (j < n) {
          let ch = pattern[j];
          if (ch === "]" && !first) {
            closed = true;
            break;
          }
          first = false;
          if (ch === "\\" && j + 1 < n) ch = pattern[++j];
          if (pattern[j + 1] === "-" && j + 2 < n && pattern[j + 2] !== "]") {
            let hi = pattern[j + 2];
            let skip = 3;
            if (hi === "\\" && j + 3 < n) {
              hi = pattern[j + 3];
              skip = 4;
            }
            if (hi < ch) throw new SyntaxError("glob: reversed range '" + ch + "-" + hi + "'");
            items.push([ch, hi]);
            j += skip;
          } else {
            items.push([ch, ch]);
            j++;
          }
        }
        if (!closed) throw new SyntaxError("glob: unterminated '[' in pattern");
        tokens.push({ t: "class", negate, items });
        i = j + 1;
      } else if (c === "\\" && i + 1 < n) {
        tokens.push({ t: "lit", c: pattern[i + 1] });
        i += 2;
      } else {
        tokens.push({ t: "lit", c });
        i++;
      }
    }
    return tokens;
  };
  const compileGlob = (name, pattern) => {
    if (typeof pattern !== "string") throw new TypeError(name + ": pattern must be a string");
    if (pattern.length > GLOB_MAX_PATTERN) {
      throw new RangeError(name + ": pattern exceeds " + GLOB_MAX_PATTERN + " characters");
    }
    const expanded = [];
    braceExpand(pattern, expanded, 0);
    return expanded.map((p) => {
      const tokens = globTokens(p);
      return { tokens, branching: tokens.filter((t) => t.t === "star" || t.t === "dirs" || t.t === "any").length };
    });
  };
  // Memoised search over (token, position) states: at most tokens x length states, so no input
  // can make it exponential or high-degree polynomial; there is no regular expression to backtrack.
  const globRun = (compiled, s) => {
    const tokens = compiled.tokens;
    const T = tokens.length;
    const n = s.length;
    const seen = compiled.branching > 1 ? new Uint8Array(T * (n + 1)) : null;
    const stack = [0, 0];
    while (stack.length) {
      let pos = stack.pop();
      let ti = stack.pop();
      for (;;) {
        if (seen !== null) {
          const key = ti * (n + 1) + pos;
          if (seen[key] === 1) break;
          seen[key] = 1;
        }
        if (ti === T) {
          if (pos === n) return true;
          break;
        }
        const tok = tokens[ti];
        const ch = s[pos];
        if (tok.t === "lit") {
          if (pos < n && ch === tok.c) {
            ti++;
            pos++;
            continue;
          }
          break;
        }
        if (tok.t === "one") {
          if (pos < n && ch !== "/") {
            ti++;
            pos++;
            continue;
          }
          break;
        }
        if (tok.t === "class") {
          if (pos >= n || ch === "/") break;
          let hit = false;
          for (const [lo, hi] of tok.items) {
            if (ch >= lo && ch <= hi) {
              hit = true;
              break;
            }
          }
          if (hit !== tok.negate) {
            ti++;
            pos++;
            continue;
          }
          break;
        }
        if (tok.t === "dirs") {
          // Zero or more whole `segment/` units.
          stack.push(ti + 1, pos);
          const slash = s.indexOf("/", pos);
          if (slash < 0) break;
          pos = slash + 1;
          continue;
        }
        // star (stops at /) or any (crosses /): try ending the run here, else extend it.
        stack.push(ti + 1, pos);
        if (pos < n && (tok.t === "any" || ch !== "/")) {
          pos++;
          continue;
        }
        break;
      }
    }
    return false;
  };
  const globTest = (name, alternatives, path) => {
    if (typeof path !== "string") throw new TypeError(name + ": path must be a string");
    if (path.length > GLOB_MAX_PATH) throw new RangeError(name + ": path exceeds " + GLOB_MAX_PATH + " characters");
    for (const alternative of alternatives) if (globRun(alternative, path)) return true;
    return false;
  };
  define("glob_match", function glob_match(pattern, path) {
    return globTest("glob_match", compileGlob("glob_match", pattern), path);
  });
  define("glob_filter", function glob_filter(patterns, paths) {
    const list = typeof patterns === "string" ? [patterns] : patterns;
    if (!Array.isArray(list)) throw new TypeError("glob_filter: patterns must be a string or an array of strings");
    if (list.length > GLOB_MAX_PATTERNS) throw new RangeError("glob_filter: more than " + GLOB_MAX_PATTERNS + " patterns");
    if (!Array.isArray(paths)) throw new TypeError("glob_filter: paths must be an array of strings");
    const compiled = list.map((p) => {
      if (typeof p !== "string") throw new TypeError("glob_filter: patterns must be strings");
      const negated = p[0] === "!";
      return { negated, glob: compileGlob("glob_filter", negated ? p.slice(1) : p) };
    });
    return paths.filter((path) => {
      let keep = false;
      for (const entry of compiled) if (globTest("glob_filter", entry.glob, path)) keep = !entry.negated;
      return keep;
    });
  });
})(globalThis);
