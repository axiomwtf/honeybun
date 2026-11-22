enum Mode { Off = 0, On = 1, Auto = "auto" }
const m: Mode = Mode.On;
if (m !== 1 || Mode.Auto !== "auto" || Mode.Off !== 0) process.exit(1);
const flags: Mode[] = [Mode.Off, Mode.On];
if (flags.length !== 2) process.exit(1);
