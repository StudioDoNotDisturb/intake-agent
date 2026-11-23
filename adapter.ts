import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { timingSafeEqual } from "node:crypto";
import { lstat, mkdtemp, rmdir, stat, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { isAbsolute, join } from "node:path";
import { z } from "zod";

const version = 1;
const bodyLimit = 8 * 1024 * 1024;
const pathArg = z.string().min(1).max(4096).refine(value => !value.includes("\0"));
const alias = z.string().regex(/^[a-z][a-z0-9_-]{0,63}$/);
const files = z.array(pathArg).min(1).max(1000);
const ids = z.array(z.int().min(1).max(4294967295)).min(1).max(1000);
const journal = z.uuid();
const actionId = z.int().min(1).max(4294967295);
const digest = z.string().regex(/^[a-f0-9]{64}$/);
const decision = z.strictObject({ file: pathArg, expected_sha256: digest.optional() });
const plan = z.record(z.string(), z.unknown());
const readShape = z.discriminatedUnion("operation", [
  z.strictObject({ operation: z.literal("diagnostics") }),
  z.strictObject({ operation: z.enum(["scan", "photo-evidence", "duplicates"]), root: alias, files }),
  z.strictObject({ operation: z.literal("photo-suggest"), root: alias, file: pathArg, model: z.string().max(160) }),
  z.strictObject({ operation: z.literal("video-inventory"), root: alias, files: files.max(32) }),
  z.strictObject({ operation: z.literal("video-archive-plan"), source: alias, destination: alias, files: files.max(32), inventory: plan }),
  z.strictObject({
    operation: z.literal("audio-plan"), source: alias, file: pathArg, destination: alias, output: pathArg,
    track: z.union([z.literal("auto"), z.int().min(0).max(255)]), quality: z.enum(["archival", "high", "portable"]),
  }),
  z.strictObject({ operation: z.literal("transcribe"), root: alias, file: pathArg, language: z.string().min(1).max(16) }),
  z.strictObject({ operation: z.literal("align-notes"), notes: z.string().max(256 * 1024), transcript: plan }),
  z.strictObject({
    operation: z.literal("clip-plan"), source: alias, destination: alias, file: pathArg,
    clips: z.array(z.strictObject({ path: pathArg, start_ms: z.int().min(0), end_ms: z.int().positive() })).min(1).max(32),
    transcript: z.strictObject({ report: plan, extraction_journal: journal, action_id: actionId }).optional(),
  }),
  z.strictObject({ operation: z.literal("verify-package"), source: alias, destination: alias, journal }),
  z.strictObject({ operation: z.literal("library-import-plan"), source: alias, destination: alias }),
  z.strictObject({ operation: z.literal("library-listening-plan"), source: alias, destination: alias, chapters: ids }),
  z.strictObject({
    operation: z.enum(["library-text-plan", "library-transcript-plan"]), book: alias, source: alias, destination: alias,
  }),
  z.strictObject({
    operation: z.enum(["library-text-search", "library-transcript-search"]), book: alias, catalog: alias,
    query: z.string().min(1).max(1024),
  }),
  z.strictObject({ operation: z.literal("speech-preview"), root: alias, file: pathArg, voice: z.string().regex(/^[a-zA-Z0-9_]{1,64}$/) }),
  z.strictObject({ operation: z.literal("plan"), source: alias, destination: alias, directory: pathArg, files }),
  z.strictObject({ operation: z.literal("narration-delivery-plan"), source: alias, destination: alias, receipt: plan }),
  z.strictObject({
    operation: z.literal("organize"), source: alias, destination: alias,
    decisions: z.array(decision.extend({ date: z.string().max(10), category: pathArg.optional(), name: pathArg.optional() })).min(1).max(1000),
  }),
  z.strictObject({
    operation: z.literal("caption-plan"), root: alias,
    decisions: z.array(decision.extend({ caption: z.string().max(4096) })).min(1).max(1000),
  }),
  z.strictObject({ operation: z.literal("verify"), source: alias, destination: alias, plan, ids }),
  z.strictObject({ operation: z.literal("receipt"), journal, allow_pending: z.boolean().optional() }),
  z.strictObject({ operation: z.literal("undo-plan"), root: alias, journal, ids }),
]);
const executionShape = z.discriminatedUnion("operation", [
  z.strictObject({ operation: z.literal("apply"), source: alias, destination: alias, plan, journal, ids }),
  z.strictObject({ operation: z.literal("recover"), source: alias, destination: alias, journal }),
]);
const shape = z.discriminatedUnion("operation", [...readShape.options, ...executionShape.options]);

class AdapterError extends Error {
  constructor(readonly status: number, message: string) {
    super(message);
  }
}

async function readBounded(stream: ReadableStream<Uint8Array>, limit: number): Promise<string> {
  const reader = stream.getReader();
  const decoder = new TextDecoder("utf-8", { fatal: true });
  let bytes = 0;
  const parts: string[] = [];
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      bytes += value.byteLength;
      if (bytes > limit) throw new AdapterError(413, "Message exceeds the byte limit");
      parts.push(decoder.decode(value, { stream: true }));
    }
    parts.push(decoder.decode());
    return parts.join("");
  } finally {
    reader.releaseLock();
  }
}

