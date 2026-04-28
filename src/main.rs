// bini-server/src/main.rs

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(clippy::unwrap_in_result)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{any, get},
    Router,
};
use http_body_util::BodyExt;
use tokio::net::TcpListener;
use tower_http::{
    compression::CompressionLayer,
    cors::CorsLayer,
    limit::RequestBodyLimitLayer,
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
    timeout::TimeoutLayer,
};
use uuid::Uuid;

// ─── Constants ────────────────────────────────────────────────────────────────

const DEFAULT_PORT: u16 = 3000;
const BODY_TIMEOUT_SECS: u64 = 30;
const HANDLER_TIMEOUT_SECS: u64 = 30;
const BODY_SIZE_LIMIT: usize = 10 * 1024 * 1024;
const PORT_SEARCH_RANGE: u16 = 99;

// ─── Runtime-configurable settings ───────────────────────────────────────────

struct Config {
    body_timeout_secs: u64,
    handler_timeout_secs: u64,
    body_size_limit: usize,
    pool_max_idle_per_host: usize,
}

impl Config {
    fn from_env() -> Self {
        Self {
            body_timeout_secs: env_u64("BINI_BODY_TIMEOUT_SECS", BODY_TIMEOUT_SECS),
            handler_timeout_secs: env_u64("BINI_HANDLER_TIMEOUT_SECS", HANDLER_TIMEOUT_SECS),
            body_size_limit: env_usize("BINI_BODY_SIZE_LIMIT", BODY_SIZE_LIMIT),
            pool_max_idle_per_host: env_usize("BINI_POOL_MAX_IDLE", 32),
        }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ─── ANSI colours ─────────────────────────────────────────────────────────────

struct C;
impl C {
    const CYAN: &'static str = "\x1b[36m";
    const RESET: &'static str = "\x1b[0m";
    const GREEN: &'static str = "\x1b[32m";
    const RED: &'static str = "\x1b[31m";
    const YELLOW: &'static str = "\x1b[33m";
    const BOLD: &'static str = "\x1b[1m";
    const DIM: &'static str = "\x1b[2m";
}

// ─── Env loader ───────────────────────────────────────────────────────────────
//
// SAFETY: load_env() MUST be called before the Tokio runtime is started.
// std::env::set_var is not thread-safe; calling it before any threads are
// spawned is sound. We enforce this via the sync `main` shim below.

const ENV_FILES: &[&str] = &[".env", ".env.local", ".env.production", ".env.development"];

fn detect_env_files(dir: &PathBuf) -> Vec<String> {
    ENV_FILES
        .iter()
        .filter(|&&f| dir.join(f).exists())
        .map(|&f| f.to_string())
        .collect()
}

fn load_env(dir: &PathBuf) {
    for &file in ENV_FILES {
        let Ok(contents) = std::fs::read_to_string(dir.join(file)) else {
            continue;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim().trim_matches('"').trim_matches('\'');
                // Only set if not already present — never overwrite real env vars.
                // SAFETY: called before Tokio runtime; no other threads exist yet.
                #[allow(unused_unsafe)]
                if std::env::var(key).is_err() {
                    unsafe { std::env::set_var(key, val) };
                }
            }
        }
    }
}

// ─── Network helpers ──────────────────────────────────────────────────────────

fn get_network_ip() -> Option<String> {
    let Ok(pairs) = local_ip_address::list_afinet_netifas() else {
        return None;
    };

    let mut best: Option<String> = None;

    for (name, ip) in pairs {
        if ip.is_loopback() || !ip.is_ipv4() {
            continue;
        }

        // Skip virtual/tunnel interfaces by name.
        let name_lower = name.to_lowercase();
        if name_lower.contains("docker")
            || name_lower.contains("veth")
            || name_lower.contains("br-")
            || name_lower.contains("vbox")
            || name_lower.contains("vmnet")
            || name_lower.contains("tun")
            || name_lower.contains("tap")
            || name_lower.contains("utun")
        {
            continue;
        }

        let octets = match ip {
            std::net::IpAddr::V4(v4) => v4.octets(),
            _ => continue,
        };

        // Skip APIPA / link-local (169.254.0.0/16) — assigned when no DHCP
        // server is reachable; these addresses are never routable.
        if octets[0] == 169 && octets[1] == 254 {
            continue;
        }

        // Prefer private LAN ranges: 10/8, 172.16/12, 192.168/16.
        let is_private = octets[0] == 10
            || (octets[0] == 172 && (16..=31).contains(&octets[1]))
            || (octets[0] == 192 && octets[1] == 168);

        if is_private {
            // First private address wins — most likely the real LAN IP.
            return Some(ip.to_string());
        }

        // Keep as fallback in case no private address exists.
        if best.is_none() {
            best = Some(ip.to_string());
        }
    }

    best
}

// ─── Port helpers ─────────────────────────────────────────────────────────────

async fn bind_listener(start: u16) -> (TcpListener, u16) {
    let end = start.saturating_add(PORT_SEARCH_RANGE);
    for port in start..=end {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                if port != start {
                    println!(
                        "\n  {}⚠{}  Port {} in use — using {} instead.",
                        C::YELLOW,
                        C::RESET,
                        start,
                        port
                    );
                }
                return (listener, port);
            }
            Err(_) => continue,
        }
    }
    panic!("No free port found in range {start}–{end}");
}

