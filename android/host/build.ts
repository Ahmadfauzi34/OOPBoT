/**
 * Bundles the Termux helper daemon (Google Play Termux has no RUN_COMMAND) into `$OUT/oopbot-helper.mjs`, and
 * copies its shell scripts into `$OUT/termux-helper/`.
 */
import { copyFile, mkdir } from "node:fs/promises";
import { build } from "esbuild";

const out = process.env.OUT;
if (!out) throw new Error("Set OUT to the directory the bundles go in");

const common = { bundle: true, platform: "node", format: "esm", target: "node22", logLevel: "warning" } as const;
const helperScripts = ["oopbot-setup.sh", "run.sh", "start.sh"];

await mkdir(`${out}/termux-helper`, { recursive: true });
await Promise.all([
  build({ ...common, entryPoints: ["src/helper/main.ts"], outfile: `${out}/oopbot-helper.mjs` }),
  ...helperScripts.map((name) => copyFile(`src/helper/${name}`, `${out}/termux-helper/${name}`)),
]);
