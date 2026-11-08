/**
 * Lightweight built-in replacement for `@earendil-works/pi-coding-agent`'s SDK (`SettingsManager`, `ModelRuntime`,
 * `SessionManager`, and `resolveModelScopeWithDiagnostics`) backed by the native Rust `oopbot` reference engine in
 * Termux (`oopbot --mode catalog` / `oopbot --mode scan-models`), so `host.mjs` needs no `npm ci` or
 * `node_modules/@earendil-works/pi-coding-agent` on disk.
 */
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { mkdir, readdir, readFile, stat, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import type { PiSdk } from "./env.js";
import { isObject, type JsonObject } from "./framing.js";

interface BuiltinProvider {
  id: string;
  name: string;
  baseUrl: string;
  api: string;
  models: JsonObject[];
}

/**
 * Mirror of `builtin_providers()` in `src/models.rs`, used when the native `oopbot` binary is not yet installed
 * (e.g. during first-time setup or host unit tests). When `oopbot` is present on disk, `getReferenceCatalogue()`
 * queries `oopbot --mode catalog` directly so the Rust engine is the single source of truth.
 */
const FALLBACK_CATALOGUE: readonly BuiltinProvider[] = [
  {
    id: "google",
    name: "Google Gemini (OOPBoT)",
    baseUrl: "https://generativelanguage.googleapis.com/v1beta",
    api: "google-generative-ai",
    models: [
      {
        id: "gemini-3.5-flash-lite",
        name: "Gemini 3.5 Flash Lite",
        provider: "google",
        api: "google-generative-ai",
        contextWindow: 1_048_576,
        maxTokens: 65_536,
        reasoning: true,
        input: ["text", "image"],
        cost: { input: 0.1, output: 0.4, cacheRead: 0.025, cacheWrite: 0.1 },
      },
      {
        id: "gemini-2.5-flash",
        name: "Gemini 2.5 Flash",
        provider: "google",
        api: "google-generative-ai",
        contextWindow: 1_048_576,
        maxTokens: 65_536,
        reasoning: true,
        input: ["text", "image"],
        cost: { input: 0.3, output: 2.5, cacheRead: 0.075, cacheWrite: 0.3 },
      },
      {
        id: "gemini-2.5-pro",
        name: "Gemini 2.5 Pro",
        provider: "google",
        api: "google-generative-ai",
        contextWindow: 1_048_576,
        maxTokens: 65_536,
        reasoning: true,
        input: ["text", "image"],
        cost: { input: 1.25, output: 10.0, cacheRead: 0.31, cacheWrite: 1.25 },
      },
      {
        id: "gemini-2.5-flash-lite",
        name: "Gemini 2.5 Flash-Lite",
        provider: "google",
        api: "google-generative-ai",
        contextWindow: 1_048_576,
        maxTokens: 65_536,
        reasoning: true,
        input: ["text", "image"],
        cost: { input: 0.1, output: 0.4, cacheRead: 0.025, cacheWrite: 0.1 },
      },
      {
        id: "gemma-4-31b-it",
        name: "Gemma 4 31B IT",
        provider: "google",
        api: "google-generative-ai",
        contextWindow: 131_072,
        maxTokens: 8_192,
        reasoning: true,
        input: ["text", "image"],
      },
    ],
  },
  {
    id: "openrouter",
    name: "OpenRouter",
    baseUrl: "https://openrouter.ai/api/v1",
    api: "openai-completions",
    models: [
      {
        id: "google/gemini-2.5-flash",
        name: "Gemini 2.5 Flash",
        provider: "openrouter",
        api: "openai-completions",
        contextWindow: 1_048_576,
        maxTokens: 65_536,
        reasoning: true,
        input: ["text", "image"],
      },
      {
        id: "google/gemini-2.5-pro",
        name: "Gemini 2.5 Pro",
        provider: "openrouter",
        api: "openai-completions",
        contextWindow: 1_048_576,
        maxTokens: 65_536,
        reasoning: true,
        input: ["text", "image"],
      },
      {
        id: "anthropic/claude-sonnet-4",
        name: "Claude Sonnet 4",
        provider: "openrouter",
        api: "openai-completions",
        contextWindow: 200_000,
        maxTokens: 64_000,
        reasoning: true,
        input: ["text", "image"],
      },
      {
        id: "deepseek/deepseek-r1",
        name: "DeepSeek R1",
        provider: "openrouter",
        api: "openai-completions",
        contextWindow: 128_000,
        maxTokens: 32_768,
        reasoning: true,
        input: ["text"],
      },
    ],
  },
  {
    id: "zai",
    name: "Z.ai Coding Plan",
    baseUrl: "https://api.z.ai/api/coding/paas/v4",
    api: "openai-completions",
    models: [
      {
        id: "glm-4.5",
        name: "GLM-4.5",
        provider: "zai",
        api: "openai-completions",
        contextWindow: 128_000,
        maxTokens: 16_384,
        reasoning: true,
        input: ["text", "image"],
      },
      {
        id: "glm-4.5-air",
        name: "GLM-4.5 Air",
        provider: "zai",
        api: "openai-completions",
        contextWindow: 128_000,
        maxTokens: 16_384,
        reasoning: true,
        input: ["text"],
      },
    ],
  },
  {
    id: "kimi-coding",
    name: "Kimi Code",
    baseUrl: "https://api.kimi.com/coding/v1",
    api: "openai-completions",
    models: [
      {
        id: "kimi-k2-0711-preview",
        name: "Kimi K2",
        provider: "kimi-coding",
        api: "openai-completions",
        contextWindow: 131_072,
        maxTokens: 32_768,
        reasoning: true,
        input: ["text", "image"],
      },
      {
        id: "kimi-thinking-preview",
        name: "Kimi Thinking",
        provider: "kimi-coding",
        api: "openai-completions",
        contextWindow: 131_072,
        maxTokens: 32_768,
        reasoning: true,
        input: ["text", "image"],
      },
    ],
  },
  {
    id: "anthropic",
    name: "Anthropic",
    baseUrl: "https://api.anthropic.com/v1",
    api: "anthropic-messages",
    models: [
      {
        id: "claude-sonnet-4-20250514",
        name: "Claude Sonnet 4",
        provider: "anthropic",
        api: "anthropic-messages",
        contextWindow: 200_000,
        maxTokens: 64_000,
        reasoning: true,
        input: ["text", "image"],
        cost: { input: 3.0, output: 15.0, cacheRead: 0.3, cacheWrite: 3.75 },
      },
      {
        id: "claude-opus-4-20250514",
        name: "Claude Opus 4",
        provider: "anthropic",
        api: "anthropic-messages",
        contextWindow: 200_000,
        maxTokens: 32_000,
        reasoning: true,
        input: ["text", "image"],
        cost: { input: 15.0, output: 75.0, cacheRead: 1.5, cacheWrite: 18.75 },
      },
      {
        id: "claude-3-5-haiku-latest",
        name: "Claude 3.5 Haiku",
        provider: "anthropic",
        api: "anthropic-messages",
        contextWindow: 200_000,
        maxTokens: 8_192,
        reasoning: false,
        input: ["text", "image"],
        cost: { input: 0.8, output: 4.0, cacheRead: 0.08, cacheWrite: 1.0 },
      },
    ],
  },
  {
    id: "openai-codex",
    name: "OpenAI",
    baseUrl: "https://api.openai.com/v1",
    api: "openai-responses",
    models: [
      {
        id: "gpt-5",
        name: "GPT-5",
        provider: "openai-codex",
        api: "openai-responses",
        contextWindow: 400_000,
        maxTokens: 128_000,
        reasoning: true,
        input: ["text", "image"],
      },
      {
        id: "gpt-5-mini",
        name: "GPT-5 Mini",
        provider: "openai-codex",
        api: "openai-responses",
        contextWindow: 400_000,
        maxTokens: 128_000,
        reasoning: true,
        input: ["text", "image"],
      },
      {
        id: "o4-mini",
        name: "o4-mini",
        provider: "openai-codex",
        api: "openai-responses",
        contextWindow: 200_000,
        maxTokens: 100_000,
        reasoning: true,
        input: ["text", "image"],
      },
    ],
  },
];

let cachedRustCatalogue: readonly BuiltinProvider[] | null = null;

function resolveOopbotBinary(): string | null {
  const candidates = [
    process.env.OOPBOT_BIN,
    join(homedir(), ".pipkin", "bin", "oopbot"),
    process.env.PREFIX ? join(process.env.PREFIX, "bin", "oopbot") : undefined,
    "/data/data/com.termux/files/usr/bin/oopbot",
  ];
  for (const candidate of candidates) {
    if (typeof candidate === "string" && candidate.length > 0 && existsSync(candidate)) {
      return candidate;
    }
  }
  return null;
}

/**
 * Queries `oopbot --mode catalog` from the Rust engine if available on disk, falling back to `FALLBACK_CATALOGUE`.
 */
function getReferenceCatalogue(): readonly BuiltinProvider[] {
  if (cachedRustCatalogue !== null) return cachedRustCatalogue;
  const bin = resolveOopbotBinary();
  if (bin !== null) {
    try {
      const stdout = execFileSync(bin, ["--mode", "catalog"], {
        encoding: "utf8",
        timeout: 3_000,
        stdio: ["ignore", "pipe", "ignore"],
      });
      const parsed: unknown = JSON.parse(stdout);
      if (isObject(parsed) && Array.isArray(parsed.providers)) {
        const providers: BuiltinProvider[] = [];
        for (const p of parsed.providers) {
          if (!isObject(p) || typeof p.id !== "string") continue;
          const models = Array.isArray(p.models) ? p.models.filter((m): m is JsonObject => isObject(m)) : [];
          providers.push({
            id: p.id,
            name: typeof p.name === "string" ? p.name : p.id,
            baseUrl: typeof p.baseUrl === "string" ? p.baseUrl : "",
            api: typeof p.api === "string" ? p.api : "openai-completions",
            models,
          });
        }
        if (providers.length > 0) {
          cachedRustCatalogue = providers;
          return cachedRustCatalogue;
        }
      }
    } catch {
      // Fallback to built-in mirror if binary execution fails.
    }
  }
  cachedRustCatalogue = FALLBACK_CATALOGUE;
  return cachedRustCatalogue;
}

class OopbotSettingsManager {
  globalSettings: JsonObject = {};
  private readonly settingsPath: string;
  private readonly errors: { scope: string; error: Error }[] = [];
  private dirty = false;

  constructor(_home: string, configDir: string) {
    this.settingsPath = join(configDir, "settings.json");
    try {
      const raw = readFileSync(this.settingsPath, "utf8").replace(/^\uFEFF/, "");
      const parsed: unknown = JSON.parse(raw);
      if (isObject(parsed)) {
        this.globalSettings = parsed;
      } else {
        this.errors.push({ scope: "global", error: new Error("settings.json is not a JSON object") });
      }
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
        this.errors.push({ scope: "global", error: error instanceof Error ? error : new Error(String(error)) });
      }
    }
  }

  static create(home: string, configDir: string): OopbotSettingsManager {
    return new OopbotSettingsManager(home, configDir);
  }

  drainErrors(): { scope: string; error: Error }[] {
    const out = [...this.errors];
    this.errors.length = 0;
    return out;
  }

  getGlobalSettings(): JsonObject {
    return this.globalSettings;
  }

  getEnabledModels(): unknown {
    return this.globalSettings.enabledModels;
  }

  setEnabledModels(patterns: string[] | undefined): void {
    if (patterns === undefined) delete this.globalSettings.enabledModels;
    else this.globalSettings.enabledModels = patterns;
    this.dirty = true;
  }

  getDefaultProvider(): string | undefined {
    return typeof this.globalSettings.defaultProvider === "string" ? this.globalSettings.defaultProvider : undefined;
  }

  getDefaultModel(): string | undefined {
    return typeof this.globalSettings.defaultModel === "string" ? this.globalSettings.defaultModel : undefined;
  }

  setDefaultModelAndProvider(provider: string, modelId: string): void {
    this.globalSettings.defaultProvider = provider;
    this.globalSettings.defaultModel = modelId;
    this.dirty = true;
  }

  getDefaultThinkingLevel(): string | undefined {
    return typeof this.globalSettings.defaultThinkingLevel === "string"
      ? this.globalSettings.defaultThinkingLevel
      : undefined;
  }

  setDefaultThinkingLevel(level: string): void {
    this.globalSettings.defaultThinkingLevel = level;
    this.dirty = true;
  }

  setSteeringMode(mode: string): void {
    this.globalSettings.steeringMode = mode;
    this.dirty = true;
  }

  getOrCreateDeviceId(): string {
    if (typeof this.globalSettings.deviceId === "string" && this.globalSettings.deviceId.length > 0) {
      return this.globalSettings.deviceId;
    }
    const id = randomUUID();
    this.globalSettings.deviceId = id;
    this.dirty = true;
    return id;
  }

  markModified(_field: string, _nestedKey?: string): void {
    this.dirty = true;
  }

  save(): void {
    this.dirty = true;
  }

  async flush(): Promise<void> {
    if (!this.dirty) return;
    try {
      await mkdir(dirname(this.settingsPath), { recursive: true });
      await writeFile(this.settingsPath, `${JSON.stringify(this.globalSettings, null, 2)}\n`);
      this.dirty = false;
    } catch (error) {
      this.errors.push({ scope: "global", error: error instanceof Error ? error : new Error(String(error)) });
    }
  }
}

