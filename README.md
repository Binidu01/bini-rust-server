# bini-rust-server

<div align="center">

[![license](https://img.shields.io/badge/license-MIT-00CFFF?labelColor=0a0a0a&style=flat-square)](./LICENSE)
[![node](https://img.shields.io/badge/node-%3E%3D20-00CFFF?labelColor=0a0a0a&style=flat-square)](https://nodejs.org)
[![rust](https://img.shields.io/badge/built%20with-Rust-00CFFF?labelColor=0a0a0a&style=flat-square)](https://www.rust-lang.org)

**Rust-powered production server for [bini-router](https://www.npmjs.com/package/bini-router) apps.**  
Serves your `dist/` statically and proxies `/api/*` to your `src/app/api/` handlers — production-grade performance with a zero-config experience.

</div>

---

## Features

- 🦀 **Rust core** — built on [Axum](https://github.com/tokio-rs/axum) for maximum throughput and minimal memory use
- 🗂️ **Static file serving** — serves `dist/` with correct MIME types and smart cache headers
- 🌐 **API routes** — proxies `/api/*` to a Node.js child process running your `src/app/api/` handlers (Hono apps + plain functions)
- 🔀 **SPA fallback** — unknown routes serve `dist/index.html`
- 🌿 **Auto env loading** — `.env`, `.env.local`, `.env.production`, `.env.development` loaded automatically
- 🛡️ **Security headers** — HSTS, `X-Frame-Options`, `X-Content-Type-Options`, `Referrer-Policy`, and `Permissions-Policy` out of the box
- 🗜️ **Compression** — gzip/deflate/brotli response compression built in
- ⏱️ **Timeouts** — 30s body read timeout + 30s handler timeout
- 🔒 **Body limit** — 10MB request body limit
- 🔌 **Port auto-increment** — starts at `3000`, increments if busy
- 🪄 **Graceful shutdown** — handles `SIGTERM` + `SIGINT`
- ⌨️ **Keyboard shortcuts** — open in browser, quit, and help from the terminal
- 🖥️ **Cross-platform** — works on Windows, macOS, and Linux

---

## Requirements

- Node.js **≥ 20** (runs your API handlers)
- A [bini-router](https://www.npmjs.com/package/bini-router) project with a built `dist/`

---

## Usage

Run the binary from your project root:

```bash
./bini-rust-server
```

Or on Windows:

```bash
.\bini-rust-server.exe
```

Terminal output:

```
  ß Bini.js  (production)
  ➜  Environments: .env
  ➜  Local:   http://localhost:3000/
  ➜  Network: http://192.168.1.5:3000/
  ➜  press h + enter to show help
```

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| `h` + enter | Show available shortcuts |
| `o` + enter | Open in browser |
| `q` + enter | Quit |

---

## How it works

bini-rust-server runs two processes:

1. **Rust (Axum)** — listens on your public port (default `3000`). Serves `dist/` statically and reverse-proxies `/api/*` requests to the Node process.
2. **Node.js child process** — binds to an internal loopback port (`port + 1`), imports your API handlers from `src/app/api/`, and handles requests. Never exposed to the network.

Your API handlers run in Node.js exactly as written — no compilation step needed — while the static file serving and request routing benefit from Rust's performance.

---

## Environment Variables

`.env` (and `.env.local`, `.env.production`, `.env.development`) are loaded automatically at startup. All vars are available in `process.env` in your API handlers.

```env
PORT=3000
SMTP_USER=user@smtp.example.com
SMTP_PASS=your_password
```

Real environment variables always take precedence over `.env` files — they are never overwritten.

---

## Important: ship your `src/` folder

bini-rust-server runs your API handlers directly from `src/app/api/` — they are **not** compiled into `dist/`. When deploying, make sure your server has access to both `dist/` and `src/app/api/`.

For VPS/pm2 this means deploying your full project directory, not just `dist/`. For Railway, Render, and Fly.io this happens automatically since they clone your repository.

---

## Port

Default port is `3000`. Override via `.env` or environment variable:

```env
PORT=8080
```

```bash
PORT=8080 ./bini-rust-server
```

If the port is busy, bini-rust-server automatically increments and warns:

```
  ⚠  Port 3000 in use — using 3001 instead.
```

---

## Configurable Directories

By default bini-rust-server reads from `src/app/api/` and `dist/`. Override via env vars:

```env
BINI_API_DIR=src/api        # default: src/app/api
BINI_DIST_DIR=build         # default: dist
```

---

## Configurable Limits

| Variable | Default | Description |
|---|---|---|
| `BINI_BODY_TIMEOUT_SECS` | `30` | Max seconds to read a request body |
| `BINI_HANDLER_TIMEOUT_SECS` | `30` | Max seconds for an API handler to respond |
| `BINI_BODY_SIZE_LIMIT` | `10485760` | Max request body size in bytes (10MB) |
| `BINI_POOL_MAX_IDLE` | `32` | Max idle connections in the proxy HTTP pool |

---

## Cache headers

| File type | Cache policy |
|---|---|
| `.js`, `.css`, `.woff`, `.woff2`, `.ttf` | `public, max-age=31536000, immutable` |
| `.html`, `/` | `no-cache` |
| Everything else | `public, max-age=3600` |

Vite hashes JS and CSS filenames at build time, so immutable caching is safe — the browser fetches new files automatically when the hash changes.

---

## Deployment

### VPS (Ubuntu, Debian, etc.)

Copy the binary and your project to the server, then run:

```bash
./bini-rust-server
```

Use [pm2](https://pm2.keymetrics.io/) or systemd to keep it running:

```bash
# pm2
pm2 start ./bini-rust-server --name my-app
pm2 save
pm2 startup
```

> **Note:** Node.js ≥ 20 must be installed on the server for API routes to work.

### Railway / Render / Fly.io

Set the start command to run the binary. These platforms inject `PORT` automatically and bini-rust-server will pick it up.

```toml
# fly.toml
[processes]
  app = "./bini-rust-server"
```

---

## vs `vite preview`

| Feature | `vite preview` | `bini-rust-server` |
|---|---|---|
| Serves `dist/` | ✅ | ✅ |
| API routes | ✅ | ✅ |
| SPA fallback | ✅ | ✅ |
| Auto env loading | ✅ | ✅ |
| Keyboard shortcuts | ✅ | ✅ |
| Production use | ❌ | ✅ |
| Rust-powered core | ❌ | ✅ |
| Gzip / Brotli compression | ❌ | ✅ |
| Security headers | ❌ | ✅ |
| Body timeout | ❌ | ✅ 30s |
| Body size limit | ❌ | ✅ 10MB |
| Handler timeout | ❌ | ✅ 30s |
| Graceful shutdown | ❌ | ✅ |
| Configurable dirs | ❌ | ✅ |

---

## License

MIT © [Binidu Ranasinghe](https://bini.js.org)
