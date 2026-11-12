import { spawn } from "node:child_process";
import { chmod, copyFile, mkdir, readFile, rename, rm, stat, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import type { Readable } from "node:stream";
import {
  HostError,
  installedPiVersion,
  OOPBOT_RELEASE_URL,
  oopbotBinaryPath,
  piInstallComplete,
  piInstallingMarker,
  readOopbotBinaryVersion,
  resolveOopbotBinary,
} from "../env.js";
import type { JsonObject } from "../framing.js";
import { type InstallLock, withInstallLock } from "../install-lock.js";
import type { ModeContext } from "../mode.js";
import { stringParam } from "../params.js";
import { installPipkinExtensions, pipkinExtensionsParam } from "../pipkin-extensions.js";
import { refreshSshConfig } from "../remote/hosts.js";
import { remotePaths } from "../remote/paths.js";

/** Longest progress line forwarded; npm never needs more, and the app shows only a few lines. */
const MAX_PROGRESS_LINE = 4096;

/** npm's report that it couldn't reach the registry: offline, DNS down, or a dropped connection. */
const NETWORK_ERROR = /^npm error (network|code E(AI_AGAIN|NOTFOUND|CONNREFUSED|CONNRESET|TIMEDOUT|NETUNREACH))\b/;

export function isNpmNetworkError(line: string): boolean {
  return NETWORK_ERROR.test(line);
}

/** Sends each line of [stream] to [send], cut to `MAX_PROGRESS_LINE`. */
export function forwardLines(stream: Readable, send: (line: string) => void): void {
  let carry = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk: string) => {
    const lines = (carry + chunk).split("\n");
    carry = (lines.pop() ?? "").slice(-MAX_PROGRESS_LINE);
    for (const line of lines) send(line.replace(/\r$/, "").slice(0, MAX_PROGRESS_LINE));
  });
  stream.on("end", () => {
    if (carry.length > 0) send(carry);
  });
}

/** `<command> failed` for a [command] that exited with [exit], or that a signal killed when it's null; under [code]. */
export function commandFailed(code: string, command: string, exit: number | null): HostError {
  if (exit === null) return new HostError(code, `${command} failed (exit signal)`, { key: "command_killed", args: { command } });
  return new HostError(code, `${command} failed (exit ${exit})`, { key: "command_failed", args: { command, exit } });
}

export async function npmCi(ctx: ModeContext, cwd: string, lock: InstallLock): Promise<void> {
  let offline = false;
  const send = (line: string): void => {
    offline ||= isNpmNetworkError(line);
    ctx.conn.send({ type: "pipkin_progress", line });
  };
  // npm at --loglevel=error stays quiet until it's done; this line tells the app it's installing, not just checking.
  send("npm ci");
  const args = ["ci", "--ignore-scripts", "--no-audit", "--no-fund", "--no-update-notifier", "--loglevel=error"];
  const child = spawn("npm", args, {
    cwd,
    signal: ctx.signal,
    stdio: ["ignore", "pipe", "pipe"],
  });
  forwardLines(child.stdout, send);
  forwardLines(child.stderr, send);
  const { promise, resolve, reject } = Promise.withResolvers<number | null>();
  child.on("error", reject);
  // A cancelled npm's error comes before it has rolled back and exited; only `close` says it's gone.
  const closed = Promise.withResolvers<void>();
  child.on("close", (code: number | null) => {
    resolve(code);
    closed.resolve();
  });
  // An npm the lock doesn't name must not outlive it, so when recording fails npm is stopped and waited for.
  const recorded = child.pid === undefined ? undefined : lock.recordNpm(child.pid);
  const [exit, record] = await Promise.allSettled([
    promise,
    recorded?.catch(async (error: unknown) => {
      if (!child.killed) child.kill();
      await closed.promise;
      throw error;
    }),
  ]);
  if (record.status === "rejected") throw record.reason;
  if (exit.status === "rejected") throw exit.reason;
  const code = exit.value;
  if (code === 0) return;
  if (offline) {
    throw new HostError("npm_offline", "Couldn't download the agent. Check your internet connection, then retry.", {
      key: "agent_download_offline",
    });
  }
  throw commandFailed("npm_failed", "npm ci", code);
}

