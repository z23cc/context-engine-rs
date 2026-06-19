// `nerve chat` entry: parse args and launch the full-screen chat UI (ui/app.ts).
//
// The terminal UI is an oh-my-pi-inspired, full-screen differential renderer
// built in pure TS (see ../ui/*). Command/arg helpers live in ./commands.ts and
// are re-exported here so existing tests keep importing them from this module.

import { stdout } from "node:process";
import { parseArgs } from "./commands.ts";
import { runApp } from "../ui/app.ts";

export {
  approvalModeLabel,
  formatModels,
  HELP_TEXT,
  parseApprovalMode,
  parseArgs,
  parseCommand,
  providerModelsTool,
} from "./commands.ts";

if (import.meta.main) {
  runApp(parseArgs(process.argv.slice(2))).catch((err) => {
    // Restore the terminal if we crashed inside the alt-screen.
    stdout.write("\x1b[?25h\x1b[?1049l");
    stdout.write(`fatal: ${(err as Error).message}\n`);
    process.exit(1);
  });
}
