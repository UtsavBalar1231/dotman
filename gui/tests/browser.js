"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

module.exports = async function withBrowser(check) {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "dt-"));
  try {
    const browser = spawn(process.env.CHROMIUM || "chromium", [
      "--headless", "--disable-gpu", "--disable-background-networking", "--disable-component-update",
      "--no-first-run", "--no-proxy-server", "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1",
      `--user-data-dir=${path.join(temp, "profile")}`, "--remote-debugging-pipe", "about:blank"
    ], {
      stdio: ["ignore", "ignore", "pipe", "pipe", "pipe"],
      env: { PATH: process.env.PATH, HOME: temp, TMPDIR: temp, XDG_CONFIG_HOME: path.join(temp, "config"),
        XDG_CACHE_HOME: path.join(temp, "cache"), XDG_DATA_HOME: path.join(temp, "data"),
        XDG_STATE_HOME: path.join(temp, "state"), XDG_RUNTIME_DIR: temp,
        DBUS_SESSION_BUS_ADDRESS: `unix:path=${path.join(temp, "no-bus")}`,
        DBUS_SYSTEM_BUS_ADDRESS: `unix:path=${path.join(temp, "no-bus")}` }
    });
    const input = browser.stdio[3], output = browser.stdio[4];
    let pending, failure, closing = false, id = 0, buffer = "", stderr = "";
    function fail(error) {
      failure ??= error;
      pending?.reject(failure);
      pending = undefined;
    }
    browser.once("error", fail);
    browser.stderr.setEncoding("utf8").on("data", chunk => { stderr = (stderr + chunk).slice(-8192); });
    const closed = new Promise(resolve => browser.once("close", (code, signal) => {
      if (!closing || code !== 0) fail(new Error(`Chromium exited (${signal ?? code})\n${stderr}`));
      resolve();
    }));
    const deadline = setTimeout(() => {
      fail(new Error(`Chromium checks timed out after 30 seconds\n${stderr}`));
      browser.kill("SIGKILL");
    }, 30000);
    input.on("error", fail);
    output.on("error", fail);
    // DevTools pipe frames are NUL-delimited; only one request is in flight.
    output.setEncoding("utf8").on("data", chunk => {
      try {
        buffer += chunk;
        let end;
        while ((end = buffer.indexOf("\0")) !== -1) {
          const message = JSON.parse(buffer.slice(0, end));
          buffer = buffer.slice(end + 1);
          if (!pending || message.id !== id) continue;
          const { resolve, reject, method } = pending;
          pending = undefined;
          if (message.error) reject(new Error(`${method}: ${message.error.message}`));
          else resolve(message.result);
        }
      } catch (error) { fail(error); }
    });
    const send = (method, params, sessionId) => new Promise((resolve, reject) => {
      if (failure) return reject(failure);
      pending = { resolve, reject, method };
      input.write(JSON.stringify({ id: ++id, method, params, sessionId }) + "\0");
    });
    try {
      await check(send);
    } finally {
      closing = true;
      // Browser.close can close the pipe before acknowledging the request.
      try {
        if (failure) browser.kill();
        else await Promise.race([send("Browser.close"), closed]);
      } finally {
        await closed;
        clearTimeout(deadline);
      }
    }
    if (failure) throw failure;
  } finally {
    fs.rmSync(temp, { recursive: true, force: true });
  }
};
