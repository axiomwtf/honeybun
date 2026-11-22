class Custom extends Error { constructor(m) { super(m); this.name = "Custom"; } }
try { throw new Custom("die"); } catch (e) { if (!(e instanceof Custom) || e.message !== "die" || e.name !== "Custom") process.exit(1); }
try { null.x; } catch (e) { if (!(e instanceof TypeError)) process.exit(1); }
try { JSON.parse("{"); } catch (e) { if (!(e instanceof SyntaxError)) process.exit(1); }