// ─── Banner ───────────────────────────────────────────────────────────────────

fn print_banner(port: u16) {
    let env_files = detect_env_files(&std::env::current_dir().unwrap_or_default());

    println!(
        "\n  {}{}ß Bini.js{}  (production)",
        C::BOLD, C::CYAN, C::RESET,
    );

    if !env_files.is_empty() {
        println!(
            "  {}➜{}  Environments: {}",
            C::GREEN, C::RESET, env_files.join(", "),
        );
    }

    println!(
        "  {}➜{}  Local:   {}http://localhost:{}/{}",
        C::GREEN, C::RESET, C::CYAN, port, C::RESET
    );

    if let Some(ip) = get_network_ip() {
        println!(
            "  {}➜{}  Network: {}http://{}:{}/{}",
            C::GREEN, C::RESET, C::CYAN, ip, port, C::RESET
        );
    }

    println!(
        "  {}➜{}  {}press {}h{} + {}enter{} to show help{}",
        C::GREEN, C::RESET, C::DIM, C::RESET, C::DIM, C::RESET, C::DIM, C::RESET,
    );

    println!();
}

// ─── Static cache headers ─────────────────────────────────────────────────────

async fn cache_headers(req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let mut res = next.run(req).await;

    if res.headers().contains_key(header::CACHE_CONTROL) {
        return res;
    }

    let cache_value = if path.ends_with(".js")
        || path.ends_with(".mjs")
        || path.ends_with(".css")
        || path.ends_with(".woff")
        || path.ends_with(".woff2")
        || path.ends_with(".ttf")
    {
        "public, max-age=31536000, immutable"
    } else if path.ends_with(".html") || path == "/" {
        "no-cache"
    } else {
        "public, max-age=3600"
    };

    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache_value));

    res
}

// ─── Node API Server ──────────────────────────────────────────────────────────

const NODE_API_SERVER: &str = r#"
import { existsSync, readdirSync } from 'node:fs';
import { join, extname, basename } from 'node:path';
import { createServer } from 'node:http';
import { pathToFileURL } from 'node:url';

const API_DIR         = process.env.BINI_API_DIR          || 'src/app/api';
const PORT            = parseInt(process.env.BINI_API_PORT        || '0',  10);
const HANDLER_TIMEOUT = parseInt(process.env.HANDLER_TIMEOUT      || '30000', 10);
const BODY_SIZE_LIMIT = parseInt(process.env.BINI_BODY_SIZE_LIMIT || String(10 * 1024 * 1024), 10);

function scanApiRoutes(dir, baseRoute = '', depth = 0) {
  const routes = [];
  if (depth > 100 || !existsSync(dir)) return routes;

  let entries;
  try { entries = readdirSync(dir, { withFileTypes: true }); } catch { return routes; }

  for (const entry of entries) {
    if (entry.name.startsWith('_') || entry.name.startsWith('.')) continue;
    const fullPath = join(dir, entry.name);

    if (entry.isDirectory()) {
      const isCatchAll = entry.name.startsWith('[...') && entry.name.endsWith(']');
      const isDynamic  = entry.name.startsWith('[')    && entry.name.endsWith(']');
      const segment    = isCatchAll ? '*' : isDynamic ? ':' + entry.name.slice(1, -1) : entry.name;
      routes.push(...scanApiRoutes(fullPath, baseRoute + '/' + segment, depth + 1));
      continue;
    }

    const ext  = extname(entry.name);
    const base = basename(entry.name, ext);
    if (!['.ts', '.js', '.mjs', '.cjs'].includes(ext)) continue;

    const isCatchAll = base.startsWith('[...') && base.endsWith(']');
    const isDynamic  = base.startsWith('[')    && base.endsWith(']');

    let routePath;
    if (isCatchAll)            routePath = baseRoute + '/*';
    else if (base === 'index') routePath = baseRoute || '/';
    else if (isDynamic)        routePath = baseRoute + '/:' + base.slice(1, -1);
    else                       routePath = baseRoute + '/' + base;

    routes.push({ routePath, filePath: fullPath });
  }
  return routes;
}

