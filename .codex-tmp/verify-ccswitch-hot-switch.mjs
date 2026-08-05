import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

const port = Number.parseInt(process.argv[2] ?? "9223", 10);
const configPath = String.raw`C:\Users\tianh\.codex\config.toml`;
const authPath = String.raw`C:\Users\tianh\.codex\auth.json`;
const trustPath = String.raw`C:\Users\tianh\.codex\browser-client-trust.json`;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function hashFile(path) {
  const content = await readFile(path);
  return createHash("sha256").update(content).digest("hex");
}

async function waitForTarget() {
  const deadline = Date.now() + 40_000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/json/list`);
      if (response.ok) {
        const targets = await response.json();
        const page = targets.find(
          (target) => target.type === "page" && target.webSocketDebuggerUrl,
        );
        if (page) return page;
      }
    } catch {
      // WebView2 may still be starting.
    }
    await sleep(250);
  }
  throw new Error(`WebView2 CDP target did not appear on port ${port}`);
}

class CdpClient {
  constructor(url) {
    this.socket = new WebSocket(url);
    this.nextId = 1;
    this.pending = new Map();
  }

  async open() {
    await new Promise((resolve, reject) => {
      this.socket.addEventListener("open", resolve, { once: true });
      this.socket.addEventListener("error", reject, { once: true });
    });
    this.socket.addEventListener("message", (event) => {
      const message = JSON.parse(String(event.data));
      if (!message.id) return;
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result);
    });
  }

  send(method, params = {}) {
    const id = this.nextId++;
    const promise = new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
    });
    this.socket.send(JSON.stringify({ id, method, params }));
    return promise;
  }

  async evaluate(expression) {
    const response = await this.send("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (response.exceptionDetails) {
      const detail =
        response.exceptionDetails.exception?.description ??
        response.exceptionDetails.text ??
        "Runtime evaluation failed";
      throw new Error(detail);
    }
    return response.result?.value;
  }

  close() {
    this.socket.close();
  }
}

function invokeExpression(command, args = {}) {
  return `(async () => await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(
    command,
  )}, ${JSON.stringify(args)}))()`;
}

async function readLiveInvariants() {
  const [config, authHash, trustText] = await Promise.all([
    readFile(configPath, "utf8"),
    hashFile(authPath),
    readFile(trustPath, "utf8"),
  ]);
  const trust = JSON.parse(trustText);
  const hashes = trust.trustedBrowserClientSha256;
  const trustedLine = config.match(
    /^NODE_REPL_TRUSTED_BROWSER_CLIENT_SHA256S\s*=\s*["']([^"']*)["']/m,
  );
  return {
    configHash: createHash("sha256").update(config).digest("hex"),
    authHash,
    localBaseUrl: /base_url\s*=\s*["']http:\/\/127\.0\.0\.1:15721\/v1["']/.test(
      config,
    ),
    responsesWireApi: /wire_api\s*=\s*["']responses["']/.test(config),
    allBrowserTrustHashesPresent:
      Array.isArray(hashes) &&
      hashes.every((hash) => trustedLine?.[1].split(",").includes(hash)),
  };
}

const target = await waitForTarget();
const cdp = new CdpClient(target.webSocketDebuggerUrl);
await cdp.open();
await cdp.send("Runtime.enable");

const providers = await cdp.evaluate(invokeExpression("get_providers", { app: "codex" }));
const originalProviderId = await cdp.evaluate(
  invokeExpression("get_current_provider", { app: "codex" }),
);
const entries = Object.entries(providers).map(([id, provider]) => ({
  id,
  name: provider.name,
  category: provider.category ?? null,
}));
const customProviders = entries.filter((provider) => provider.category !== "official");
const officialProviders = entries.filter((provider) => provider.category === "official");
const baseline = await readLiveInvariants();
const customResults = [];
const officialResults = [];

try {
  for (const provider of customProviders) {
    let error = null;
    try {
      await cdp.evaluate(
        invokeExpression("switch_provider", { app: "codex", id: provider.id }),
      );
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught);
    }

    const currentProviderId = await cdp.evaluate(
      invokeExpression("get_current_provider", { app: "codex" }),
    );
    const proxyStatus = await cdp.evaluate(invokeExpression("get_proxy_status"));
    const healthResponse = await fetch("http://127.0.0.1:15721/health");
    const health = await healthResponse.json();
    const invariants = await readLiveInvariants();
    customResults.push({
      id: provider.id,
      name: provider.name,
      switched: error === null && currentProviderId === provider.id,
      proxyRunning: proxyStatus.running === true,
      health: health.status,
      localBaseUrl: invariants.localBaseUrl,
      responsesWireApi: invariants.responsesWireApi,
      browserTrustPreserved: invariants.allBrowserTrustHashesPresent,
      authPlaceholderStable: invariants.authHash === baseline.authHash,
      error,
    });
  }

  for (const provider of officialProviders) {
    let error = null;
    try {
      await cdp.evaluate(
        invokeExpression("switch_provider", { app: "codex", id: provider.id }),
      );
    } catch (caught) {
      error = caught instanceof Error ? caught.message : String(caught);
    }
    const currentProviderId = await cdp.evaluate(
      invokeExpression("get_current_provider", { app: "codex" }),
    );
    officialResults.push({
      id: provider.id,
      name: provider.name,
      blockedByProxySafetyPolicy: Boolean(error),
      currentProviderUnchanged: currentProviderId !== provider.id,
      error,
    });
  }
} finally {
  await cdp.evaluate(
    invokeExpression("switch_provider", { app: "codex", id: originalProviderId }),
  );
  cdp.close();
}

const restoredProviderId = await (async () => {
  const retryTarget = await waitForTarget();
  const retry = new CdpClient(retryTarget.webSocketDebuggerUrl);
  await retry.open();
  const value = await retry.evaluate(
    invokeExpression("get_current_provider", { app: "codex" }),
  );
  retry.close();
  return value;
})();
const finalInvariants = await readLiveInvariants();

console.log(
  JSON.stringify(
    {
      originalProviderId,
      restoredProviderId,
      customProviderCount: customResults.length,
      customSuccessCount: customResults.filter((result) => result.switched).length,
      customResults,
      officialResults,
      finalInvariants,
    },
    null,
    2,
  ),
);
