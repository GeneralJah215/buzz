import path from "node:path";
import { fileURLToPath } from "node:url";

import { runTestCountCheck } from "../../scripts/check-test-counts-core.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const projectRoot = path.resolve(__dirname, "..");

await runTestCountCheck({
  projectRoot,
  roots: ["src-tauri/src", "src-tauri/crates", "src"],
  label: "Desktop",
});
