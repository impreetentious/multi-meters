#!/usr/bin/env node

import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const has = (file) => existsSync(path.join(root, file));
const read = (file) => readFileSync(path.join(root, file), "utf8");
const errors = [];

const readmeVersion = read("README.md").match(
  /\*\*Version:\*\*\s+v([0-9]+\.[0-9]+\.[0-9]+)(?:\s|$)/,
)?.[1];
if (!readmeVersion) errors.push("README.md is missing **Version:** vX.Y.Z");

let version = readmeVersion;
if (has("Cargo.toml")) {
  const cargoVersion = read("Cargo.toml").match(
    /\[workspace\.package\][\s\S]*?^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"/m,
  )?.[1];
  if (!cargoVersion) errors.push("Cargo.toml is missing the workspace package version");
  if (version && cargoVersion !== version) {
    errors.push(`Cargo.toml version ${cargoVersion ?? "missing"} != README.md ${version}`);
  }
  version = cargoVersion ?? version;
}

if (version && has("Cargo.lock")) {
  const projectPackages = new Set(["multimeters", "multimeters-cli", "multimeters-core"]);
  for (const block of read("Cargo.lock").split("[[package]]").slice(1)) {
    const name = block.match(/^\s*name\s*=\s*"([^"]+)"/m)?.[1];
    const packageVersion = block.match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1];
    if (name && projectPackages.has(name) && packageVersion !== version) {
      errors.push(`Cargo.lock ${name} version ${packageVersion ?? "missing"} != ${version}`);
    }
  }
}

if (version && has("ui/package.json")) {
  const pkg = JSON.parse(read("ui/package.json"));
  if (pkg.version !== version) {
    errors.push(`ui/package.json version ${pkg.version ?? "missing"} != ${version}`);
  }
  if (has("ui/package-lock.json")) {
    const lock = JSON.parse(read("ui/package-lock.json"));
    if (lock.version !== version) {
      errors.push(`ui/package-lock.json version ${lock.version ?? "missing"} != ${version}`);
    }
    if (lock.packages?.[""]?.version !== version) {
      errors.push(`ui lock root version ${lock.packages?.[""]?.version ?? "missing"} != ${version}`);
    }
  }
}

if (version && has("src-tauri/tauri.conf.json")) {
  const tauri = JSON.parse(read("src-tauri/tauri.conf.json"));
  if (tauri.version !== version) {
    errors.push(`src-tauri/tauri.conf.json version ${tauri.version ?? "missing"} != ${version}`);
  }
}

if (has("crates/cli/src/main.rs") && !read("crates/cli/src/main.rs").includes('env!("CARGO_PKG_VERSION")')) {
  errors.push("the CLI must derive its version from CARGO_PKG_VERSION");
}

if (errors.length) {
  console.error("version-coherence FAILED:");
  for (const error of errors) console.error(` - ${error}`);
  process.exit(1);
}

console.log(`version-coherence OK — ${version}`);