const OOPBOT_RELEASES_BASE = "https://github.com/Ahmadfauzi34/OOPBoT/releases";
const OOPBOT_RELEASE_ASSET = "oopbot-aarch64-linux-android";

export function oopbotReleaseUrls(expectedVersion: string): string[] {
  const trimmed = expectedVersion.trim();
  return trimmed.length > 0
    ? [`${OOPBOT_RELEASES_BASE}/download/v${trimmed}/${OOPBOT_RELEASE_ASSET}`, OOPBOT_RELEASE_URL]
    : [OOPBOT_RELEASE_URL];
}

/**
 * Installs or upgrades the native Rust `oopbot` binary in Termux (`$PIPKIN_ROOT/bin/oopbot` and
 * `$PREFIX/bin/oopbot`) instead of downloading `@earendil-works/pi-coding-agent` via `npm ci`.
 */
export async function installOopbotRuntime(
  ctx: ModeContext,
  expectedVersion: string,
  releaseUrls: readonly string[] = oopbotReleaseUrls(expectedVersion),
): Promise<string> {
  const send = (line: string): void => {
    ctx.conn.send({ type: "pipkin_progress", line });
  };
  send("Installing oopbot Rust engine...");
  const binPath = oopbotBinaryPath(ctx.env.root);
  const prefixBin = join(ctx.env.prefix, "bin", "oopbot");
  await mkdir(dirname(binPath), { recursive: true });
  const hasPrefixBin = await stat(prefixBin).then(
    (s) => s.isFile() && s.size > 0,
    () => false,
  );
  const prefixVersion = hasPrefixBin ? readOopbotBinaryVersion(prefixBin) : null;
  let installedVer: string | null = null;
  if (
    ctx.params.reinstall !== true &&
    prefixVersion !== null &&
    (expectedVersion === "" || prefixVersion === expectedVersion)
  ) {
    const tmpPath = `${binPath}.${process.pid}.tmp`;
    await copyFile(prefixBin, tmpPath);
    await chmod(tmpPath, 0o755);
    await rename(tmpPath, binPath);
    installedVer = prefixVersion;
  } else {
    let lastStatus: number | undefined;
    let mismatchMsg: string | undefined;
    let sawNetworkError = false;
    let downloadedBytes: Uint8Array | null = null;
    for (const url of releaseUrls) {
      send(`Downloading ${url}`);
      let response: Response;
      try {
        response = await fetch(url, { signal: ctx.signal });
      } catch (error) {
        if (ctx.signal.aborted) throw error;
        sawNetworkError = true;
        continue;
      }
      if (!response.ok) {
        lastStatus = response.status;
        continue;
      }
      const bytes = new Uint8Array(await response.arrayBuffer());
      if (bytes.byteLength === 0) {
        mismatchMsg = "Downloaded oopbot binary was empty";
        continue;
      }
      const tmpPath = `${binPath}.${process.pid}.tmp`;
      await writeFile(tmpPath, bytes);
      await chmod(tmpPath, 0o755);
      const candidateVer = readOopbotBinaryVersion(tmpPath);
      if (candidateVer === null) {
        await rm(tmpPath, { force: true });
        mismatchMsg = "Downloaded oopbot binary failed --version check";
        continue;
      }
      if (expectedVersion.length > 0 && candidateVer !== expectedVersion) {
        await rm(tmpPath, { force: true });
        mismatchMsg = `Downloaded oopbot ${candidateVer}, expected ${expectedVersion}`;
        continue;
      }
      await rename(tmpPath, binPath);
      installedVer = candidateVer;
      downloadedBytes = bytes;
      break;
    }
    if (installedVer === null) {
      if (sawNetworkError && lastStatus === undefined && mismatchMsg === undefined) {
        throw new HostError("npm_offline", "Couldn't download oopbot. Check your internet connection, then retry.", {
          key: "agent_download_offline",
        });
      }
      if (mismatchMsg !== undefined) {
        throw new HostError("upgrade_failed", mismatchMsg);
      }
      const status = lastStatus ?? 1;
      throw new HostError("npm_failed", `Failed to download oopbot (HTTP ${status})`, {
        key: "command_failed",
        args: { command: "download oopbot", exit: status },
      });
    }
    if (downloadedBytes !== null) {
      const prefixTmp = `${prefixBin}.${process.pid}.tmp`;
      await mkdir(dirname(prefixBin), { recursive: true }).catch(() => undefined);
      await writeFile(prefixTmp, downloadedBytes)
        .then(() => chmod(prefixTmp, 0o755))
        .then(() => rename(prefixTmp, prefixBin))
        .catch(() => rm(prefixTmp, { force: true }).catch(() => undefined));
    }
  }

  // Record the verified installed version for diagnostics; installedPiVersion() always verifies `--version` first.
  const versionFile = join(dirname(binPath), "oopbot.version");
  await writeFile(versionFile, `${installedVer}\n`);
  return installedVer;
}