function matchRoute(pattern, pathname) {
  const patParts = pattern.split('/').filter(Boolean);
  const urlParts = pathname.split('/').filter(Boolean);

  const isCatchAll = patParts[patParts.length - 1] === '*';
  if (isCatchAll) {
    const prefix = patParts.slice(0, -1);
    if (urlParts.length < prefix.length) return null;
    for (let i = 0; i < prefix.length; i++) {
      if (!prefix[i].startsWith(':') && prefix[i] !== urlParts[i]) return null;
    }
    return { '*': urlParts.slice(prefix.length).join('/') };
  }

  if (patParts.length !== urlParts.length) return null;

  const params = {};
  for (let i = 0; i < patParts.length; i++) {
    if (patParts[i].startsWith(':')) {
      const value = decodeURIComponent(urlParts[i]);
      if (value.includes('..') || value.includes('//')) return null;
      params[patParts[i].slice(1)] = value;
    } else if (patParts[i] !== urlParts[i]) {
      return null;
    }
  }
  return params;
}

const handlerCache = new Map();

async function importHandler(filePath) {
  if (handlerCache.has(filePath)) return handlerCache.get(filePath);
  try {
    const mod     = await import(pathToFileURL(filePath).href);
    const handler = mod.default ?? null;
    handlerCache.set(filePath, handler);
    return handler;
  } catch (e) {
    console.error('[bini-api] import error:', filePath, e?.message);
    handlerCache.set(filePath, null);
    return null;
  }
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;

    const timer = setTimeout(() => {
      req.destroy();
      reject(new Error('Request body timeout'));
    }, HANDLER_TIMEOUT);

    req.on('data', chunk => {
      size += chunk.length;
      if (size > BODY_SIZE_LIMIT) {
        clearTimeout(timer);
        req.destroy();
        reject(new Error('Request body too large'));
        return;
      }
      chunks.push(chunk);
    });

    req.on('end',   () => { clearTimeout(timer); resolve(chunks.length > 0 ? Buffer.concat(chunks) : null); });
    req.on('error', err => { clearTimeout(timer); reject(err); });
  });
}

function normalizeHeaders(raw) {
  const out = {};
  for (const [k, v] of Object.entries(raw)) {
    if (v === undefined) continue;
    out[k] = Array.isArray(v)
      ? (k.toLowerCase() === 'cookie' ? v.join('; ') : v.join(', '))
      : v;
  }
  return out;
}

const CORS_HEADERS = {
  'Access-Control-Allow-Origin':  '*',
  'Access-Control-Allow-Methods': 'GET,POST,PUT,PATCH,DELETE,OPTIONS',
  'Access-Control-Allow-Headers': 'Content-Type,Authorization,X-Request-ID',
  'Vary': 'Origin',
};

const routes = scanApiRoutes(API_DIR);

await Promise.all(routes.map(r => importHandler(r.filePath)));

