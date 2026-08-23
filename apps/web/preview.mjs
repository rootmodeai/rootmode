#!/usr/bin/env node
/**
 * Static preview server for the pre-launch site.
 * Serves this directory and appends POSTs to /api/waitlist into data/waitlist.json.
 */
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.dirname(fileURLToPath(import.meta.url));
const dataDir = path.join(root, "data");
const dataFile = path.join(dataDir, "waitlist.json");
const port = Number(process.env.PORT || 4173);

const types = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".webp": "image/webp",
  ".ico": "image/x-icon",
  ".json": "application/json; charset=utf-8",
  ".txt": "text/plain; charset=utf-8",
};

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on("data", (c) => {
      size += c.length;
      if (size > 32_768) {
        reject(new Error("too large"));
        req.destroy();
        return;
      }
      chunks.push(c);
    });
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    req.on("error", reject);
  });
}

function send(res, status, body, headers = {}) {
  res.writeHead(status, headers);
  res.end(body);
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url || "/", `http://${req.headers.host || "localhost"}`);

  if (req.method === "POST" && url.pathname === "/api/waitlist") {
    try {
      const raw = await readBody(req);
      const data = JSON.parse(raw);
      const email = String(data.email || "").trim().toLowerCase();
      if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) {
        send(res, 400, JSON.stringify({ ok: false, error: "invalid email" }), {
          "content-type": "application/json",
        });
        return;
      }
      const entry = {
        email,
        role: ["use", "provide", "both"].includes(data.role) ? data.role : "use",
        note: String(data.note || "").slice(0, 2000),
        at: new Date().toISOString(),
      };
      fs.mkdirSync(dataDir, { recursive: true });
      const existing = fs.existsSync(dataFile)
        ? JSON.parse(fs.readFileSync(dataFile, "utf8"))
        : [];
      existing.push(entry);
      fs.writeFileSync(dataFile, JSON.stringify(existing, null, 2) + "\n");
      send(res, 200, JSON.stringify({ ok: true }), {
        "content-type": "application/json",
      });
    } catch {
      send(res, 400, JSON.stringify({ ok: false }), {
        "content-type": "application/json",
      });
    }
    return;
  }

  if (req.method !== "GET" && req.method !== "HEAD") {
    send(res, 405, "method not allowed");
    return;
  }

  let file = decodeURIComponent(url.pathname);
  if (file.length > 1 && file.endsWith("/")) {
    send(res, 301, "", { location: file.slice(0, -1) });
    return;
  }
  const pages = {
    "/": "/index.html",
    "/worker": "/pages/worker.html",
    "/explorer": "/pages/explorer.html",
    "/protocol": "/pages/protocol.html",
    "/discovery": "/pages/discovery.html",
    "/brand": "/pages/brand.html",
  };
  if (pages[file]) file = pages[file];
  else if (!file.startsWith("/assets/")) {
    send(res, 404, "not found");
    return;
  }
  const safe = path.normalize(file).replace(/^(\.\.(\/|\\|$))+/, "");
  const full = path.join(root, safe);
  if (!full.startsWith(root)) {
    send(res, 403, "forbidden");
    return;
  }

  fs.readFile(full, (err, buf) => {
    if (err) {
      send(res, 404, "not found");
      return;
    }
    const type = types[path.extname(full)] || "application/octet-stream";
    send(res, 200, req.method === "HEAD" ? undefined : buf, {
      "content-type": type,
      "cache-control": "no-store",
    });
  });
});

server.listen(port, "127.0.0.1", () => {
  console.log(`rootmode landing → http://127.0.0.1:${port}`);
});
