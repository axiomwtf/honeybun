const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));
(async () => {
  const a: number = await sleep(10).then(() => 1);
  const [b, c] = await Promise.all([sleep(5).then(() => 2), sleep(5).then(() => 3)]);
  if (a !== 1 || b !== 2 || c !== 3) process.exit(1);
  try { await Promise.reject(new Error("boom")); process.exit(1); } catch (e) {}
  process.exit(0);
})();
