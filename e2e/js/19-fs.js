const fs = require("fs");
const path = require("os").tmpdir() + "/honey-e2e-fs.txt";
fs.writeFileSync(path, "hello-e2e");
if (fs.readFileSync(path, "utf8") !== "hello-e2e") process.exit(1);
fs.unlinkSync(path);