class OopbotModelRuntime {
  private readonly configDir: string;
  private readonly builtinOnly: boolean;
  private authDoc: JsonObject = {};
  private modelsDoc: JsonObject = {};
  private loadError: string | undefined;

  private constructor(configDir: string, builtinOnly: boolean) {
    this.configDir = configDir;
    this.builtinOnly = builtinOnly;
  }

  static async create(opts?: { modelsPath?: string | null }): Promise<OopbotModelRuntime> {
    const configDir = process.env.PI_CODING_AGENT_DIR ?? "";
    const builtinOnly = opts?.modelsPath === null;
    const rt = new OopbotModelRuntime(configDir, builtinOnly);
    await rt.load();
    return rt;
  }

  private async load(): Promise<void> {
    try {
      const authText = await readFile(join(this.configDir, "auth.json"), "utf8");
      const parsed: unknown = JSON.parse(authText);
      if (isObject(parsed)) this.authDoc = parsed;
    } catch {
      this.authDoc = {};
    }
    if (this.builtinOnly) return;
    try {
      const modelsText = await readFile(join(this.configDir, "models.json"), "utf8");
      const parsed: unknown = JSON.parse(modelsText);
      if (isObject(parsed) && isObject(parsed.providers)) {
        this.modelsDoc = parsed.providers;
      }
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
        this.loadError = error instanceof Error ? error.message : String(error);
      }
    }
  }

  getError(): string | undefined {
    return this.loadError;
  }

  getProviders(): { id: string; name: string; baseUrl: string }[] {
    const catalogue = getReferenceCatalogue();
    const list = catalogue.map((p) => {
      const override = isObject(this.modelsDoc[p.id]) ? (this.modelsDoc[p.id] as JsonObject) : undefined;
      const baseUrl = typeof override?.baseUrl === "string" ? override.baseUrl : p.baseUrl;
      return { id: p.id, name: p.name, baseUrl };
    });
    if (!this.builtinOnly) {
      for (const [id, entry] of Object.entries(this.modelsDoc)) {
        if (!isObject(entry) || catalogue.some((b) => b.id === id)) continue;
        list.push({
          id,
          name: id,
          baseUrl: typeof entry.baseUrl === "string" ? entry.baseUrl : "",
        });
      }
    }
    return list;
  }

  getProvider(id: string): { id: string; name: string; baseUrl: string } | undefined {
    return this.getProviders().find((p) => p.id === id);
  }

  getModels(providerId?: string): JsonObject[] {
    const catalogue = getReferenceCatalogue();
    const providers = providerId === undefined ? this.getProviders().map((p) => p.id) : [providerId];
    const result: JsonObject[] = [];
    for (const pid of providers) {
      const builtin = catalogue.find((b) => b.id === pid);
      const entry = !this.builtinOnly && isObject(this.modelsDoc[pid]) ? (this.modelsDoc[pid] as JsonObject) : undefined;
      const overrides = isObject(entry?.modelOverrides) ? (entry.modelOverrides as JsonObject) : {};
      const extraList = Array.isArray(entry?.models) ? entry.models : [];
      const byId = new Map<string, JsonObject>();

      if (builtin !== undefined) {
        for (const baseModel of builtin.models) {
          const mid = String(baseModel.id);
          const ov = isObject(overrides[mid]) ? (overrides[mid] as JsonObject) : {};
          byId.set(mid, { ...baseModel, ...ov, provider: pid, id: mid });
        }
      }
      for (const extra of extraList) {
        if (typeof extra === "string") {
          const ov = isObject(overrides[extra]) ? (overrides[extra] as JsonObject) : {};
          const prev = byId.get(extra);
          byId.set(extra, {
            id: extra,
            name: extra,
            provider: pid,
            api: builtin?.api ?? "openai-completions",
            ...prev,
            ...ov,
          });
        } else if (isObject(extra) && typeof extra.id === "string") {
          const mid = extra.id;
          const ov = isObject(overrides[mid]) ? (overrides[mid] as JsonObject) : {};
          const prev = byId.get(mid);
          byId.set(mid, {
            provider: pid,
            api: builtin?.api ?? "openai-completions",
            ...prev,
            ...extra,
            ...ov,
            id: mid,
          });
        }
      }
      for (const modelObj of byId.values()) {
        result.push(modelObj);
      }
    }
    return result;
  }

  async checkAuth(id: string): Promise<{ type: "api_key" | "oauth" } | undefined> {
    const auth = await this.getAuth(id);
    if (auth?.auth.apiKey) return { type: "api_key" };
    return undefined;
  }

  async getAuth(id: string): Promise<{ auth: { apiKey?: string } } | undefined> {
    await this.load();
    const entry = this.authDoc[id];
    if (typeof entry === "string" && entry.length > 0) {
      return { auth: { apiKey: entry } };
    }
    if (isObject(entry)) {
      if (typeof entry.apiKey === "string" && entry.apiKey.length > 0) {
        return { auth: { apiKey: entry.apiKey } };
      }
      if (isObject(entry.auth) && typeof entry.auth.apiKey === "string" && entry.auth.apiKey.length > 0) {
        return { auth: { apiKey: entry.auth.apiKey } };
      }
    }
    const modelEntry = this.modelsDoc[id];
    if (isObject(modelEntry) && typeof modelEntry.apiKey === "string" && modelEntry.apiKey.length > 0) {
      return { auth: { apiKey: modelEntry.apiKey } };
    }
    return undefined;
  }

  async getAvailable(): Promise<{ provider: string; id: string; name?: string }[]> {
    await this.load();
    const out: { provider: string; id: string; name?: string }[] = [];
    for (const provider of this.getProviders()) {
      const hasAuth = (await this.checkAuth(provider.id)) !== undefined;
      if (!hasAuth) continue;
      for (const model of this.getModels(provider.id)) {
        const id = String(model.id);
        const name = typeof model.name === "string" ? model.name : undefined;
        out.push(name === undefined ? { provider: provider.id, id } : { provider: provider.id, id, name });
      }
    }
    return out;
  }

  async login(
    providerId: string,
    _type: "oauth" | "api_key",
    interaction: {
      prompt: (p: { type: "input"; message: string; placeholder?: string }) => Promise<string>;
    },
  ): Promise<void> {
    const key = await interaction.prompt({
      type: "input",
      message: `Enter your ${providerId} API key:`,
      placeholder: "API key",
    });
    const trimmed = key.trim();
    if (!trimmed) throw new Error("API key cannot be empty");
    await this.load();
    this.authDoc[providerId] = { type: "api_key", apiKey: trimmed };
    await mkdir(this.configDir, { recursive: true });
    await writeFile(join(this.configDir, "auth.json"), `${JSON.stringify(this.authDoc, null, 2)}\n`);

    if (providerId === "google") {
      await this.scanAndSaveGeminiModels(trimmed);
    }
  }

  private async scanAndSaveGeminiModels(apiKey: string): Promise<void> {
    try {
      const baseUrl = this.getProvider("google")?.baseUrl ?? "https://generativelanguage.googleapis.com/v1beta";
      const response = await fetch(`${baseUrl.replace(/\/+$/, "")}/models?pageSize=100`, {
        headers: { "x-goog-api-key": apiKey },
        signal: AbortSignal.timeout(10_000),
      });
      if (!response.ok) return;
      const body: unknown = await response.json();
      if (!isObject(body) || !Array.isArray(body.models)) return;
      const scanned: JsonObject[] = [];
      for (const item of body.models) {
        if (!isObject(item) || typeof item.name !== "string") continue;
        const methods = Array.isArray(item.supportedGenerationMethods) ? item.supportedGenerationMethods : [];
        if (methods.length > 0 && !methods.includes("generateContent")) continue;
        const id = item.name.replace(/^models\//, "");
        if (id.includes("embedding") || id.includes("aqa") || id.includes("imagen")) continue;
        scanned.push({
          id,
          name: typeof item.displayName === "string" ? item.displayName : id,
          contextWindow: typeof item.inputTokenLimit === "number" ? item.inputTokenLimit : 1_048_576,
          maxTokens: typeof item.outputTokenLimit === "number" ? item.outputTokenLimit : 65_536,
          reasoning: typeof item.thinking === "boolean" ? item.thinking : !id.includes("gemini-1.5"),
          input: ["text", "image"],
        });
      }
      if (scanned.length === 0) return;
      const existing = isObject(this.modelsDoc.google) ? (this.modelsDoc.google as JsonObject) : {};
      this.modelsDoc.google = { ...existing, models: scanned };
      await writeFile(join(this.configDir, "models.json"), `${JSON.stringify({ providers: this.modelsDoc }, null, 2)}\n`);
    } catch {
      // Best-effort auto-scan; built-in Gemini catalog remains available if offline.
    }
  }

  async logout(providerId: string): Promise<void> {
    await this.load();
    delete this.authDoc[providerId];
    await mkdir(this.configDir, { recursive: true });
    await writeFile(join(this.configDir, "auth.json"), `${JSON.stringify(this.authDoc, null, 2)}\n`);
  }
}

