const m = "2026-11-22".match(/(\d+)-(\d+)-(\d+)/);
if (!m || m[1] !== "2026" || m[3] !== "22") process.exit(1);
if (!/h(el)lo/u.test("héllo") === true) { /* non-match ok */ }
if ("a,b,,c".split(/,/).length !== 4) process.exit(1);
if ("foo123bar".replace(/(\d+)/, "[$1]") !== "foo[123]bar") process.exit(1);
