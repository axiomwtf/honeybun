class Cell {
  #v: number = 0;
  get v(): number { return this.#v; }
  set v(x: number) { this.#v = x; }
}
const cell = new Cell();
cell.v = 9;
if (cell.v !== 9) process.exit(1);