interface ParsedSession {
  path: string;
  id: string;
  cwd: string;
  name?: string;
  created: Date;
  modified: Date;
  messageCount: number;
  firstMessage: string;
  messages: JsonObject[];
}

function parseSessionContent(path: string, content: string, mtime: Date): ParsedSession | null {
  const lines = content.split("\n").filter((l) => l.trim().length > 0);
  if (lines.length === 0) return null;
  let header: JsonObject | undefined;
  try {
    const first: unknown = JSON.parse(lines[0] ?? "");
    if (isObject(first) && first.type === "session" && typeof first.id === "string" && typeof first.cwd === "string") {
      // Subagent transcripts carry `parentSession` and stay out of the top-level session list.
      if (typeof first.parentSession === "string") return null;
      header = first;
    }
  } catch {
    return null;
  }
  if (header === undefined) return null;

  const created = typeof header.timestamp === "string" ? new Date(header.timestamp) : mtime;
  let name: string | undefined;
  let firstMessage = "";
  const messages: JsonObject[] = [];

  for (let i = 1; i < lines.length; i++) {
    try {
      const entry: unknown = JSON.parse(lines[i] ?? "");
      if (!isObject(entry)) continue;
      if (entry.type === "session_info" && typeof entry.name === "string") {
        name = entry.name;
      } else if (entry.type === "message" && isObject(entry.message)) {
        const msg = entry.message;
        messages.push(msg);
        if (firstMessage === "" && msg.role === "user") {
          if (typeof msg.content === "string") {
            firstMessage = msg.content;
          } else if (Array.isArray(msg.content)) {
            firstMessage = msg.content
              .map((part) => (isObject(part) && typeof part.text === "string" ? part.text : ""))
              .join("");
          }
        }
      }
    } catch {
      // Ignore malformed lines
    }
  }

  return {
    path,
    id: String(header.id),
    cwd: String(header.cwd),
    ...(name === undefined ? {} : { name }),
    created,
    modified: mtime,
    messageCount: messages.length,
    firstMessage,
    messages,
  };
}

