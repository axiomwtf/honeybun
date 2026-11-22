const name: string = "honey", n: number = 7;
if (`hi ${name} ${n + 1}` !== "hi honey 8") process.exit(1);