async function subprocess(binary: string, operation: string, args: string[], signal: AbortSignal, env: NodeJS.ProcessEnv) {
  const execution = operation === "apply" || operation === "recover";
  const longReads = ["video-inventory", "transcribe", "speech-preview", "photo-suggest"];
  const budget = AbortSignal.timeout(execution || longReads.includes(operation) ? 120_000 : 30_000);
  const inspect = execution ? ". Check the journal receipt before retrying" : "";
  const stop = new AbortController();
  const combined = AbortSignal.any([budget, signal, stop.signal]);
  if (signal.aborted) throw new AdapterError(499, "Operation cancelled");
  let child;
  try {
    child = Bun.spawn([binary, "rpc"], {
      stdin: new TextEncoder().encode(JSON.stringify({ protocol_version: version, operation, arguments: args })),
      stdout: "pipe",
      stderr: "pipe",
      signal: combined,
      killSignal: "SIGKILL",
      windowsHide: true,
      env,
    });
  } catch {
    throw new AdapterError(503, "Rust backend could not start");
  }
  try {
    const [code, output, error] = await Promise.all([
      child.exited,
      readBounded(child.stdout, bodyLimit),
      readBounded(child.stderr, 16 * 1024),
    ]);
    if (signal.aborted) throw new AdapterError(499, "Operation cancelled" + inspect);
    if (budget.aborted) throw new AdapterError(504, "Rust operation timed out" + inspect);
    if (code !== 0 && code !== 3) {
      throw new AdapterError(code === 2 ? 422 : 502, error.trim() || "Rust operation failed");
    }
    const envelope = z.strictObject({
      protocol_version: z.literal(version),
      record: z.record(z.string(), z.unknown()),
    }).safeParse(JSON.parse(output));
    if (!envelope.success) throw new AdapterError(502, "Unexpected Rust subprocess envelope");
    return { ...envelope.data, complete: code === 0 };
  } catch (error) {
    if (error instanceof AdapterError) throw error;
    if (signal.aborted) throw new AdapterError(499, "Operation cancelled" + inspect);
    if (budget.aborted) throw new AdapterError(504, "Rust operation timed out" + inspect);
    throw new AdapterError(502, "Invalid Rust subprocess output");
  } finally {
    stop.abort();
    await child.exited;
  }
}

const engineSettings = [
  "INTAKE_FFPROBE", "INTAKE_FFMPEG", "INTAKE_WHISPER_PYTHON", "INTAKE_WHISPER_MODEL",
  "INTAKE_KOKORO_PYTHON", "INTAKE_KOKORO_MODEL", "INTAKE_KOKORO_VOICES",
] as const;

async function withRequestFiles<T>(
  directory: string,
  contents: string[],
  invoke: (paths: string[]) => Promise<T>,
) {
  const bytes = contents.map(content => new TextEncoder().encode(content));
  if (bytes.reduce((total, current) => total + current.byteLength, 0) > bodyLimit) {
    throw new AdapterError(413, "Request documents exceed the byte limit");
  }
  const dir = await mkdtemp(join(directory, "intake-request-"));
  const paths = contents.map((_, index) => join(dir, `request-${index}`));
  try {
    for (const [index, path] of paths.entries()) {
      await writeFile(path, bytes[index], { flag: "wx", mode: 0o600 });
    }
    return await invoke(paths);
  } finally {
    for (const path of paths) {
      await unlink(path).catch((error: NodeJS.ErrnoException) => {
        if (error.code !== "ENOENT") throw error;
      });
    }
    await rmdir(dir);
  }
}