const OopbotSessionManager = {
  async listAll(dir?: string): Promise<ParsedSession[]> {
    const baseDir = dir ?? join(process.env.PI_CODING_AGENT_DIR ?? "", "sessions");
    let folders: string[];
    try {
      folders = await readdir(baseDir);
    } catch {
      return [];
    }
    const results: ParsedSession[] = [];
    for (const folder of folders) {
      const folderPath = join(baseDir, folder);
      let files: string[];
      try {
        files = await readdir(folderPath);
      } catch {
        continue;
      }
      for (const file of files) {
        if (!file.endsWith(".jsonl")) continue;
        const fullPath = join(folderPath, file);
        try {
          const [st, content] = await Promise.all([stat(fullPath), readFile(fullPath, "utf8")]);
          const parsed = parseSessionContent(fullPath, content, st.mtime);
          if (parsed !== null) results.push(parsed);
        } catch {
          // Ignore unreadable session files
        }
      }
    }
    return results;
  },

  open(path: string): { buildSessionContext(): { messages: JsonObject[] } } {
    let messages: JsonObject[] = [];
    try {
      const content = readFileSync(path, "utf8");
      const parsed = parseSessionContent(path, content, new Date());
      if (parsed !== null) messages = parsed.messages;
    } catch {
      messages = [];
    }
    return {
      buildSessionContext: () => ({ messages }),
    };
  },
};

