class V { constructor(x) { this.x = x; } get doubled() { return this.x * 2; } static tag() { return "V"; } }
const v = new V(21);
if (v.doubled !== 42 || V.tag() !== "V") process.exit(1);
class W extends V { constructor(x) { super(x); this.y = x + 1; } }
const w = new W(1);
if (w.x !== 1 || w.y !== 2 || w.doubled !== 2) process.exit(1);