const server = createServer(async (req, res) => {
  const method = (req.method || 'GET').toUpperCase();

  if (method === 'OPTIONS') {
    res.writeHead(204, { ...CORS_HEADERS, 'Access-Control-Max-Age': '86400' });
    res.end();
    return;
  }

  const host   = req.headers.host || 'localhost';
  const rawUrl = `http://${host}${req.url || '/'}`;

  let parsedUrl;
  try { parsedUrl = new URL(rawUrl); } catch {
    res.writeHead(400, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: 'Bad request URL' }));
    return;
  }

  const { pathname, search } = parsedUrl;

  const matchPath = pathname.startsWith('/api')
    ? pathname.slice(4) || '/'
    : pathname;

  let matchedRoute = null;
  let params       = null;
  for (const route of routes) {
    params = matchRoute(route.routePath, matchPath);
    if (params !== null) { matchedRoute = route; break; }
  }

  if (!matchedRoute) {
    res.writeHead(404, { 'Content-Type': 'application/json', ...CORS_HEADERS });
    res.end(JSON.stringify({ error: `No API handler for ${pathname}` }));
    return;
  }

  let bodyBuffer = null;
  if (!['GET', 'HEAD'].includes(method)) {
    try {
      bodyBuffer = await readBody(req);
    } catch (e) {
      const status = e.message.includes('too large') ? 413 : 408;
      res.writeHead(status, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: e.message }));
      return;
    }
  }

  const handler = await importHandler(matchedRoute.filePath);
  if (!handler) {
    res.writeHead(500, { 'Content-Type': 'application/json', ...CORS_HEADERS });
    res.end(JSON.stringify({ error: 'Failed to load handler' }));
    return;
  }

  let webRes;
  try {
    const normalizedHeaders = normalizeHeaders(req.headers);

    if (typeof handler.fetch === 'function') {
      const honoUrl = `http://${host}${matchPath}${search}`;
      const honoReq = new Request(honoUrl, {
        method,
        headers: normalizedHeaders,
        body: bodyBuffer,
      });
      webRes = await handler.fetch(honoReq);

    } else if (typeof handler === 'function') {
      const reqWithParams = new Request(rawUrl, {
        method,
        headers: {
          ...normalizedHeaders,
          'x-bini-params': JSON.stringify(params ?? {}),
        },
        body: bodyBuffer,
      });

      const result = await Promise.race([
        handler(reqWithParams),
        new Promise((_, reject) =>
          setTimeout(() => reject(new Error('Handler timeout')), HANDLER_TIMEOUT)
        ),
      ]);

      webRes = result instanceof Response
        ? result
        : new Response(JSON.stringify(result), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          });

    } else {
      res.writeHead(500, { 'Content-Type': 'application/json', ...CORS_HEADERS });
      res.end(JSON.stringify({ error: 'Handler has no valid default export' }));
      return;
    }
  } catch (e) {
    console.error('[bini-api] handler error:', e?.message ?? e);
    res.writeHead(500, { 'Content-Type': 'application/json', ...CORS_HEADERS });
    res.end(JSON.stringify({ error: e?.message ?? 'Internal server error' }));
    return;
  }

  const finalHeaders = { ...CORS_HEADERS };
  webRes.headers.forEach((v, k) => { finalHeaders[k] = v; });
  res.writeHead(webRes.status, finalHeaders);
  res.end(Buffer.from(await webRes.arrayBuffer()));
});

server.listen(PORT, '127.0.0.1', () => {
  process.stdout.write(JSON.stringify({ port: server.address().port }) + '\n');
});

server.on('error', err => {
  console.error('[bini-api] server error:', err.message);
  process.exit(1);
});
"#;

async fn start_node_api(
    port: u16,
    handler_timeout_secs: u64,
    body_size_limit: usize,
    api_dir: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut child = tokio::process::Command::new("node")
        .arg("--input-type=module")
        .env("BINI_API_DIR", api_dir)
        .env("BINI_API_PORT", port.to_string())
        .env("HANDLER_TIMEOUT", (handler_timeout_secs * 1000).to_string())
        .env("BINI_BODY_SIZE_LIMIT", body_size_limit.to_string())
        .env("NODE_ENV", "production")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;

    use tokio::io::AsyncWriteExt;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(NODE_API_SERVER.as_bytes()).await?;
    }

    let stdout = child.stdout.take().ok_or("Node produced no stdout")?;
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut line = String::new();

    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line),
    )
        .await
        .map_err(|_| "Node API server did not start within 15s")??;

    let info: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| format!("Unexpected Node startup message: {e} (got: {line:?})"))?;

    let reported_port = info["port"]
        .as_u64()
        .ok_or("Missing 'port' field in Node startup JSON")? as u16;

    if reported_port != port {
        return Err(
            format!("Node started on port {} but expected {}", reported_port, port).into(),
        );
    }

    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    Ok(())
}

// ─── Health check ─────────────────────────────────────────────────────────────

async fn health() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(r#"{"status":"ok"}"#))
        .expect("health: static inputs always produce a valid response")
}

