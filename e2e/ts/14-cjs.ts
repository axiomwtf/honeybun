import helper = require("./helper15.js");
import path = require("path");
import os = require("os");
if (helper.twice(10) !== 20 || path.join("a", "b") !== "a/b") process.exit(1);
if (typeof os.platform() !== "string") process.exit(1);