/**
 * Installs the pinned runtime unless it's already installed in full, or always when `reinstall` is true, then
 * Pipkin's extensions.
 */
export async function ensure(ctx: ModeContext): Promise<JsonObject> {
  const packageJson = stringParam(ctx.params, "packageJson");
  const packageLock = stringParam(ctx.params, "packageLock");
  const expectedVersion = stringParam(ctx.params, "expectedVersion");
  const pipkinExtensions = pipkinExtensionsParam(ctx.params);
  const piDir = join(ctx.env.root, "pi");
  await mkdir(piDir, { recursive: true });
  await mkdir(ctx.env.configDir, { recursive: true });
  const isTermux = ctx.env.prefix.includes("com.termux");

  await withInstallLock(ctx.env.root, ctx.signal, async (lock) => {
    const packageJsonPath = join(piDir, "package.json");
    const lockPath = join(piDir, "package-lock.json");
    const [onDiskPackageJson, onDiskLock] = await Promise.all([
      readFile(packageJsonPath, "utf8").catch(() => null),
      readFile(lockPath, "utf8").catch(() => null),
    ]);
    const hasRuntime = isTermux
      ? (await resolveOopbotBinary(ctx.env.root, ctx.env.prefix)) !== null
      : await piInstallComplete(ctx.env.root);
    // rpc refuses a package.json that differs from the app's, so both files must match to skip the install.
    const current =
      ctx.params.reinstall !== true &&
      (await installedPiVersion(ctx.env.root)) === expectedVersion &&
      (isTermux ? hasRuntime : onDiskPackageJson === packageJson && onDiskLock === packageLock && hasRuntime);
    if (current) return;
    const marker = piInstallingMarker(ctx.env.root);
    await writeFile(marker, "");
    if (isTermux) {
      await installOopbotRuntime(ctx, expectedVersion);
    } else {
      await writeFile(packageJsonPath, packageJson);
      await writeFile(lockPath, packageLock);
      await npmCi(ctx, piDir, lock);
    }
    await rm(marker, { force: true });
  });
  await installPipkinExtensions(ctx.env.root, pipkinExtensions);
  // Remote hosts are secondary here: a hosts file that can't be read mustn't keep local chats from starting.
  await refreshSshConfig(remotePaths(ctx.env.configDir, ctx.env.prefix, ctx.env.home)).catch((error: unknown) => {
    process.stderr.write(`pipkin-host ensure: couldn't rewrite ssh/config: ${String(error)}\n`);
  });

  const piVersion = await installedPiVersion(ctx.env.root);
  return isTermux ? { piVersion, oopbotVersion: piVersion } : { piVersion };
}
