// Build the per-platform npm packages from binaries produced by the release
// workflow. Each one carries a single executable plus `os`/`cpu` fields, so a
// package manager installs only the one that matches the machine.
//
//   node npm/build.mjs <version> <artifacts-dir>
//
// <artifacts-dir> holds one directory per target, named for the Rust target
// triple, each containing the built `popgres` (or `popgres.exe`).

import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));

const TARGETS = [
  { triple: "aarch64-apple-darwin", pkg: "darwin-arm64", os: "darwin", cpu: "arm64" },
  { triple: "x86_64-apple-darwin", pkg: "darwin-x64", os: "darwin", cpu: "x64" },
  { triple: "aarch64-unknown-linux-gnu", pkg: "linux-arm64", os: "linux", cpu: "arm64" },
  { triple: "x86_64-unknown-linux-gnu", pkg: "linux-x64", os: "linux", cpu: "x64" },
  { triple: "x86_64-pc-windows-msvc", pkg: "win32-x64", os: "win32", cpu: "x64" },
];

const [version, artifacts] = process.argv.slice(2);
if (!version || !artifacts) {
  console.error("usage: node npm/build.mjs <version> <artifacts-dir>");
  process.exit(1);
}

for (const target of TARGETS) {
  const exe = target.os === "win32" ? "popgres.exe" : "popgres";
  const source = join(artifacts, target.triple, exe);
  const dir = join(HERE, target.pkg);

  mkdirSync(join(dir, "bin"), { recursive: true });
  copyFileSync(source, join(dir, "bin", exe));
  if (target.os !== "win32") chmodSync(join(dir, "bin", exe), 0o755);

  writeFileSync(
    join(dir, "package.json"),
    `${JSON.stringify(
      {
        name: `@popgres/${target.pkg}`,
        version,
        description: `popgres binary for ${target.pkg}`,
        license: "MIT",
        repository: {
          type: "git",
          url: "git+https://github.com/algolab-cloud/popgres.git",
        },
        os: [target.os],
        cpu: [target.cpu],
        files: [`bin/${exe}`],
      },
      null,
      2,
    )}\n`,
  );
  console.log(`built @popgres/${target.pkg}`);
}

// Keep the wrapper's version and its optionalDependencies in lockstep: a
// mismatch here is how these setups usually break.
const wrapper = join(HERE, "popgres", "package.json");
const manifest = JSON.parse(readFileSync(wrapper, "utf8"));
manifest.version = version;
manifest.optionalDependencies = Object.fromEntries(
  TARGETS.map((target) => [`@popgres/${target.pkg}`, version]),
);
writeFileSync(wrapper, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`stamped popgres@${version}`);
