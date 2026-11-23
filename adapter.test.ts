import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { expect, test } from "bun:test";
import { randomBytes, randomUUID } from "node:crypto";
import { mkdir, mkdtemp, readFile, realpath, rename, rm, writeFile } from "node:fs/promises";
import { request as httpRequest } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { createAdapter, serveHttp } from "./adapter";

const binary = join(import.meta.dir, "target", "debug", process.platform === "win32" ? "intake-agent.exe" : "intake-agent");
const photo = "portrait.png";
const png = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aXx0AAAAASUVORK5CYII=", "base64");

test("real authenticated HTTP and stdio MCP return the CLI photo evidence and selected plan", async () => {
  const fixture = await mkdtemp(join(tmpdir(), "intake-adapter-test-"));
  const photos = join(fixture, "photos");
  const delivery = join(fixture, "delivery");
  const journals = join(fixture, "journals");
  const network = process.env.INTAKE_TEST_NETWORK_ROOT ?? String.raw`\\unavailable.invalid\test\intake`;
  const roots: Record<string, string> = process.platform === "win32" ? { photos, delivery, network } : { photos, delivery };
  const token = randomBytes(32).toString("base64url");
  const dispatch = createAdapter({ binary, roots, journalDirectory: journals });
  const server = serveHttp(dispatch, token, 0);
  const transport = new StdioClientTransport({
    command: process.execPath,
    args: ["--no-env-file", join(import.meta.dir, "adapter.ts"), "mcp"],
    env: { INTAKE_BINARY: binary, INTAKE_ROOTS: JSON.stringify(roots), INTAKE_JOURNALS: journals },
    stderr: "pipe",
  });
  const client = new Client({ name: "intake-test", version: "1.0.0" });
  const request = (input: unknown, authorized = true) => fetch(new URL("/v1/operations", server.url), {
    method: "POST",
    headers: { "Content-Type": "application/json", ...(authorized ? { Authorization: "Bearer " + token } : {}) },
    body: JSON.stringify(input),
  });
  const mcp = (input: Record<string, unknown>) => client.callTool({ name: "intake_read", arguments: { request: input } });
  try {
    await mkdir(photos);
    await mkdir(delivery);
    await mkdir(journals);
    await mkdir(join(photos, "portraits", "2024", "02", "29"), { recursive: true });
    await writeFile(join(photos, photo), png);
    const decisions = [{ file: photo, category: "portraits", date: "2024-02-29" }];
    const decisionFile = join(fixture, "decisions.json");
    await writeFile(decisionFile, JSON.stringify(decisions));
    await client.connect(transport);
    expect((await client.listTools()).tools.map(tool => tool.name)).toEqual(["intake_read"]);

    for (const [input, args] of [
      [{ operation: "photo-evidence", root: "photos", files: [photo] }, ["photo-evidence", photos, photo]],
      [{ operation: "organize", source: "photos", destination: "photos", decisions }, ["organize", photos, photos, decisionFile]],
    ] as const) {
      const cli = Bun.spawnSync([binary, ...args]);
      expect(cli.exitCode).toBe(0);
      const record = JSON.parse(cli.stdout.toString());
      const expected = { protocol_version: 1, record, complete: true };
      const overHttp = await request(input);
      expect(overHttp.status).toBe(200);
      expect(await overHttp.json()).toEqual(expected);
      const overMcp = await mcp(input);
      expect(overMcp.isError).toBeUndefined();
      expect(overMcp.structuredContent).toEqual(expected);
      if (input.operation === "organize" && process.platform === "win32") {
        const verify = { operation: "verify", source: "photos", destination: "photos", plan: record, ids: [1] };
        const mutation = { ...verify, operation: "apply", journal: randomUUID() };
        expect((await request(verify)).status).toBe(200);
        expect((await mcp(verify)).isError).toBeUndefined();
        expect((await request(mutation)).status).toBe(403);
        expect((await mcp(mutation)).isError).toBe(true);
        await writeFile(join(photos, photo), "changed since planning");
        expect((await request(verify)).status).toBe(422);
        expect((await mcp(verify)).isError).toBe(true);
        const execution = createAdapter({ binary, roots, journalDirectory: journals, allowExecution: true });
        const signal = new AbortController().signal;
        await expect(execution({ ...mutation, approval: true }, signal)).rejects.toThrow("Invalid operation or arguments");
        await expect(execution(mutation, signal)).rejects.toThrow("changed");
        await writeFile(join(photos, photo), png);
      }
    }
    expect((await request({ operation: "diagnostics" }, false)).status).toBe(401);
    const diagnostics = await (await request({ operation: "diagnostics" })).json();
    expect(diagnostics.record.root_aliases).toEqual(Object.keys(roots).sort());
    expect(diagnostics.record.execution_enabled).toBe(false);
    expect((await mcp({ operation: "diagnostics" })).structuredContent).toEqual(diagnostics);
    expect((await request({
      operation: "clip-plan", source: "photos", destination: "photos", file: "take.mp4",
      clips: [{ path: "clip.mp4", start_ms: 0, end_ms: 2000 }],
      transcript: { report: {}, extraction_journal: "../arbitrary", action_id: 1 },
    })).status).toBe(400);
    expect((await request({ operation: "apply", approval: true })).status).toBe(400);
    expect((await mcp({ operation: "apply", approval: true })).isError).toBe(true);
    expect((await request({ operation: "receipt", journal: "../arbitrary" })).status).toBe(400);
    const receipt = { operation: "receipt", journal: randomUUID() };
    expect((await request(receipt)).status).toBe(404);
    expect(await (await request({ ...receipt, allow_pending: true })).json()).toEqual({
      protocol_version: 1, record: { status: "pending" }, complete: false,
    });
    expect((await request({ operation: "scan", root: "photos", files: ["../decisions.json"] })).status).toBe(422);
    expect((await request({ operation: "scan", root: "unconfigured", files: [photo] })).status).toBe(422);
    expect(await readFile(join(photos, photo))).toEqual(png);
    const jobFile = join(photos, "job.json");
    await writeFile(jobFile, "{}");
    const photoScan = JSON.parse(Bun.spawnSync([binary, "scan", photos, photo, "job.json"]).stdout.toString());
    const stageReceipt = {
      version: 1,
      stage: photos,
      files: Object.fromEntries(photoScan.assets.map((asset: { relative_path: string; sha256: string }) => [asset.relative_path, asset.sha256])),
    };
    const stageReceiptFile = join(fixture, "stage-receipt.json");
    await writeFile(stageReceiptFile, JSON.stringify(stageReceipt));
    const narrationCli = Bun.spawnSync([binary, "narration-delivery-plan", photos, delivery, stageReceiptFile]);
    expect(narrationCli.exitCode).toBe(0);
    const narrationExpected = { protocol_version: 1, record: JSON.parse(narrationCli.stdout.toString()), complete: true };
    const narrationReq = { operation: "narration-delivery-plan", source: "photos", destination: "delivery", receipt: stageReceipt };
    expect(await (await request(narrationReq)).json()).toEqual(narrationExpected);
    expect((await mcp(narrationReq)).structuredContent).toEqual(narrationExpected);
    await writeFile(jobFile, "{\"tampered\":true}");
    expect((await request(narrationReq)).status).toBe(422);
    expect((await mcp(narrationReq)).isError).toBe(true);
    await rm(jobFile);
    if (process.platform === "win32") {
      const name = "file-link.fixture";
      const scan = { operation: "scan", root: "network", files: [name] };
      const cli = Bun.spawnSync([binary, "scan", network, name]);
      const problem = "Network roots are unsupported for safe path validation";
      expect(cli.exitCode).toBe(2);
      expect(cli.stdout.toString()).toBe("");
      expect(cli.stderr.toString()).toContain(problem);
      const overHttp = await request(scan);
      expect(overHttp.status).toBe(422);
      expect((await overHttp.json()).error).toContain(problem);
      const overMcp = await mcp(scan);
      expect(overMcp.isError).toBe(true);
      expect(overMcp.content).toEqual([{ type: "text", text: cli.stderr.toString().trim() }]);
    }

    const missing = createAdapter({ binary: join(fixture, "missing"), roots, journalDirectory: journals });
    const signal = new AbortController();
    await expect(missing({ operation: "diagnostics" }, signal.signal)).rejects.toThrow("could not start");
    signal.abort();
    await expect(dispatch({ operation: "diagnostics" }, signal.signal)).rejects.toThrow("cancelled");
  } finally {
    await client.close();
    await server.stop(true);
    await rm(fixture, { recursive: true });
  }
}, 15_000);