function globMatch(pattern: string, text: string): boolean {
  const escaped = pattern
    .toLowerCase()
    .replaceAll(/[.+^${}()|[\]\\]/g, "\\$&")
    .replaceAll("*", ".*")
    .replaceAll("?", ".");
  return new RegExp(`^${escaped}$`).test(text.toLowerCase());
}

async function resolveModelScopeWithDiagnostics(
  patterns: string[],
  source: { getAvailable: () => Promise<{ provider: string; id: string; name?: string }[]> },
): Promise<{
  scopedModels: { model: { provider: string; id: string; name?: string }; thinkingLevel?: string }[];
}> {
  const available = await source.getAvailable();
  const scopedModels: { model: { provider: string; id: string; name?: string }; thinkingLevel?: string }[] = [];
  const seen = new Set<string>();

  for (const rawPattern of patterns) {
    const colonIdx = rawPattern.lastIndexOf(":");
    const hasLevel =
      colonIdx > 0 && ["off", "minimal", "low", "medium", "high", "xhigh"].includes(rawPattern.slice(colonIdx + 1));
    const core = hasLevel ? rawPattern.slice(0, colonIdx) : rawPattern;
    const thinkingLevel = hasLevel ? rawPattern.slice(colonIdx + 1) : undefined;

    for (const model of available) {
      const fullRef = `${model.provider}/${model.id}`;
      const matched = core.includes("/")
        ? globMatch(core, fullRef)
        : globMatch(core, model.id) || (model.name !== undefined && globMatch(core, model.name));
      if (matched) {
        const key = `${model.provider}/${model.id}`;
        if (!seen.has(key)) {
          seen.add(key);
          scopedModels.push(thinkingLevel === undefined ? { model } : { model, thinkingLevel });
        }
      }
    }
  }
  return { scopedModels };
}

export const oopbotSdk: PiSdk = {
  SettingsManager: OopbotSettingsManager,
  ModelRuntime: OopbotModelRuntime,
  SessionManager: OopbotSessionManager,
  resolveModelScopeWithDiagnostics,
} as unknown as PiSdk;
