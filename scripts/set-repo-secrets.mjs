// Sets GitHub Actions secrets for a repo. Usage:
//   npm i libsodium-wrappers   (in any node project that requires this)
//   GITHUB_PAT=... node scripts/set-repo-secrets.mjs OWNER REPO NAME1=VALUE1 NAME2=VALUE2
// Encrypts each value with the repo's Actions public key (libsodium sealed box).

import { execSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";

const libDir = path.join(tmpdir(), "opencode", "gh-secrets");
mkdirSync(libDir, { recursive: true });
const require = createRequire(import.meta.url);
try {
  require(path.join(libDir, "node_modules", "libsodium-wrappers"));
} catch {
  execSync("npm install libsodium-wrappers --no-audit --no-fund --silent", {
    cwd: libDir,
    stdio: "inherit",
  });
}
const sodium = require(path.join(libDir, "node_modules", "libsodium-wrappers"));

const [, , owner, repo, ...pairs] = process.argv;
const pat = process.env.GITHUB_PAT;
if (!owner || !repo || !pairs.length || !pat) {
  console.error("usage: GITHUB_PAT=... node set-repo-secrets.mjs OWNER REPO NAME=VALUE ...");
  process.exit(1);
}

await sodium.ready;

const headers = {
  Authorization: `Bearer ${pat}`,
  "X-GitHub-Api-Version": "2022-11-28",
  Accept: "application/vnd.github+json",
};

const keyRes = await fetch(
  `https://api.github.com/repos/${owner}/${repo}/actions/secrets/public-key`,
  { headers }
);
if (!keyRes.ok) throw new Error(`public-key: ${keyRes.status} ${await keyRes.text()}`);
const { key_id, key } = await keyRes.json();
const pubKey = sodium.from_base64(key, sodium.base64_variants.ORIGINAL);

for (const pair of pairs) {
  const eq = pair.indexOf("=");
  const name = pair.slice(0, eq);
  const value = pair.slice(eq + 1);
  const cipher = sodium.crypto_box_seal(sodium.from_string(value), pubKey);
  const res = await fetch(
    `https://api.github.com/repos/${owner}/${repo}/actions/secrets/${name}`,
    {
      method: "PUT",
      headers: { ...headers, "Content-Type": "application/json" },
      body: JSON.stringify({
        encrypted_value: sodium.to_base64(cipher, sodium.base64_variants.ORIGINAL),
        key_id,
      }),
    }
  );
  if (!res.ok) throw new Error(`${name}: ${res.status} ${await res.text()}`);
  console.log(`set ${name}`);
}