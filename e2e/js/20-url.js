const { pathToFileURL, fileURLToPath } = require("url");
const u = pathToFileURL("/tmp/a b.txt");
if (u.protocol !== "file:") process.exit(1);
if (fileURLToPath(u) !== "/tmp/a b.txt") process.exit(1);
