#!/usr/bin/env node
// Stage the CLI the desktop bundle carries.
//
// `tauri-build` hard-errors when a declared bundle resource is missing, so every
// `cargo` command in `src-tauri/` — and `tauri build` — needs
// `src-tauri/binaries/turnpike-cli[.exe]` staged first. The release workflow gets
// it from its `download-artifact` step; this is the local equivalent.
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..", "..");
const isWindows = process.platform === "win32";

const exe = isWindows ? "turnpike.exe" : "turnpike";
const dest = join(
  root,
  "desktop",
  "src-tauri",
  "binaries",
  `turnpike-cli${isWindows ? ".exe" : ""}`,
);

const build = spawnSync("cargo", ["build", "--release", "--locked"], {
  cwd: root,
  stdio: "inherit",
});
if (build.status !== 0) {
  console.error("stage-cli: `cargo build --release --locked` failed");
  process.exit(build.status ?? 1);
}

const src = join(root, "target", "release", exe);
mkdirSync(dirname(dest), { recursive: true });
copyFileSync(src, dest);
// The payload is copied out and exec'd at install time, so the bit has to
// survive the copy; `npm` does not set it.
if (!isWindows) chmodSync(dest, 0o755);

console.log(`stage-cli: ${src} → ${dest}`);
