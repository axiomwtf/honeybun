class V {
  constructor(public x: number) {}
  get doubled(): number { return this.x * 2; }
  static tag(): string { return "V"; }
}
const v = new V(21);
if (v.doubled !== 42 || V.tag() !== "V") process.exit(1);
class W extends V {
  y: number;
  constructor(x: number) { super(x); this.y = x + 1; }
}
const w = new W(1);
if (w.x !== 1 || w.y !== 2 || w.doubled !== 2) process.exit(1);
