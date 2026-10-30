// Lazily evaluated pack segment: path, tally, stats; see pack.rs and pack_boot.js.
// Every helper has one catalogue line below (`//@ name | signature | purpose | usage`) that
// `help()` serves; the segment body must define each listed name on globalThis. Keep lines free of `|`.
//@ path | path.basename(p, ext?), dirname(p), extname(p), join(...parts), normalize(p), relative(from, to) | POSIX path strings without any file access | basename ignores trailing / and strips ext only when it is a proper suffix; extname is '' for Makefile, .bashrc and '..', and '.gz' for a.tar.gz; dirname('a') is '.', dirname('/a') is '/'; normalize folds //, . and .. (a leading .. of a relative path stays) and keeps one trailing /; join ignores empty parts and normalizes; relative needs two absolute or two relative paths and throws when from climbs above where to starts. Backslash is an ordinary character. Strings only, max 65536 chars. Example: path.extname('src/Makefile') === '' and path.join('/a', '../b', 'c.txt') === '/b/c.txt'
//@ tally | tally(items, key?) | count items by a key, most frequent first | Returns [[key, count], ...] sorted by count descending, ties by key ascending (numbers numerically, strings by UTF-16 code units, never by locale). key is omitted (count the items themselves), a property name, or a function (item, index) returning a primitive. Keys keep their type, so 1 and '1' are different. Max 1,000,000 items; an object key throws TypeError. Example: tally(['b', 'a', 'b']) gives [['b', 2], ['a', 1]]
//@ stats | stats(nums) | summary statistics of a list of numbers | Returns {n, sum, mean, p50, p90, p95, p99, min, max}. Percentiles use linear interpolation between the two closest ranks, rank = p/100 * (n-1) on the sorted values (the numpy default, not nearest-rank), so p50 of [1,2,3,4] is 2.5. sum is compensated (Neumaier). Throws on an empty list, NaN, Infinity or a non-number; max 1,000,000 numbers. Example: stats([1, 2, 3, 4]).p50 === 2.5
(function (g) {
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const MAX_PATH = 65536;
  const MAX_PARTS = 1024;
  const MAX_ITEMS = 1000000;

  const str = (name, what, value) => {
    if (typeof value !== "string") throw new TypeError(name + ": " + what + " must be a string");
    if (value.length > MAX_PATH) throw new RangeError(name + ": " + what + " exceeds " + MAX_PATH + " characters");
    return value;
  };
  // Segments of a path with empty and "." parts removed and ".." resolved; relative paths keep
  // the ".." they cannot resolve, absolute paths drop them (as the kernel does at the root).
  const resolveSegments = (p, absolute) => {
    const out = [];
    let start = 0;
    for (let i = 0; i <= p.length; i++) {
      if (i < p.length && p.charCodeAt(i) !== 47) continue;
      const seg = p.slice(start, i);
      start = i + 1;
      if (seg === "" || seg === ".") continue;
      if (seg === "..") {
        if (out.length > 0 && out[out.length - 1] !== "..") out.pop();
        else if (!absolute) out.push("..");
      } else {
        out.push(seg);
      }
    }
    return out;
  };
  const normalize = (p) => {
    const absolute = p.charCodeAt(0) === 47;
    let out = (absolute ? "/" : "") + resolveSegments(p, absolute).join("/");
    if (out === "") out = ".";
    if (p.length > 0 && p.charCodeAt(p.length - 1) === 47 && out !== "/" && out.charCodeAt(out.length - 1) !== 47) out += "/";
    return out;
  };
  const trimSlashes = (p) => {
    let end = p.length;
    while (end > 0 && p.charCodeAt(end - 1) === 47) end--;
    return end === p.length ? p : p.slice(0, end);
  };
  const baseOf = (p) => {
    const t = trimSlashes(p);
    return t.slice(t.lastIndexOf("/") + 1);
  };
  const path = function path() {
    throw new TypeError("path is a namespace: call path.join, path.basename, ...; see help('path')");
  };
  const methods = {
    basename(p, ext) {
      str("path.basename", "path", p);
      let name = baseOf(p);
      if (ext !== undefined) {
        str("path.basename", "ext", ext);
        if (ext.length > 0 && name !== ext && name.endsWith(ext)) name = name.slice(0, name.length - ext.length);
      }
      return name;
    },
    dirname(p) {
      str("path.dirname", "path", p);
      const t = trimSlashes(p);
      const cut = t.lastIndexOf("/");
      if (cut < 0) return p.charCodeAt(0) === 47 ? "/" : ".";
      const dir = trimSlashes(t.slice(0, cut));
      return dir === "" ? "/" : dir;
    },
    extname(p) {
      str("path.extname", "path", p);
      const name = baseOf(p);
      const dot = name.lastIndexOf(".");
      return dot <= 0 || name === ".." ? "" : name.slice(dot);
    },
    normalize(p) {
      return normalize(str("path.normalize", "path", p));
    },
    join(...parts) {
      if (parts.length > MAX_PARTS) throw new RangeError("path.join: more than " + MAX_PARTS + " parts");
      let total = 0;
      const kept = [];
      for (const part of parts) {
        str("path.join", "every part", part);
        total += part.length + 1;
        if (total > MAX_PATH) throw new RangeError("path.join: result exceeds " + MAX_PATH + " characters");
        if (part !== "") kept.push(part);
      }
      return kept.length === 0 ? "." : normalize(kept.join("/"));
    },
    relative(from, to) {
      str("path.relative", "from", from);
      str("path.relative", "to", to);
      const absolute = from.charCodeAt(0) === 47;
      if (absolute !== (to.charCodeAt(0) === 47)) {
        throw new RangeError("path.relative: from and to must both be absolute or both relative");
      }
      const a = resolveSegments(from, absolute);
      const b = resolveSegments(to, absolute);
      let common = 0;
      while (common < a.length && common < b.length && a[common] === b[common]) common++;
      const up = a.slice(common);
      if (up.includes("..")) throw new RangeError("path.relative: from climbs above the start of to");
      return up.map(() => "..").concat(b.slice(common)).join("/");
    },
  };
  for (const name of Object.keys(methods)) {
    Object.defineProperty(path, name, { value: methods[name], writable: true, configurable: true, enumerable: true });
  }
  define("path", path);

  const checkItems = (name, items) => {
    if (!Array.isArray(items)) throw new TypeError(name + ": expected an array");
    if (items.length > MAX_ITEMS) throw new RangeError(name + ": more than " + MAX_ITEMS + " items");
  };
  // Total order over primitive keys: undefined, null, boolean, number (NaN last), bigint, string.
  const keyClass = (k) => {
    switch (typeof k) {
      case "undefined": return 0;
      case "boolean": return 2;
      case "number": return 3;
      case "bigint": return 4;
      default: return typeof k === "string" ? 5 : 1;
    }
  };
  const compareKeys = (a, b) => {
    const ca = keyClass(a);
    const cb = keyClass(b);
    if (ca !== cb) return ca - cb;
    if (ca === 3 && (a !== a || b !== b)) return a !== a ? (b !== b ? 0 : 1) : -1;
    return a < b ? -1 : a > b ? 1 : 0;
  };
  define("tally", function tally(items, key) {
    checkItems("tally", items);
    const keyOf =
      key === undefined
        ? (item) => item
        : typeof key === "function"
          ? key
          : typeof key === "string"
            ? (item) => {
                if (item === null || item === undefined) throw new TypeError("tally: cannot read property '" + key.slice(0, 40) + "' of an empty item");
                return item[key];
              }
            : null;
    if (keyOf === null) throw new TypeError("tally: key must be a property name or a function");
    const counts = new Map();
    for (let i = 0; i < items.length; i++) {
      const k = keyOf(items[i], i);
      if (k !== null && (typeof k === "object" || typeof k === "function" || typeof k === "symbol")) {
        throw new TypeError("tally: keys must be primitives (got " + (typeof k === "object" ? "an object" : typeof k) + ")");
      }
      counts.set(k, (counts.get(k) || 0) + 1);
    }
    return Array.from(counts).sort((x, y) => y[1] - x[1] || compareKeys(x[0], y[0]));
  });

  const lerp = (a, b, t) => {
    const v = t < 0.5 ? a + (b - a) * t : b - (b - a) * (1 - t);
    return Number.isFinite(v) ? v : a * (1 - t) + b * t;
  };
  define("stats", function stats(nums) {
    checkItems("stats", nums);
    const n = nums.length;
    if (n === 0) throw new RangeError("stats: the list is empty");
    const sorted = new Float64Array(n);
    let sum = 0;
    let carry = 0;
    for (let i = 0; i < n; i++) {
      const v = nums[i];
      if (typeof v !== "number" || !Number.isFinite(v)) {
        throw new TypeError("stats: item " + i + " is not a finite number (NaN, Infinity and non-numbers are rejected)");
      }
      sorted[i] = v;
      const t = sum + v;
      carry += Math.abs(sum) >= Math.abs(v) ? sum - t + v : v - t + sum;
      sum = t;
    }
    sum += carry;
    sorted.sort();
    const pct = (p) => {
      const rank = (p / 100) * (n - 1);
      const lo = Math.floor(rank);
      return lo + 1 >= n ? sorted[n - 1] : lerp(sorted[lo], sorted[lo + 1], rank - lo);
    };
    return { n, sum, mean: sum / n, p50: pct(50), p90: pct(90), p95: pct(95), p99: pct(99), min: sorted[0], max: sorted[n - 1] };
  });
})(globalThis);
