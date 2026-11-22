class Cell {
  #v = 0;
  get v() { return this.#v; }
  set v(x) { this.#v = x; }
}
const cell = new Cell();
cell.v = 9;
if (cell.v !== 9) process.exit(1);
