// The script inside the message frame is allowed by hash in three places: its text (FRAME_SCRIPT),
// FRAME_SCRIPT_HASH in ui/app.js and script-src in src-tauri/tauri.conf.json.
// If they drift apart, links in messages silently stop working — this check catches that.
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

const app = readFileSync(new URL("../ui/app.js", import.meta.url), "utf8");
const conf = readFileSync(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8");

const script = JSON.parse(app.match(/^const FRAME_SCRIPT = (".*");$/m)[1]);
const declared = app.match(/^const FRAME_SCRIPT_HASH = "(.*)";$/m)[1];
const actual = `sha256-${createHash("sha256").update(script, "utf8").digest("base64")}`;

const problems = [];
if (declared !== actual) problems.push(`FRAME_SCRIPT_HASH in ui/app.js must be ${actual}`);
if (!conf.includes(`'${actual}'`)) problems.push(`script-src in src-tauri/tauri.conf.json lacks '${actual}'`);

if (problems.length > 0) {
  console.error(`The message frame script hash does not match:\n- ${problems.join("\n- ")}`);
  process.exit(1);
}
console.log("The message frame script hash matches in all three places.");