async function awaitFixture<T>(read: () => Promise<T | false>, failure: string): Promise<T> {
  const deadline = Date.now() + 8000;
  while (Date.now() < deadline) {
    const result = await read();
    if (result !== false) return result;
    await Bun.sleep(40);
  }
  throw new Error(failure);
}

async function rustChild(parent: number | null) {
  if (!parent || !Number.isSafeInteger(parent)) throw new Error("Missing fixture adapter PID");
  return await awaitFixture(async () => {
    const child = Bun.spawn([
      "powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
      `Get-CimInstance Win32_Process -Filter "ParentProcessId=${parent} AND Name='intake-agent.exe'" | Select-Object ProcessId,ExecutablePath | ConvertTo-Json -Compress`,
    ], { stdout: "pipe", stderr: "pipe", windowsHide: true });
    const [code, output, error] = await Promise.all([
      child.exited, new Response(child.stdout).text(), new Response(child.stderr).text(),
    ]);
    if (code !== 0) throw new Error(error || "Could not locate the fixture subprocess");
    const info = output.trim() ? JSON.parse(output) : null;
    return info?.ExecutablePath?.toLowerCase() === binary.toLowerCase() ? Number(info.ProcessId) : false;
  }, "Fixture did not start exactly one known Rust subprocess");
}