// ─── API reverse proxy ────────────────────────────────────────────────────────

struct AppState {
    api_port: u16,
    client: reqwest::Client,
    config: Config,
}

async fn api_proxy(
    State(state): State<Arc<AppState>>,
    AxumPath(path): AxumPath<String>,
    req: Request<Body>,
) -> Response<Body> {
    let (parts, body) = req.into_parts();

    let request_id = parts
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let body_bytes = match tokio::time::timeout(
        Duration::from_secs(state.config.body_timeout_secs),
        body.collect(),
    )
        .await
    {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(e)) => {
            eprintln!("[proxy] [{request_id}] body read error: {e}");
            return error_response(StatusCode::BAD_REQUEST, "Failed to read request body");
        }
        Err(_) => {
            return error_response(StatusCode::REQUEST_TIMEOUT, "Request body timed out");
        }
    };

    let query = parts
        .uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();

    let upstream = format!(
        "http://127.0.0.1:{}/api/{}{}",
        state.api_port, path, query
    );

    let mut fwd = HeaderMap::new();
    for key in &[
        header::CONTENT_TYPE,
        header::AUTHORIZATION,
        header::ACCEPT,
        header::ACCEPT_ENCODING,
        header::ACCEPT_LANGUAGE,
        header::CACHE_CONTROL,
    ] {
        if let Some(v) = parts.headers.get(key) {
            fwd.insert(key.clone(), v.clone());
        }
    }

    let xff = parts
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1")
        .to_owned();
    if let Ok(v) = HeaderValue::from_str(&xff) {
        fwd.insert("x-forwarded-for", v);
    }

    if let Ok(v) = HeaderValue::from_str(&request_id) {
        fwd.insert("x-request-id", v);
    }

    let proxy_req = match state
        .client
        .request(parts.method, &upstream)
        .headers(fwd)
        .body(body_bytes)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[proxy] [{request_id}] request build error: {e}");
            return error_response(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };

    match tokio::time::timeout(
        Duration::from_secs(state.config.handler_timeout_secs),
        state.client.execute(proxy_req),
    )
        .await
    {
        Ok(Ok(res)) => {
            let status = res.status();
            let res_headers = res.headers().clone();
            let body_bytes = match res.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[proxy] [{request_id}] upstream body read error: {e}");
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "Failed to read upstream response",
                    );
                }
            };

            let mut builder = Response::builder().status(status);
            if let Some(h) = builder.headers_mut() {
                for (k, v) in &res_headers {
                    match k.as_str() {
                        "transfer-encoding"
                        | "connection"
                        | "keep-alive"
                        | "te"
                        | "trailer"
                        | "upgrade"
                        | "proxy-authenticate"
                        | "proxy-authorization" => continue,
                        _ => {}
                    }
                    h.insert(k.clone(), v.clone());
                }
                if let Ok(v) = HeaderValue::from_str(&request_id) {
                    h.insert("x-request-id", v);
                }
            }
            builder.body(Body::from(body_bytes)).unwrap_or_else(|e| {
                eprintln!("[proxy] [{request_id}] response build error: {e}");
                error_response(StatusCode::INTERNAL_SERVER_ERROR, "Response build error")
            })
        }
        Ok(Err(e)) => {
            eprintln!("[proxy] [{request_id}] upstream error: {e}");
            error_response(StatusCode::BAD_GATEWAY, "API unavailable")
        }
        Err(_) => error_response(StatusCode::GATEWAY_TIMEOUT, "API timed out"),
    }
}

#[inline]
fn error_response(status: StatusCode, msg: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(r#"{{"error":"{msg}"}}"#)))
        .expect("error_response: static inputs always produce a valid response")
}

// ─── Security headers ─────────────────────────────────────────────────────────

const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "SAMEORIGIN"),
    ("x-xss-protection", "0"),
    ("referrer-policy", "strict-origin-when-cross-origin"),
    ("permissions-policy", "camera=(), microphone=(), geolocation=()"),
    ("strict-transport-security", "max-age=31536000; includeSubDomains"),
];

// ─── Graceful shutdown ────────────────────────────────────────────────────────

// ─── Interactive keyboard loop ────────────────────────────────────────────────

