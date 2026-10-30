(function (g, segments) {
  // segments: array of [catalog, load]; each segment is evaluated lazily and independently.
  const catalog = [];
  for (const seg of segments) for (const e of seg[0]) catalog.push(e);
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const entryText = (e) => e[1] + " - " + e[2];
  define("help", function help(name) {
    if (name === undefined) {
      return catalog.length
        ? "Pure helpers (help('name') shows usage):\n" + catalog.map(entryText).join("\n")
        : "No extra helpers are installed.";
    }
    if (typeof name !== "string") throw new TypeError("help: name must be a string");
    const e = catalog.find((x) => x[0] === name);
    if (!e) throw new RangeError("help: unknown helper '" + name.slice(0, 60) + "'; call help() for the list");
    return entryText(e) + "\n" + e[3];
  });
  let loading = false;
  const ensureLoaded = (seg) => {
    if (seg.done) return;
    if (loading) throw new Error("help pack: re-entrant load");
    loading = true;
    try {
      seg[1]();
    } finally {
      loading = false;
    }
    for (const e of seg[0]) {
      const d = Object.getOwnPropertyDescriptor(g, e[0]);
      if (!d || !("value" in d) || typeof d.value !== "function") {
        throw new Error("help pack: " + e[0] + " was not defined by the pack");
      }
    }
    seg.done = true;
  };
  for (const seg of segments) {
    for (const e of seg[0]) {
      const name = e[0];
      Object.defineProperty(g, name, {
        configurable: true,
        enumerable: false,
        get() {
          ensureLoaded(seg);
          return g[name];
        },
        set(value) {
          define(name, value);
        },
      });
    }
  }
})
