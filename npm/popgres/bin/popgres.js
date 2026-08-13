#!/usr/bin/env node
"use strict";

// Thin launcher: npm installed exactly one @popgres/<platform> package for this
// machine (the others are optionalDependencies that skipped on os/cpu mismatch).
// Find its binary and hand over.

const { spawn } = require("node:child_process");
const os = require("node:os");

const PLATFORM = `${process.platform}-${process.arch}`;
const SUPPORTED = [
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64",
  "linux-x64",
  "win32-x64",
];

function binaryPath() {
  const exe = process.platform === "win32" ? "popgres.exe" : "popgres";
  try {
    return require.resolve(`@popgres/${PLATFORM}/bin/${exe}`);
  } catch {
    const supported = SUPPORTED.join(", ");
    const hint = SUPPORTED.includes(PLATFORM)
      ? `The @popgres/${PLATFORM} package is missing. If you installed with --no-optional, reinstall without it.`
      : `popgres has no prebuilt binary for ${PLATFORM}. Supported: ${supported}.`;
    console.error(
      `popgres: ${hint}\nYou can always build from source: cargo install popgres`,
    );
    process.exit(1);
  }
}

const child = spawn(binaryPath(), process.argv.slice(2), { stdio: "inherit" });

// popgres tears its database down when signalled, so it must outlive this
// wrapper. Handling the signals here also stops Node exiting first and
// returning the shell prompt while cleanup is still running.
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    if (child.exitCode === null) child.kill(signal);
  });
}

child.on("error", (error) => {
  console.error(`popgres: failed to launch the binary: ${error.message}`);
  process.exit(1);
});

// Exit exactly as the binary did — `popgres run` forwards your command's code.
child.on("exit", (code, signal) => {
  if (signal) {
    process.exit(128 + (os.constants.signals[signal] ?? 0));
  }
  process.exit(code ?? 1);
});