async fn keyboard_loop(port: u16) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();

    loop {
        let line = tokio::select! {
            result = lines.next_line() => match result {
                Ok(Some(l)) => l,
                _ => break,
            },
            _ = tokio::signal::ctrl_c() => {
                std::process::exit(0);
            }
        };

        match line.trim() {
            "h" => {
                println!();
                println!("  Shortcuts");
                println!("  {}press {}o{} + {}enter{} to open in browser{}", C::DIM, C::RESET, C::DIM, C::RESET, C::DIM, C::RESET);
                println!("  {}press {}q{} + {}enter{} to quit{}", C::DIM, C::RESET, C::DIM, C::RESET, C::DIM, C::RESET);
                println!();
            }
            "o" => {
                let url = format!("http://localhost:{}/", port);
                #[cfg(target_os = "windows")]
                let _ = std::process::Command::new("cmd")
                    .args(["/c", "start", &url])
                    .spawn();
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("open").arg(&url).spawn();
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            }
            "q" => {
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install SIGINT handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c    => {}
        () = terminate => {}
    }
    // Shutdown silently — no console output.
}

// ─── Entry point ──────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    load_env(&cwd);

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cwd))
}

async fn run(cwd: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env();

    let dist_dir: PathBuf = std::env::var("BINI_DIST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| cwd.join("dist"));

    if !dist_dir.exists() {
        eprintln!(
            "\n  {}✗{}  dist/ not found at {}.\n      Run {}npm run build{} first.\n",
            C::RED,
            C::RESET,
            dist_dir.display(),
            C::CYAN,
            C::RESET
        );
        std::process::exit(1);
    }

    match tokio::process::Command::new("node")
        .arg("--version")
        .output()
        .await
    {
        Err(_) => {
            eprintln!(
                "\n  {}✗{}  Node.js is not installed.\n      Install it from https://nodejs.org\n",
                C::RED, C::RESET
            );
            std::process::exit(1);
        }
        Ok(out) => {
            let raw = String::from_utf8_lossy(&out.stdout);
            let version_str = raw.trim().trim_start_matches('v');
            let major: u32 = version_str
                .split('.')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if major < 20 {
                eprintln!(
                    "\n  {}✗{}  Node.js v20 or later is required (found v{}).\n      Upgrade at https://nodejs.org\n",
                    C::RED, C::RESET, version_str
                );
                std::process::exit(1);
            }
        }
    }

    let default_port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let (listener, port) = bind_listener(default_port).await;
    let (api_listener, api_port) = bind_listener(port + 1).await;
    drop(api_listener);

    let api_dir = std::env::var("BINI_API_DIR").unwrap_or_else(|_| "src/app/api".into());

    start_node_api(api_port, config.handler_timeout_secs, config.body_size_limit, &api_dir)
        .await
        .map_err(|e| {
            eprintln!(
                "\n  {}✗{}  Failed to start Node API process: {}\n",
                C::RED, C::RESET, e
            );
            e
        })?;

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(config.pool_max_idle_per_host)
        .tcp_keepalive(Duration::from_secs(15))
        .timeout(Duration::from_secs(config.handler_timeout_secs))
        .build()?;

    let state = Arc::new(AppState {
        api_port,
        client,
        config,
    });

    let fallback_file = dist_dir.join("index.html");
    let static_svc = ServeDir::new(&dist_dir)
        .fallback(ServeFile::new(&fallback_file));

    let handler_timeout = Duration::from_secs(state.config.handler_timeout_secs);
    let body_size_limit = state.config.body_size_limit;

    let api_router = Router::new()
        .route("/_health", get(health))
        .route("/api/{*path}", any(api_proxy))
        .with_state(state)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            handler_timeout,
        ));

    let base = api_router
        .fallback_service(static_svc)
        .layer(middleware::from_fn(cache_headers))
        .layer(RequestBodyLimitLayer::new(body_size_limit))
        .layer(CorsLayer::permissive())
        .layer(CompressionLayer::new())
        .layer(SetResponseHeaderLayer::if_not_present(
            header::HeaderName::from_static("x-powered-by"),
            HeaderValue::from_static("Bini.js"),
        ));

    let app = SECURITY_HEADERS
        .iter()
        .fold(base, |router, &(name, value)| {
            router.layer(SetResponseHeaderLayer::if_not_present(
                header::HeaderName::from_static(name),
                HeaderValue::from_static(value),
            ))
        });

    print_banner(port);

    tokio::spawn(keyboard_loop(port));

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}