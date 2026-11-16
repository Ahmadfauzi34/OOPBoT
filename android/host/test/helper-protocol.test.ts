import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import os from "node:os";
import path from "node:path";
import { describe, expect, it, onTestFinished } from "vitest";
import { HelperFiles } from "../src/helper/files.js";
import { decryptPairingBody, pair, pairingKey } from "../src/helper/pairing.js";
import {
  clientMac,
  FrameError,
  FrameOpener,
  FrameSealer,
  serverMac,
  sessionKeys,
} from "../src/helper/protocol.js";

// The spec's test vectors; the app's HelperProtocol asserts the same ones.
const K = Buffer.from("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f", "hex");
const nonceC = Buffer.alloc(32, 0xaa);
const nonceS = Buffer.alloc(32, 0xbb);
const { c2s, s2c } = sessionKeys(K, nonceC, nonceS);

describe("oopbot-helper/1 vectors", () => {
  it("derives the handshake MACs and session keys", () => {
    expect(serverMac(K, nonceC, nonceS).toString("hex")).toBe(
      "7af9b89ac35c5bb1313a2ab8d01d3ca7e2b243cfcbf21cbedc00d52382a974cc",
    );
    expect(clientMac(K, nonceC, nonceS).toString("hex")).toBe(
      "8de01dbdf2d3f926a6ee8f0e3db8357437d2706ab10f930ed1d9eda80bb17c49",
    );
    expect(c2s.toString("hex")).toBe("db3a7b81f9befeb674eb0ecadf12d5c13752cd44050c57c4e3887393fd4f468c");
    expect(s2c.toString("hex")).toBe("ee8b812f140b381dd81757e979ce9b072e03061e7994182e3b9a8423a76134a4");
  });

  it("seals frames with a per-direction counter", () => {
    const client = new FrameSealer(c2s);
    expect(client.sealBody({ type: "hello" }).toString("hex")).toBe(
      "6df8de77a014b0cdf50b69ddfe674dbb926daec86e508bc51747045f0e909de4",
    );
    expect(client.sealBody({ type: "hello" }).toString("hex")).toBe(
      "34716c399ab769820eaa79750db9087c88d8cba8323a4dcc1ae00afb9fc09542",
    );
    expect(new FrameSealer(s2c).sealBody({ type: "hello", version: "x" }).toString("hex")).toBe(
      "f29768b7cb5ae0d0e7a53985c69d905e7c002be1b8a713ed5e2e6e76b64b9122af39463ef5f03de812ce15f06068",
    );
  });

  it("opens length-prefixed frames split across chunks", () => {
    const sealer = new FrameSealer(c2s);
    const bytes = Buffer.concat([sealer.seal({ type: "hello" }), sealer.seal({ type: "launch", n: 1 })]);
    const opener = new FrameOpener(c2s);
    expect(opener.push(bytes.subarray(0, 3))).toEqual([]);
    expect(opener.push(bytes.subarray(3, 40))).toEqual([{ type: "hello" }]);
    expect(opener.push(bytes.subarray(40))).toEqual([{ type: "launch", n: 1 }]);
  });

  it("rejects replayed, reordered, tampered and oversized frames", () => {
    const sealer = new FrameSealer(c2s);
    const first = sealer.seal({ type: "hello" });
    const second = sealer.seal({ type: "hello" });

    const replay = new FrameOpener(c2s);
    replay.push(first);
    expect(() => replay.push(first)).toThrow(FrameError);

    expect(() => new FrameOpener(c2s).push(second)).toThrow(FrameError);

    const tampered = Buffer.from(first);
    tampered[6] = (tampered[6] ?? 0) ^ 1;
    expect(() => new FrameOpener(c2s).push(tampered)).toThrow(FrameError);

    expect(() => new FrameOpener(c2s).push(Buffer.from([0x00, 0x41, 0x00, 0x11]))).toThrow(FrameError);
    expect(() => new FrameOpener(c2s).push(Buffer.alloc(4))).toThrow(FrameError);
  });
});

describe("pairing vector", () => {
  const token = Buffer.alloc(32, 0x11);
  const sid = "00112233445566778899aabbccddeeff";
  const body =
    "222222222222222222222222aa7ecbd8e22b36928599e2b343d73b0ede989399a42852de7d1a85ae54a33ead63d625268421e196cc26929d9a4d623c";

  it("derives pk and decrypts the key body", () => {
    expect(pairingKey(token).toString("hex")).toBe("820c287eadee847ce6a8273d681e378e10982b67cb5cc40651a9400797005b0a");
    expect(decryptPairingBody(body, token, sid)).toEqual(K);
  });

  it("rejects a body for another session", () => {
    expect(() => decryptPairingBody(body, token, "ffeeddccbbaa99887766554433221100")).toThrow(
      "The key from OOPBoT didn't match this setup command.",
    );
  });

  it.each([
    ["OOPBOT_URL and OOPBOT_TOKEN", (url: string, hex: string) => ({ OOPBOT_URL: url, OOPBOT_TOKEN: hex })],
  ])("pairs via %s", async (_, buildEnv) => {
    const server = createServer((req, res) => {
      if (req.url === `/oopbot/${sid}/key`) {
        res.writeHead(200, { "content-type": "text/plain" });
        res.end(body);
        return;
      }
      res.writeHead(404);
      res.end();
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", () => resolve()));
    onTestFinished(async () => {
      await new Promise<void>((resolve) => server.close(() => resolve()));
    });

    const tempDir = mkdtempSync(path.join(os.tmpdir(), "oopbot-pair-"));
    onTestFinished(() => rmSync(tempDir, { recursive: true, force: true }));

    const files = new HelperFiles(tempDir);
    const { port } = server.address() as AddressInfo;
    const exitCode = await pair(buildEnv(`http://127.0.0.1:${port}/oopbot/${sid}`, token.toString("hex")), files);
    expect(exitCode).toBe(0);
    expect(readFileSync(files.key, "utf8")).toBe(`${K.toString("hex")}\n`);
  });
});