export function createAdapter(config: {
  binary: string; roots: Record<string, string>; journalDirectory: string;
  allowExecution?: boolean; executionRoot?: string;
  engines?: Record<string, string | undefined>;
}) {
  const absolute = pathArg.refine(isAbsolute);
  const roots = new Map(Object.entries(z.record(alias, absolute).parse(config.roots)));
  const binary = absolute.parse(config.binary);
  const journalDirectory = absolute.parse(config.journalDirectory);
  const executionRoot = process.platform === "linux" ? absolute.optional().parse(config.executionRoot) : undefined;
  const allowExecution = config.allowExecution && (process.platform === "win32" || executionRoot !== undefined);
  const env = {
    SystemRoot: process.env.SystemRoot, TEMP: process.env.TEMP, TMP: process.env.TMP,
    INTAKE_EXECUTION_ROOT: executionRoot,
    INTAKE_ALLOW_EXECUTION: executionRoot && allowExecution ? "1" : undefined,
    ...Object.fromEntries(engineSettings.map(key => [key, absolute.optional().parse(config.engines?.[key])])),
  };
  const root = (name: string) => {
    const path = roots.get(name);
    if (!path) throw new AdapterError(422, "Unknown configured root");
    return path;
  };
  const journalPath = (id: string) => join(journalDirectory, id + ".jsonl");
  return async (input: unknown, signal: AbortSignal) => {
    const parsed = shape.safeParse(input);
    if (!parsed.success) throw new AdapterError(400, "Invalid operation or arguments");
    const request = parsed.data;
    if ((request.operation === "apply" || request.operation === "recover") && !allowExecution) {
      throw new AdapterError(403, "Adapter execution is disabled");
    }
    const invoke = (args: string[]) => subprocess(binary, request.operation, args, signal, env);
    const withFiles = <T>(contents: string[], using: (paths: string[]) => Promise<T>) =>
      withRequestFiles(executionRoot ?? tmpdir(), contents, using);
    switch (request.operation) {
      case "diagnostics": {
        const result = await invoke([]);
        return {
          ...result,
          record: { ...result.record, root_aliases: [...roots.keys()].sort(), execution_enabled: Boolean(allowExecution) },
        };
      }
      case "scan":
      case "photo-evidence":
      case "video-inventory":
      case "duplicates": return invoke([root(request.root), ...request.files]);
      case "photo-suggest": return invoke([root(request.root), request.file, request.model]);
      case "transcribe": return invoke([root(request.root), request.file, request.language]);
      case "speech-preview": return invoke([root(request.root), request.file, request.voice]);
      case "audio-plan":
        return invoke([root(request.source), request.file, root(request.destination), request.output, String(request.track), request.quality]);
      case "verify-package":
        return invoke([journalPath(request.journal), root(request.source), root(request.destination)]);
      case "library-import-plan": return invoke([root(request.source), root(request.destination)]);
      case "library-listening-plan":
        return invoke([root(request.source), root(request.destination), ...request.chapters.map(String)]);
      case "library-text-plan":
      case "library-transcript-plan": return invoke([root(request.book), root(request.source), root(request.destination)]);
      case "library-text-search":
      case "library-transcript-search": return invoke([root(request.book), root(request.catalog), request.query]);
      case "align-notes": return withFiles([request.notes, JSON.stringify(request.transcript)], invoke);
      case "video-archive-plan":
        return withFiles([JSON.stringify(request.inventory)], ([inventory]) =>
          invoke([root(request.source), root(request.destination), inventory, ...request.files]));
      case "clip-plan": {
        const args = [root(request.source), request.file, root(request.destination)];
        const selected = request.transcript;
        if (!selected) {
          return withFiles([JSON.stringify({ clips: request.clips })], paths => invoke([...args, ...paths]));
        }
        return withFiles([JSON.stringify(selected.report)], ([report]) => withFiles([
          JSON.stringify({
            clips: request.clips,
            transcript: { report, extraction_journal: journalPath(selected.extraction_journal), action_id: selected.action_id },
          }),
        ], paths => invoke([...args, ...paths])));
      }
      case "plan": return invoke([root(request.source), root(request.destination), request.directory, ...request.files]);
      case "receipt": {
        const file = journalPath(request.journal);
        const exists = await lstat(file).then(() => true, (error: NodeJS.ErrnoException) => {
          if (error.code === "ENOENT") return false;
          throw error;
        });
        if (exists) return invoke([file]);
        if (!request.allow_pending) throw new AdapterError(404, "Journal does not exist");
        return { protocol_version: version, record: { status: "pending" }, complete: false };
      }
      case "recover": return invoke([journalPath(request.journal), root(request.source), root(request.destination)]);
      case "undo-plan": return invoke([journalPath(request.journal), root(request.root), ...request.ids.map(String)]);
      case "organize":
      case "caption-plan":
      case "narration-delivery-plan":
      case "verify":
      case "apply": {
        const values = "plan" in request ? request.plan : "receipt" in request ? request.receipt : request.decisions;
        return withFiles([JSON.stringify(values)], ([file]) => {
          if (request.operation === "organize" || request.operation === "narration-delivery-plan") {
            return invoke([root(request.source), root(request.destination), file]);
          }
          if (request.operation === "caption-plan") return invoke([root(request.root), file]);
          const args = [file, root(request.source), root(request.destination)];
          if (request.operation === "apply") args.push(journalPath(request.journal));
          return invoke([...args, ...request.ids.map(String)]);
        });
      }
    }
  };
}

