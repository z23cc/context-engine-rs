import { NerveClient } from "../backend/nerveClient.ts";
import type { RuntimeEvent, RuntimeJob } from "../backend/types.ts";

const args = parseArgs(process.argv.slice(2));
const client = new NerveClient({
  root: args.root ?? process.cwd(),
  binary: args.binary,
  cwd: process.cwd(),
});

const events: RuntimeEvent[] = [];
client.onEvent((event) => events.push(event));

try {
  await client.start();
  const info = await client.info();
  const started = await client.startJob({ kind: "ping" }, { jobId: "smoke-ping" });
  const job = await waitForTerminalJob("smoke-ping");
  const listed = await client.listJobs({ includeTerminal: true, includeResults: false, limit: 10 });
  const result = job.result;
  const ok =
    job.status === "completed" &&
    typeof result === "object" &&
    result !== null &&
    !Array.isArray(result) &&
    (result as Record<string, unknown>).status === "ok";
  if (!ok) throw new Error(`smoke ping job did not complete successfully: ${job.status}`);
  console.log(JSON.stringify({ ok, runtime: info.serverInfo, started, job, listed, events }, null, 2));
} finally {
  await client.stop();
}

async function waitForTerminalJob(jobId: string): Promise<RuntimeJob> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const job = await client.getJob(jobId, { includeResult: true });
    if (job.status === "completed" || job.status === "failed" || job.status === "cancelled") return job;
    await delay(25);
  }
  throw new Error(`timed out waiting for runtime job: ${jobId}`);
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function parseArgs(argv: string[]): { root?: string; binary?: string } {
  const parsed: { root?: string; binary?: string } = {};
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--root") parsed.root = requiredValue(argv, ++index, "--root");
    else if (value === "--binary") parsed.binary = requiredValue(argv, ++index, "--binary");
    else if (value === "--help" || value === "-h") {
      console.log("usage: bun src/cli/smoke.ts [--root PATH] [--binary PATH]");
      process.exit(0);
    } else {
      throw new Error(`unknown argument: ${value}`);
    }
  }
  return parsed;
}

function requiredValue(argv: string[], index: number, flag: string): string {
  const value = argv[index];
  if (!value) throw new Error(`${flag} requires a value`);
  return value;
}