async function waitForExit(pid: number, failure: string) {
  await awaitFixture(async () => {
    try {
      process.kill(pid, 0);
      return false;
    } catch {
      return true;
    }
  }, failure);
}

test.skipIf(process.platform !== "win32" || process.env.INTAKE_TEST_DESKTOP !== "1")(
  "desktop HTTP and MCP apply, cancel, time out and recover a real Windows NTFS run",
  async () => {
    const build = Bun.spawnSync([
      "cargo", "test", "--locked", "--lib", "--no-run", "--message-format=json",
    ], { cwd: import.meta.dir });
    expect(build.exitCode, build.stderr.toString()).toBe(0);
    const runner = build.stdout.toString().trim().split("\n").map(line => JSON.parse(line))
      .find(value => value.reason === "compiler-artifact" && value.profile?.test && value.executable)?.executable;
    if (typeof runner !== "string") throw new Error("Cargo did not report the Rust fixture executable");
    const rustFixture = (name: string, env: Record<string, string>) => Bun.spawn([
      runner, "--ignored", "--exact", "execution::tests::" + name,
    ], { env: { ...process.env, ...env }, stdout: "pipe", stderr: "pipe", windowsHide: true });
    const approve = async (pid: number, answer?: string) => {
      const helper = rustFixture("approval_console_worker", {
        INTAKE_TEST_CONSOLE_PID: String(pid), INTAKE_TEST_CONSOLE_ANSWER: answer ?? "",
      });
      const [code, output, error] = await Promise.all([
        helper.exited, new Response(helper.stdout).text(), new Response(helper.stderr).text(),
      ]);
      expect(code, output + error).toBe(0);
    };
    const temp = await realpath(tmpdir());
    const fixture = await realpath(await mkdtemp(join(temp, "intake-desktop-test-")));
    if (dirname(fixture) !== temp) throw new Error("Fixture escaped the local temp directory");
    const incoming = join(fixture, "incoming");
    const sorted = join(incoming, "sorted");
    const roots = { incoming };
    const aliases = { source: "incoming", destination: "incoming" };
    const token = randomBytes(32).toString("base64url");
    const server = serveHttp(createAdapter({ binary, roots, journalDirectory: fixture, allowExecution: true }), token, 0);
    const transport = new StdioClientTransport({
      command: process.execPath, args: ["--no-env-file", join(import.meta.dir, "adapter.ts"), "mcp"],
      env: {
        INTAKE_BINARY: binary, INTAKE_JOURNALS: fixture, INTAKE_ROOTS: JSON.stringify(roots), INTAKE_ALLOW_EXECUTION: "1",
      },
    });
    const client = new Client({ name: "intake-desktop-test", version: "1" });
    const endpoint = new URL("/v1/operations", server.url);
    const headers = { Authorization: "Bearer " + token, "Content-Type": "application/json" };
    const signal = new AbortController();
    const http = (input: Record<string, unknown>) => fetch(endpoint, {
      method: "POST", headers, body: JSON.stringify(input), signal: signal.signal,
    });
    const mcp = (name: string, request: Record<string, unknown>) => client.callTool(
      { name, arguments: { request } }, undefined, { signal: signal.signal },
    );
    const cli = (...args: string[]) => {
      const result = Bun.spawnSync([binary, ...args]);
      expect(result.exitCode, result.stderr.toString()).toBe(0);
      return JSON.parse(result.stdout.toString());
    };
    const journalPath = (id: string) => join(fixture, id + ".jsonl");
    let worker: ReturnType<typeof rustFixture> | undefined;
    try {
      await mkdir(sorted, { recursive: true });
      await writeFile(join(incoming, "a.txt"), "disposable selected bytes");
      await writeFile(join(incoming, "b.txt"), "unselected bytes");
      const plan = cli("plan", incoming, incoming, "sorted", "a.txt", "b.txt");
      const journal = randomUUID();
      const selection = { operation: "apply", ...aliases, plan, journal, ids: [1] };
      expect((await http({ ...selection, approval: true })).status).toBe(400);
      const denied = http(selection);
      await approve(await rustChild(process.pid), "APPROVE " + plan.sha256 + " 2");
      expect((await denied).status).toBe(422);
      expect(await Bun.file(journalPath(journal)).exists()).toBe(false);

      const cancelled = httpRequest(endpoint, { method: "POST", headers });
      cancelled.on("error", () => {});
      let cancelledPid: number;
      try {
        cancelled.end(JSON.stringify(selection));
        cancelledPid = await rustChild(process.pid);
        await approve(cancelledPid);
      } finally {
        cancelled.destroy();
      }
      await waitForExit(cancelledPid, "HTTP disconnect left the approval subprocess running");
      expect(await Bun.file(journalPath(journal)).exists()).toBe(false);

      const apply = http(selection);
      await approve(await rustChild(process.pid), "APPROVE " + plan.sha256 + " 1");
      const selected = await (await apply).json();
      expect(selected.complete).toBe(true);
      expect(selected.record.actions.map((entry: { action: { id: number } }) => entry.action.id)).toEqual([1]);
      expect(selected.record).toEqual(cli("receipt", journalPath(journal)));
      expect(await readFile(join(sorted, "a.txt"), "utf8")).toBe("disposable selected bytes");
      expect(await readFile(join(incoming, "b.txt"), "utf8")).toBe("unselected bytes");
      expect(await Bun.file(join(incoming, "a.txt")).exists()).toBe(false);

      await rename(join(sorted, "a.txt"), join(incoming, "a.txt"));
      worker = rustFixture("interruption_worker", {
        INTAKE_TEST_FIXTURE: fixture, INTAKE_TEST_BOUNDARY: "AfterPublication", INTAKE_TEST_COPY: "no",
      });
      await awaitFixture(async () => await Bun.file(join(fixture, "paused")).exists(), "Worker did not reach publication");
      expect(await readFile(join(sorted, "a.txt"), "utf8")).toBe("disposable selected bytes");
      worker.kill("SIGKILL");
      expect(await worker.exited).not.toBe(0);
      worker = undefined;

      const interrupted = randomUUID();
      await rename(join(fixture, "run.jsonl"), journalPath(interrupted));
      await client.connect(transport);
      const status = { operation: "receipt", journal: interrupted };
      const before = cli("receipt", journalPath(interrupted));
      expect(before.status).toBe("incomplete");
      const incompleteReceipt = { protocol_version: 1, record: before, complete: true };
      expect(await (await http(status)).json()).toEqual(incompleteReceipt);
      expect((await mcp("intake_read", status)).structuredContent).toEqual(incompleteReceipt);
      const recovery = mcp("intake_execute", { operation: "recover", ...aliases, journal: interrupted });
      await approve(await rustChild(transport.pid), "RECOVER " + before.plan_sha256 + " 1");
      const restored = await recovery;
      expect(restored.isError).toBeUndefined();
      const expected = { protocol_version: 1, record: cli("receipt", journalPath(interrupted)), complete: true };
      expect(expected.record.status).toBe("completed");
      expect(restored.structuredContent).toEqual(expected);
      expect((await mcp("intake_read", status)).structuredContent).toEqual(expected);
      expect(await (await http(status)).json()).toEqual(expected);
      expect(await readFile(join(sorted, "a.txt"), "utf8")).toBe("disposable selected bytes");
      expect(await readFile(join(incoming, "b.txt"), "utf8")).toBe("unselected bytes");

      await writeFile(join(incoming, photo), png);
      const caption = "Portrait at sunset.";
      const proposed = await mcp("intake_read", {
        operation: "caption-plan", root: "incoming", decisions: [{ file: photo, caption }],
      });
      expect(proposed.isError).toBeUndefined();
      const { record: captionPlan } = proposed.structuredContent as { record: { sha256: string } };
      const captionJournal = randomUUID();
      const captionRequest = { ...selection, plan: captionPlan, journal: captionJournal };
      const abortMcp = new AbortController();
      const pending = client.callTool(
        { name: "intake_execute", arguments: { request: captionRequest } },
        undefined, { signal: AbortSignal.any([signal.signal, abortMcp.signal]) },
      );
      const mcpPid = await rustChild(transport.pid);
      await approve(mcpPid);
      abortMcp.abort();
      await expect(pending).rejects.toThrow();
      await waitForExit(mcpPid, "MCP cancellation left the approval subprocess running");
      expect(await Bun.file(journalPath(captionJournal)).exists()).toBe(false);
      const applyCaption = mcp("intake_execute", captionRequest);
      await approve(await rustChild(transport.pid), "APPROVE " + captionPlan.sha256 + " 1");
      expect((await applyCaption).structuredContent).toEqual({
        protocol_version: 1, record: cli("receipt", journalPath(captionJournal)), complete: true,
      });
      expect(await readFile(join(incoming, photo + ".caption.txt"), "utf8")).toBe(caption);
      expect(await readFile(join(incoming, photo))).toEqual(png);

      const timeoutJournal = randomUUID();
      const unanswered = http({
        ...selection, plan: cli("plan", incoming, incoming, "sorted", "b.txt"), journal: timeoutJournal,
      });
      const waitingPid = await rustChild(process.pid);
      await approve(waitingPid);
      expect((await unanswered).status).toBe(504);
      await waitForExit(waitingPid, "Timed-out HTTP approval left its subprocess running");
      expect(await Bun.file(journalPath(timeoutJournal)).exists()).toBe(false);
      expect(await Bun.file(join(sorted, "b.txt")).exists()).toBe(false);
      expect(await readFile(join(incoming, "b.txt"), "utf8")).toBe("unselected bytes");
    } finally {
      signal.abort();
      if (worker) {
        worker.kill("SIGKILL");
        await worker.exited;
      }
      await client.close();
      await server.stop(true);
      await rm(fixture, { recursive: true });
    }
  },
  155_000,
);
