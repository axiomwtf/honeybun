const helper = require("./helper15.js");
if (helper.twice(10) !== 20 || helper.NAME !== "c15") process.exit(1);
const path = require("path");
if (path.join("a", "b") !== "a/b") process.exit(1);
const os = require("os");
if (typeof os.platform() !== "string") process.exit(1);
