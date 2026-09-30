#!/usr/bin/env node
// Stage the CLI the desktop bundle carries.
//
// `tauri-build` hard-errors when a declared bundle resource is missing, so every
// `cargo` command in `src-tauri/` — and `tauri build` — needs
// `src-tauri/binaries/turnpike-cli[.exe]` staged first. The release workflow gets
// it from its `download-artifact` step; this is the local equivalent, and
// `tauri dev` runs it for you (`beforeDevCommand` in tauri.conf.json) so a fresh
// clone opens the window instead of dying on a missing-resource error.
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..", "..");
const isWindows = process.platform === "win32";

const exe = isWindows ? "turnpike.exe" : "turnpike";
const src = join(root, "target", "release", exe);
const dest = join(
  root,
  "desktop",
  "src-tauri",
  "binaries",
  `turnpike-cli${isWindows ? ".exe" : ""}`,
);

// Skip the build *and* the copy when the staged payload is at least as new as
// the release binary: re-copying would bump the payload's mtime and make
// `tauri-build` re-run its script on every dev start.
//
// The two "cannot compare" cases are not the same, and the difference is what
// keeps this safe in CI:
//   * nothing staged yet → build. This is the fresh-clone path, and the build
//     is also what produces the `turnpike` the supervisor will run.
//   * payload staged but no release binary beside it → leave it. CI's
//     `download-artifact` put the payload there and builds only the desktop
//     crate, so there is no local binary to compare against — and rebuilding
//     would replace the artifact the CLI job published with a second one.
function current() {
  let staged;
  try {
    staged = statSync(dest).mtimeMs;
  } catch {
    return false; // nothing staged yet
  }
  try {
    return staged >= statSync(src).mtimeMs;
  } catch {
    return true; // staged from elsewhere; nothing local to compare against
  }
}

if (current()) {
  console.log(`stage-cli: ${dest} is already current`);
  process.exit(0);
}

const build = spawnSync("cargo", ["build", "--release", "--locked"], {
  cwd: root,
  stdio: "inherit",
});
if (build.status !== 0) {
  console.error("stage-cli: `cargo build --release --locked` failed");
  process.exit(build.status ?? 1);
}

mkdirSync(dirname(dest), { recursive: true });
copyFileSync(src, dest);
// The payload is copied out and exec'd at install time, so the bit has to
// survive the copy; `npm` does not set it.
if (!isWindows) chmodSync(dest, 0o755);

console.log(`stage-cli: ${src} → ${dest}`);