type Dispatch = ReturnType<typeof createAdapter>;

function failure(error: unknown) {
  return error instanceof AdapterError ? error : new AdapterError(500, "Adapter operation failed");
}

export function serveHttp(dispatch: Dispatch, token: string, port: number) {
  const expected = Buffer.from("Bearer " + z.string().regex(/^[A-Za-z0-9_-]{32,256}$/).parse(token));
  return Bun.serve({
    hostname: "127.0.0.1",
    port,
    maxRequestBodySize: bodyLimit,
    idleTimeout: 135,
    async fetch(request) {
      const presented = Buffer.from(request.headers.get("Authorization") ?? "");
      if (presented.length !== expected.length || !timingSafeEqual(expected, presented)) {
        return Response.json({ error: "Authentication required" }, { status: 401, headers: { "WWW-Authenticate": "Bearer" } });
      }
      if (new URL(request.url).pathname !== "/v1/operations") return new Response(null, { status: 404 });
      if (request.method !== "POST") return new Response(null, { status: 405, headers: { Allow: "POST" } });
      if (request.headers.get("content-type")?.split(";")[0]?.trim() !== "application/json" || !request.body) {
        return Response.json({ error: "Expected a JSON request body" }, { status: 415 });
      }
      try {
        const content = await readBounded(request.body, bodyLimit);
        let command: unknown;
        try {
          command = JSON.parse(content);
        } catch {
          throw new AdapterError(400, "Malformed JSON");
        }
        return Response.json(await dispatch(command, request.signal), { headers: { "Cache-Control": "no-store" } });
      } catch (error) {
        const problem = failure(error);
        return Response.json({ error: problem.message }, { status: problem.status });
      }
    },
  });
}

async function serveMcp(dispatch: Dispatch, allowExecution: boolean) {
  const server = new McpServer({ name: "intake-agent", version: "0.2.0" });
  const invoke = async ({ request }: { request: unknown }, { signal }: { signal: AbortSignal }) => {
    try {
      const response = await dispatch(request, signal);
      return { structuredContent: response, content: [{ type: "text" as const, text: JSON.stringify(response) }] };
    } catch (error) {
      return { isError: true, content: [{ type: "text" as const, text: failure(error).message }] };
    }
  };
  server.registerTool(
    "intake_read",
    {
      description: "Inspect selected media and library assets, propose and verify plans, search cited records or inspect receipts using configured root aliases.",
      inputSchema: { request: readShape },
      annotations: { readOnlyHint: true, openWorldHint: false },
    },
    invoke,
  );
  if (allowExecution) {
    server.registerTool("intake_execute", {
      description: "Review selected actions in the operator's approval console or foreground terminal before applying or recovering a journal. After interruption, inspect the journal with intake_read before retrying.",
      inputSchema: { request: executionShape },
      annotations: { readOnlyHint: false, destructiveHint: true, openWorldHint: false },
    }, invoke);
  }
  await server.connect(new StdioServerTransport());
}

if (import.meta.main) {
  try {
    const mode = z.enum(["mcp", "http"]).parse(process.argv[2]);
    const binary = process.env.INTAKE_BINARY ?? "";
    const journalDirectory = process.env.INTAKE_JOURNALS ?? "";
    if (!(await stat(journalDirectory)).isDirectory()) throw new Error("Expected an existing journal directory");
    const executionRoot = process.platform === "linux" ? process.env.INTAKE_EXECUTION_ROOT : undefined;
    const allowExecution = process.env.INTAKE_ALLOW_EXECUTION === "1" && (process.platform === "win32" || Boolean(executionRoot));
    const dispatch = createAdapter({
      binary, journalDirectory, allowExecution, executionRoot,
      roots: JSON.parse(process.env.INTAKE_ROOTS ?? "{}"), engines: process.env,
    });
    if (mode === "mcp") {
      await serveMcp(dispatch, allowExecution);
    } else {
      const port = z.coerce.number().int().min(1).max(65535).parse(process.env.INTAKE_PORT ?? "8787");
      const service = serveHttp(dispatch, process.env.INTAKE_HTTP_TOKEN ?? "", port);
      console.error("intake-agent HTTP listening on " + service.url);
    }
  } catch {
    console.error("Could not start intake-agent adapter. Check the mode, binary, configured roots, journals and HTTP settings.");
    process.exitCode = 1;
  }
}
