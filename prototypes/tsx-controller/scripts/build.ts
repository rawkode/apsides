function run(command: string[]) {
  const result = Bun.spawnSync(command, { stdout: "inherit", stderr: "inherit" });
  if (result.exitCode !== 0) process.exit(result.exitCode || 1);
}

run([process.execPath, "build", "src/cli.ts", "--compile", "--outfile", "dist/controller"]);
// Bun appends the bundle to its runtime; refresh the local macOS signature.
if (process.platform === "darwin") {
  run(["codesign", "--force", "--sign", "-", "dist/controller"]);
  run(["codesign", "--verify", "dist/controller"]);
}
