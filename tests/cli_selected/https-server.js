const https = require("https");
const fs = require("fs");
const { spawn } = require("child_process");

const [root, port, redirect] = process.argv.slice(2);
const server = https.createServer({
  key: fs.readFileSync(`${root}/tls-key.pem`),
  cert: fs.readFileSync(`${root}/tls-cert.pem`),
}, (req, res) => {
  if (redirect === "require-auth") {
    const expected = `Basic ${Buffer.from(["fixture", ":", "fixture", "-secret"].join("")).toString("base64")}`;
    if (req.headers.authorization !== expected) {
      res.writeHead(401, { "WWW-Authenticate": 'Basic realm="fixture"' }).end();
      return;
    }
    fs.writeFileSync(`${root}/authenticated`, "yes");
  } else if (req.headers.authorization) {
    fs.writeFileSync(`${root}/unexpected-authorization`, "received");
    res.writeHead(403).end();
    return;
  }
  if (redirect === "auth") {
    res.writeHead(401, { "WWW-Authenticate": 'Basic realm="fixture"' }).end();
    return;
  }
  if (redirect === "redirect") {
    res.writeHead(302, { Location: `https://localhost:${port}/other.git` }).end();
    return;
  }
  const url = new URL(req.url, "https://localhost");
  const git = spawn("git", ["http-backend"], {
    env: {
      PATH: process.env.PATH,
      HOME: root,
      XDG_CONFIG_HOME: root,
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_PROJECT_ROOT: root,
      GIT_HTTP_EXPORT_ALL: "1",
      REMOTE_USER: "fixture",
      REQUEST_METHOD: req.method,
      PATH_INFO: url.pathname,
      QUERY_STRING: url.search.slice(1),
      CONTENT_TYPE: req.headers["content-type"] || "",
      CONTENT_LENGTH: req.headers["content-length"] || "",
    },
  });
  const chunks = [];
  git.stdout.on("data", chunk => chunks.push(chunk));
  git.on("close", code => {
    if (code !== 0) { res.writeHead(500).end(); return; }
    const output = Buffer.concat(chunks);
    const end = output.indexOf("\r\n\r\n");
    if (end < 0) { res.writeHead(500).end(); return; }
    for (const line of output.subarray(0, end).toString().split("\r\n")) {
      const colon = line.indexOf(":");
      const name = line.slice(0, colon);
      const value = line.slice(colon + 1).trim();
      if (name.toLowerCase() === "status") res.statusCode = Number(value.split(" ")[0]);
      else res.setHeader(name, value);
    }
    res.end(output.subarray(end + 4));
  });
  req.pipe(git.stdin);
});
server.listen(Number(port), "127.0.0.1");
