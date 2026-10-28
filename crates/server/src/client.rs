//! `railwatch issues | issue | why | last | compare | tail | open | tools |
//! call | login`: a client of a Railwatch server's MCP endpoint (POST
//! <base>/mcp, JSON-RPC), for agents with a shell and for people. Every
//! command is one or two tools/call requests, so the client never drifts
//! from the tools and works against any server: the cloud, a self-hosted
//! one, or the dev server a Rails app embeds (railwatch-server gem).
//!
//! Where it connects, first match wins:
//!   --url, RAILWATCH_URL, the `url` saved by `railwatch login`, else
//!   http://localhost:${PORT:-3000}${RAILWATCH_SERVER_MOUNT_PATH:-/railwatch}/mcp
//!   (the railwatch-server gem's default mount in development).
//! A URL that does not end in /mcp gets it appended, so the dashboard's
//! address works too.
//!
//! The token (`Authorization: Bearer`): --token, RAILWATCH_TOKEN, or the
//! token saved with the URL by `railwatch login`; none at all for the gem's
//! tokenless development path. A RAILWATCH_TOKEN that holds the app's
//! ingest token (rw_...) is ignored with a warning: /mcp takes API & MCP
//! tokens (rwp_...), and a Rails app's shell usually has the ingest one.
//! The saved token is only sent to the saved URL.
//!
//! Exit codes: 0 ok; 1 an error (unreachable, unauthorized, server error);
//! 2 not found or a usage error (no such issue, route or tool, or a tool
//! the server is too old to have); 3 `compare --fail-if-worse` crossed.

use std::collections::{HashSet, VecDeque};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{Map, Value, json};

mod render;

pub const USAGE: &str = "Client (talks to a Railwatch server's MCP endpoint; see `railwatch help client`):
  railwatch login --url URL [--token T | --token-stdin]   check and save the server (and token, 0600)
  railwatch issues [--status open|resolved|ignored] [--sort recent|impact] [--limit N]
  railwatch issue <KEY>                    the issue, what led up to it, and its dashboard link
  railwatch why <route-or-path> [--window 1h|6h|24h|7d|30d]   why a route is slow
  railwatch last [path] [--kind request|job|any]   the newest request or job, summary first
  railwatch compare [route] [--before D] [--after D] [--since T] [--kind request|job] [--window W]
                    [--fail-if-worse PCT]          before/after a deploy or a mark; exit 3 on a regression
  railwatch tail [path] [--errors] [--slow MS] [--interval SECS]   follow new requests
  railwatch open [KEY | route | execution-id]   print the dashboard URL (and open it in a terminal)
  railwatch tools                          the server's tools
  railwatch call <tool> [--arg k=v]...     any tool; v is JSON when it parses, else a string
  every client command: [--url URL] [--token T] [--app SLUG] [--env NAME] [--json]";

const HELP: &str = "Connection, first match wins:
  --url, RAILWATCH_URL, the url saved by `railwatch login`, else
  http://localhost:${PORT:-3000}${RAILWATCH_SERVER_MOUNT_PATH:-/railwatch}/mcp
  (a Rails app's embedded server in development). /mcp is appended when missing.
Token: --token, RAILWATCH_TOKEN (an API & MCP token, rwp_...; an ingest token, rw_..., is
  ignored), or the one saved for that url; none for the embedded server's tokenless dev path.
Output: a short text summary; --json prints the tool's raw JSON result.
Exit codes: 0 ok, 1 error, 2 not found or usage, 3 compare --fail-if-worse crossed.";

const COMMANDS: &[&str] = &["login", "issues", "issue", "why", "last", "compare", "tail", "open", "tools", "call"];

/// The release that first shipped each tool a command needs, for the
/// message an older server gets.
const FIRST_SHIPPED: &[(&str, &str)] = &[("last_execution", "v0.2.7"), ("investigate_issue", "v0.2.9"), ("explain_route", "v0.2.9"), ("compare", "v0.2.9")];

const OK: u8 = 0;
const ERROR: u8 = 1;
const NOT_FOUND: u8 = 2;
const REGRESSION: u8 = 3;

/// A command's outcome other than success: the exit code and what to say.
#[derive(Debug)]
struct Failure {
    code: u8,
    message: String,
}

fn fail(code: u8, message: impl Into<String>) -> Failure {
    Failure { code, message: message.into() }
}

fn usage(message: impl Into<String>) -> Failure {
    fail(NOT_FOUND, format!("{}\n{USAGE}", message.into()))
}

type Outcome = Result<u8, Failure>;

/// The command's exit code, or None when `args` is not a client command.
pub fn run(args: &[&str]) -> Option<ExitCode> {
    let command = *args.first()?;
    if !COMMANDS.contains(&command) {
        return None;
    }
    let code = match main(command, &args[1..]) {
        Ok(code) => code,
        Err(f) => {
            eprintln!("railwatch: {}", f.message);
            f.code
        }
    };
    Some(ExitCode::from(code))
}

pub fn help() {
    println!("{USAGE}\n\n{HELP}");
}

// ---------------------------------------------------------------------------
// Arguments

/// Flags that take a value; every other `--x` is a switch.
const VALUED: &[&str] =
    &["url", "token", "app", "env", "window", "status", "sort", "limit", "kind", "before", "after", "since", "fail-if-worse", "interval", "slow", "arg"];

struct Opts {
    flags: Vec<(String, Option<String>)>,
    positional: Vec<String>,
}

impl Opts {
    fn parse(args: &[&str], allowed: &[&str]) -> Result<Opts, Failure> {
        const COMMON: &[&str] = &["url", "token", "app", "env", "json"];
        let mut o = Opts { flags: vec![], positional: vec![] };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let Some(flag) = a.strip_prefix("--") else {
                o.positional.push(a.to_string());
                continue;
            };
            let (name, inline) = match flag.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (flag, None),
            };
            if !COMMON.contains(&name) && !allowed.contains(&name) {
                return Err(usage(format!("unknown option --{name}")));
            }
            let value = match (VALUED.contains(&name), inline) {
                (true, Some(v)) => Some(v),
                (true, None) => Some(it.next().ok_or_else(|| usage(format!("--{name} needs a value")))?.to_string()),
                (false, Some(_)) => return Err(usage(format!("--{name} takes no value"))),
                (false, None) => None,
            };
            o.flags.push((name.to_string(), value));
        }
        Ok(o)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags.iter().rev().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }

    fn all(&self, name: &str) -> impl Iterator<Item = &str> {
        self.flags.iter().filter(move |(n, _)| n == name).filter_map(|(_, v)| v.as_deref())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }

    fn json(&self) -> bool {
        self.has("json")
    }

    /// At most `max` positional arguments.
    fn positional(&self, max: usize) -> Result<Vec<&str>, Failure> {
        if self.positional.len() > max {
            return Err(usage(format!("unexpected argument {}", self.positional[max])));
        }
        Ok(self.positional.iter().map(String::as_str).collect())
    }

    /// The application/environment every tool takes, from --app/--env.
    fn scope(&self) -> Map<String, Value> {
        let mut m = Map::new();
        if let Some(a) = self.get("app") {
            m.insert("application_slug".into(), a.into());
        }
        if let Some(e) = self.get("env") {
            m.insert("environment".into(), e.into());
        }
        m
    }

    /// `scope()` plus the given flags under their tool argument names.
    fn args(&self, pairs: &[(&str, &str)]) -> Map<String, Value> {
        let mut m = self.scope();
        for (flag, arg) in pairs {
            if let Some(v) = self.get(flag) {
                m.insert((*arg).into(), v.into());
            }
        }
        m
    }
}

fn main(command: &str, args: &[&str]) -> Outcome {
    if args.iter().any(|a| *a == "--help" || *a == "-h") {
        help();
        return Ok(OK);
    }
    match command {
        "login" => login(&Opts::parse(args, &["token-stdin"])?),
        "issues" => issues(&Opts::parse(args, &["status", "sort", "limit"])?),
        "issue" => issue(&Opts::parse(args, &[])?),
        "why" => why(&Opts::parse(args, &["window"])?),
        "last" => last(&Opts::parse(args, &["kind"])?),
        "compare" => compare(&Opts::parse(args, &["before", "after", "since", "kind", "window", "fail-if-worse"])?),
        "tail" => tail(&Opts::parse(args, &["errors", "slow", "interval"])?),
        "open" => open(&Opts::parse(args, &["window"])?),
        "tools" => tools(&Opts::parse(args, &[])?),
        "call" => call(&Opts::parse(args, &["arg"])?),
        _ => unreachable!("a client command"),
    }
}

// ---------------------------------------------------------------------------
// Connection

/// `~/.config/railwatch/client.toml` (XDG_CONFIG_HOME respected).
fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("railwatch").join("client.toml"))
}

#[derive(Default)]
struct Saved {
    url: Option<String>,
    token: Option<String>,
}

fn saved() -> Result<Saved, Failure> {
    let Some(path) = config_path() else { return Ok(Saved::default()) };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Saved::default()),
        Err(e) => return Err(fail(ERROR, format!("{}: {e}", path.display()))),
    };
    let t: toml::Table = text.parse().map_err(|e| fail(ERROR, format!("{}: {e}", path.display())))?;
    let s = |k: &str| t.get(k).and_then(|v| v.as_str()).filter(|v| !v.is_empty()).map(str::to_string);
    Ok(Saved { url: s("url"), token: s("token") })
}

/// `url` as an MCP endpoint: no trailing slash, ending in /mcp.
fn endpoint(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if u.ends_with("/mcp") { u.to_string() } else { format!("{u}/mcp") }
}

fn default_endpoint() -> String {
    let port = std::env::var("PORT").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "3000".into());
    let mount = std::env::var("RAILWATCH_SERVER_MOUNT_PATH").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "/railwatch".into());
    endpoint(&format!("http://localhost:{port}/{}", mount.trim_matches('/')))
}

fn env_token() -> Option<String> {
    let t = std::env::var("RAILWATCH_TOKEN").ok().filter(|t| !t.trim().is_empty())?;
    if t.starts_with("rw_") {
        eprintln!(
            "railwatch: ignoring RAILWATCH_TOKEN: it holds an ingest token (rw_...), which /mcp does not accept; pass --token with an API & MCP token (rwp_...)"
        );
        return None;
    }
    Some(t)
}

struct Client {
    endpoint: String,
    token: Option<String>,
    /// Where the endpoint came from, for the "cannot reach" message.
    source: &'static str,
    http: reqwest::blocking::Client,
    next_id: u64,
}

fn http_client() -> Result<reqwest::blocking::Client, Failure> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        .user_agent(format!("railwatch-cli/{}", railwatch_server::VERSION))
        .build()
        .map_err(|e| fail(ERROR, format!("http client: {e}")))
}

impl Client {
    fn connect(o: &Opts) -> Result<Client, Failure> {
        let saved = saved()?;
        let (url, source) = if let Some(u) = o.get("url") {
            (endpoint(u), "--url")
        } else if let Some(u) = std::env::var("RAILWATCH_URL").ok().filter(|u| !u.trim().is_empty()) {
            (endpoint(&u), "RAILWATCH_URL")
        } else if let Some(u) = &saved.url {
            (endpoint(u), "railwatch login")
        } else {
            (default_endpoint(), "default")
        };
        // The saved token belongs to the saved server: never send it elsewhere.
        let saved_token = saved.token.filter(|_| saved.url.as_deref().map(endpoint).as_deref() == Some(url.as_str()));
        let token = o.get("token").map(str::to_string).or_else(env_token).or(saved_token);
        Ok(Client { endpoint: url, token, source, http: http_client()?, next_id: 1 })
    }

    /// One JSON-RPC request; the `result`, or a Failure saying what went wrong.
    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, Rpc> {
        let id = self.next_id;
        self.next_id += 1;
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string());
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let res = req.send().map_err(|e| {
            let how = match self.source {
                "default" => " (the default for a Rails app's embedded server in development: pass --url, set RAILWATCH_URL, or run `railwatch login`)",
                _ => "",
            };
            Rpc::Failed(fail(ERROR, format!("cannot reach {}{how}: {}", self.endpoint, error_chain(&e))))
        })?;
        let status = res.status().as_u16();
        let body = res.text().unwrap_or_default();
        let parsed: Option<Value> = serde_json::from_str(&body).ok();
        match status {
            200..=299 => {}
            401 => {
                let hint = if self.token.is_some() {
                    "the token was refused (revoked, expired, or for another server)"
                } else {
                    "no token was sent: pass --token, set RAILWATCH_TOKEN, or `railwatch login --url URL --token T`"
                };
                return Err(Rpc::Failed(fail(
                    ERROR,
                    format!("401 unauthorized from {}: {hint}. Tokens are made at Settings > Profile > API & MCP tokens (scopes mcp and read).", self.endpoint),
                )));
            }
            403 => {
                let what = parsed.as_ref().map_or_else(|| first_chars(&body, 200), |v| v.to_string());
                return Err(Rpc::Failed(fail(ERROR, format!("403 from {}: {what} (the token needs the mcp and read scopes)", self.endpoint))));
            }
            404 => {
                let hint = match self.source {
                    "default" => {
                        " (the default for a Rails app's embedded server: is railwatch-server in its Gemfile? Set RAILWATCH_SERVER_MOUNT_PATH if it mounts elsewhere, or pass --url)"
                    }
                    _ => " (check the URL: the server's address, its base path, then /mcp)",
                };
                return Err(Rpc::Failed(fail(ERROR, format!("no Railwatch MCP endpoint at {}: 404{hint}", self.endpoint))));
            }
            429 => return Err(Rpc::Failed(fail(ERROR, format!("429 rate limited by {}; retry in a minute", self.endpoint)))),
            _ => return Err(Rpc::Failed(fail(ERROR, format!("HTTP {status} from {}: {}", self.endpoint, first_chars(&body, 200))))),
        }
        let Some(v) = parsed else {
            return Err(Rpc::Failed(fail(
                ERROR,
                format!("{} did not answer JSON (is it a Railwatch MCP endpoint?): {}", self.endpoint, first_chars(&body, 200)),
            )));
        };
        if let Some(e) = v.get("error") {
            let message = e.get("message").and_then(Value::as_str).unwrap_or("error").to_string();
            return Err(match e.get("code").and_then(Value::as_i64) {
                Some(-32601) => Rpc::UnknownTool(message),
                Some(-32602) => Rpc::Failed(fail(NOT_FOUND, message)),
                _ => Rpc::Failed(fail(ERROR, format!("{message} (from {})", self.endpoint))),
            });
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// tools/call: the result's text.
    fn call_raw(&mut self, tool: &str, args: &Map<String, Value>) -> Result<String, Rpc> {
        let result = self.rpc("tools/call", json!({"name": tool, "arguments": args}))?;
        let text = result.get("content").and_then(|c| c.get(0)).and_then(|c| c.get("text")).and_then(Value::as_str);
        match text {
            Some(t) => Ok(t.to_string()),
            None => Err(Rpc::Failed(fail(ERROR, format!("{tool}: the server's result has no text content: {}", first_chars(&result.to_string(), 200))))),
        }
    }

    /// The first of `tools` the server has, called with `args`: the tool's
    /// name and its parsed result. A server without any of them is the
    /// command needing a newer one.
    fn call_first(&mut self, command: &str, tools: &[&str], args: &Map<String, Value>) -> Result<(String, ToolResult), Failure> {
        for tool in tools {
            match self.call_raw(tool, args) {
                Ok(text) => return Ok((tool.to_string(), ToolResult::parse(text))),
                Err(Rpc::UnknownTool(_)) => continue,
                Err(Rpc::Failed(f)) => return Err(f),
            }
        }
        let wanted = tools[0];
        let version = FIRST_SHIPPED.iter().find(|(t, _)| *t == wanted).map_or("a newer release", |(_, v)| v);
        Err(fail(
            NOT_FOUND,
            format!(
                "`railwatch {command}` needs the {wanted} tool, which {} does not have: upgrade the server to railwatch-rs {version} or later (`railwatch tools` lists what it has)",
                self.endpoint
            ),
        ))
    }

    fn call(&mut self, command: &str, tool: &str, args: &Map<String, Value>) -> Result<ToolResult, Failure> {
        self.call_first(command, &[tool], args).map(|(_, r)| r)
    }

    /// The dashboard's root: the endpoint without /mcp.
    fn dashboard(&self) -> String {
        self.endpoint.trim_end_matches("/mcp").to_string()
    }
}

enum Rpc {
    /// -32601: the server has no such tool (or method).
    UnknownTool(String),
    Failed(Failure),
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        let m = inner.to_string();
        if !s.contains(&m) {
            s.push_str(": ");
            s.push_str(&m);
        }
        src = inner.source();
    }
    s
}

fn first_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

/// A tool's result: its raw text (for --json) and the JSON in it, with the
/// cap's {"truncated", "result"} wrapper taken off.
struct ToolResult {
    raw: String,
    value: Value,
    truncated: Option<Value>,
}

impl ToolResult {
    fn parse(raw: String) -> ToolResult {
        let v: Value = serde_json::from_str(&raw).unwrap_or_else(|_| Value::String(raw.clone()));
        let (value, truncated) = match v {
            Value::Object(mut m) if m.len() == 2 && m.contains_key("truncated") && m.contains_key("result") => {
                let t = m.remove("truncated");
                (m.remove("result").unwrap_or(Value::Null), t)
            }
            v => (v, None),
        };
        ToolResult { raw, value, truncated }
    }

    /// `{"error": ..., "hint": ...}`: the tool answered that the thing is
    /// not there, or the arguments were wrong.
    fn error(&self) -> Option<String> {
        let e = self.value.get("error")?.as_str()?;
        Some(match self.value.get("hint").and_then(Value::as_str) {
            Some(h) => format!("{e}\nhint: {h}"),
            None => e.to_string(),
        })
    }
}

/// Prints a result: raw with --json, else `text(value)`. A tool-level
/// error is exit 2 either way.
fn print(o: &Opts, r: &ToolResult, text: impl FnOnce(&Value) -> String) -> Outcome {
    if o.json() {
        println!("{}", r.raw);
        return Ok(if r.error().is_some() { NOT_FOUND } else { OK });
    }
    if let Some(e) = r.error() {
        return Err(fail(NOT_FOUND, e));
    }
    let mut out = match r.value.get("markdown").and_then(Value::as_str) {
        Some(md) => md.trim_end().to_string(),
        None => text(&r.value).trim_end().to_string(),
    };
    if let Some(t) = &r.truncated {
        let hint = t.get("hint").and_then(Value::as_str).unwrap_or("narrow the request");
        out.push_str(&format!("\n(result truncated by the server: {hint})"));
    }
    println!("{out}");
    Ok(OK)
}

// ---------------------------------------------------------------------------
// Commands

fn login(o: &Opts) -> Outcome {
    o.positional(0)?;
    let url = o.get("url").ok_or_else(|| usage("login needs --url (the server's address or its /mcp endpoint)"))?;
    let token = if o.has("token-stdin") {
        let mut t = String::new();
        std::io::stdin().read_to_string(&mut t).map_err(|e| fail(ERROR, format!("stdin: {e}")))?;
        Some(t.trim().to_string()).filter(|t| !t.is_empty())
    } else {
        o.get("token").map(str::to_string)
    };
    let mut c = Client { endpoint: endpoint(url), token: token.clone(), source: "--url", http: http_client()?, next_id: 1 };
    let tools = match c.rpc("tools/list", json!({})) {
        Ok(r) => r.get("tools").and_then(Value::as_array).map_or(0, Vec::len),
        Err(Rpc::Failed(f)) => return Err(fail(f.code, format!("{}\nNothing saved.", f.message))),
        Err(Rpc::UnknownTool(m)) => return Err(fail(ERROR, format!("{}: {m}; is it a Railwatch MCP endpoint? Nothing saved.", c.endpoint))),
    };
    let path = config_path().ok_or_else(|| fail(ERROR, "neither XDG_CONFIG_HOME nor HOME is set: nowhere to save"))?;
    let mut t = toml::Table::new();
    t.insert("url".into(), toml::Value::String(c.endpoint.clone()));
    if let Some(tok) = &token {
        t.insert("token".into(), toml::Value::String(tok.clone()));
    }
    write_private(&path, &t.to_string()).map_err(|e| fail(ERROR, format!("{}: {e}", path.display())))?;
    let auth = if token.is_some() { "with a token" } else { "without a token" };
    println!("{} answers {auth}: {tools} tools. Saved to {}.", c.endpoint, path.display());
    Ok(OK)
}

/// Writes `text` readable by the owner only (0600, the directory 0700).
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    // An existing file keeps its mode through open: tighten it.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(text.as_bytes())
}

fn issues(o: &Opts) -> Outcome {
    o.positional(0)?;
    let mut c = Client::connect(o)?;
    let mut args = o.args(&[("status", "status"), ("sort", "sort")]);
    if let Some(l) = o.get("limit") {
        args.insert("limit".into(), l.parse::<i64>().map_err(|_| usage("--limit must be a number"))?.into());
    }
    let status = o.get("status").unwrap_or("open").to_string();
    let r = c.call("issues", "list_issues", &args)?;
    print(o, &r, |v| render::issues(v, &status))
}

fn issue(o: &Opts) -> Outcome {
    let [key] = o.positional(1)?[..] else { return Err(usage("issue needs a key, e.g. railwatch issue SHOP-12")) };
    let mut c = Client::connect(o)?;
    let mut args = o.scope();
    args.insert("key".into(), key.into());
    let (tool, r) = c.call_first("issue", &["investigate_issue", "get_issue"], &args)?;
    print(o, &r, |v| if tool == "get_issue" { render::issue(v) } else { render::generic(v) })
}

/// The route name to look up: `target` itself when the tool finds it, else
/// the name of the newest request whose path, route or name contains it
/// (a path like /orders/12, or "GET /orders" without "(.:format)").
fn route_result(c: &mut Client, o: &Opts, command: &str, tools: &[&str], target: &str) -> Result<(String, ToolResult, Option<String>), Failure> {
    let mut args = o.args(&[("window", "window")]);
    args.insert("route".into(), target.into());
    let (tool, r) = c.call_first(command, tools, &args)?;
    if r.error().is_none_or(|e| !e.starts_with("route not found")) {
        return Ok((tool, r, None));
    }
    let mut last = o.scope();
    last.insert("path".into(), target.into());
    last.insert("kind".into(), "request".into());
    last.insert("timeline_limit".into(), 1.into());
    let newest = c.call(command, "last_execution", &last).ok();
    let Some(name) = newest.as_ref().and_then(|n| n.value.get("name")).and_then(Value::as_str).filter(|n| *n != target).map(str::to_string) else {
        return Ok((tool, r, None));
    };
    args.insert("route".into(), name.clone().into());
    let (tool, r) = c.call_first(command, tools, &args)?;
    Ok((tool, r, Some(name)))
}

fn why(o: &Opts) -> Outcome {
    let [target] = o.positional(1)?[..] else {
        return Err(usage("why needs a route or path, e.g. railwatch why \"GET /orders/:id\" or railwatch why /orders/12"));
    };
    let mut c = Client::connect(o)?;
    let (tool, r, resolved) = route_result(&mut c, o, "why", &["explain_route", "get_route"], target)?;
    print(o, &r, |v| {
        let head = resolved.map(|n| format!("({n}: the route of the newest request matching {target})\n")).unwrap_or_default();
        head + &if tool == "get_route" { render::route(v) } else { render::explain(v) }
    })
}

fn last(o: &Opts) -> Outcome {
    let p = o.positional(1)?;
    let mut c = Client::connect(o)?;
    let mut args = o.args(&[("kind", "kind")]);
    if let Some(path) = p.first() {
        args.insert("path".into(), (*path).into());
    }
    args.insert("timeline_limit".into(), 1.into());
    let r = c.call("last", "last_execution", &args)?;
    print(o, &r, render::execution)
}

fn compare(o: &Opts) -> Outcome {
    let p = o.positional(1)?;
    let threshold = match o.get("fail-if-worse") {
        Some(t) => Some(t.trim_end_matches('%').parse::<f64>().ok().filter(|t| *t >= 0.0).ok_or_else(|| usage("--fail-if-worse takes a percentage, e.g. 20"))?),
        None => None,
    };
    let mut c = Client::connect(o)?;
    let mut args = o.args(&[("before", "before"), ("after", "after"), ("since", "since"), ("kind", "kind"), ("window", "window")]);
    if let Some(route) = p.first() {
        args.insert("route".into(), (*route).into());
    }
    let r = c.call("compare", "compare", &args)?;
    let code = print(o, &r, render::compare)?;
    let Some(threshold) = threshold.filter(|_| code == OK) else { return Ok(code) };
    let Some(worst) = r.value.get("worst_regression_pct").and_then(Value::as_f64) else {
        return Err(fail(ERROR, "--fail-if-worse: the server's compare result has no worst_regression_pct; upgrade the server"));
    };
    if r.value.get("low_samples").and_then(Value::as_bool) == Some(true) {
        eprintln!("railwatch: compare: too few executions on one side to trust; not gating on {worst}%");
        return Ok(OK);
    }
    if worst > threshold {
        eprintln!("railwatch: compare: regression of {worst}% is over --fail-if-worse {threshold}%");
        return Ok(REGRESSION);
    }
    Ok(OK)
}

fn tail(o: &Opts) -> Outcome {
    let p = o.positional(1)?;
    let interval = match o.get("interval") {
        Some(s) => s.parse::<f64>().ok().filter(|s| *s > 0.0).ok_or_else(|| usage("--interval takes seconds, e.g. 2 or 0.5"))?,
        None => 2.0,
    };
    let mut q: Vec<String> = vec![];
    if let Some(path) = p.first() {
        q.push(format!("route:{path}"));
    }
    if o.has("errors") {
        q.push("status:5xx".into());
    }
    if let Some(ms) = o.get("slow") {
        q.push(format!("min_ms:{}", ms.parse::<u64>().map_err(|_| usage("--slow takes milliseconds"))?));
    }
    let mut c = Client::connect(o)?;
    let mut args = o.scope();
    args.insert("q".into(), q.join(" ").into());
    args.insert("window".into(), "1h".into());
    args.insert("limit".into(), 50.into());
    // The rows themselves (a filtered search is grouped by default).
    args.insert("shape".into(), "rows".into());
    let mut seen: (HashSet<String>, VecDeque<String>) = Default::default();
    let mut first = true;
    loop {
        let r = c.call("tail", "search_requests", &args)?;
        if let Some(e) = r.error() {
            return Err(fail(NOT_FOUND, e));
        }
        let rows = r.value.as_array().cloned().unwrap_or_default();
        // Newest first: print the new ones oldest first, and only the last
        // ten of what is already there.
        let mut fresh: Vec<&Value> = rows.iter().filter(|row| !seen.0.contains(row.get("execution_id").and_then(Value::as_str).unwrap_or(""))).collect();
        if first {
            fresh.truncate(10);
            first = false;
        }
        let mut out = std::io::stdout().lock();
        for row in fresh.into_iter().rev() {
            let id = row.get("execution_id").and_then(Value::as_str).unwrap_or("").to_string();
            if seen.0.insert(id.clone()) {
                seen.1.push_back(id);
                if seen.1.len() > 2_000
                    && let Some(old) = seen.1.pop_front()
                {
                    seen.0.remove(&old);
                }
            }
            let line = if o.json() { row.to_string() } else { render::request_line(row) };
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                return Ok(OK); // the reader went away (| head)
            }
        }
        drop(out);
        std::thread::sleep(Duration::from_secs_f64(interval));
    }
}

fn looks_like_execution_id(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12]) && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// SHOP-12, or account/application_id/SHOP-12.
fn looks_like_issue_key(s: &str) -> bool {
    let key = s.rsplit('/').next().unwrap_or(s);
    !s.starts_with('/')
        && !s.contains(' ')
        && key.rsplit_once('-').is_some_and(|(p, n)| !p.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn open(o: &Opts) -> Outcome {
    let p = o.positional(1)?;
    let mut c = Client::connect(o)?;
    let url = match p.first() {
        None => format!("{}/", c.dashboard()),
        Some(t) => {
            let r = if looks_like_execution_id(t) {
                let mut args = o.scope();
                args.insert("execution_id".into(), (*t).into());
                args.insert("timeline_limit".into(), 1.into());
                c.call("open", "get_execution", &args)?
            } else if looks_like_issue_key(t) {
                let mut args = o.scope();
                args.insert("key".into(), (*t).into());
                c.call("open", "get_issue", &args)?
            } else {
                route_result(&mut c, o, "open", &["get_route"], t)?.1
            };
            if let Some(e) = r.error() {
                return Err(fail(NOT_FOUND, e));
            }
            match r.value.get("url").and_then(Value::as_str) {
                // Paths when the server has no public url configured.
                Some(u) if u.starts_with('/') => format!("{}{u}", c.dashboard()),
                Some(u) => u.to_string(),
                None => return Err(fail(ERROR, format!("{t}: the server's result has no url"))),
            }
        }
    };
    if o.json() {
        println!("{}", json!({"url": url}));
    } else {
        println!("{url}");
    }
    if std::io::stdout().is_terminal() {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(&url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
    }
    Ok(OK)
}

fn tools(o: &Opts) -> Outcome {
    o.positional(0)?;
    let mut c = Client::connect(o)?;
    let r = match c.rpc("tools/list", json!({})) {
        Ok(r) => r,
        Err(Rpc::Failed(f)) => return Err(f),
        Err(Rpc::UnknownTool(m)) => return Err(fail(ERROR, format!("{}: {m}", c.endpoint))),
    };
    let list = r.get("tools").cloned().unwrap_or(Value::Array(vec![]));
    if o.json() {
        println!("{list}");
    } else {
        println!("{}", render::tools(&list).trim_end());
    }
    Ok(OK)
}

fn call(o: &Opts) -> Outcome {
    let [tool] = o.positional(1)?[..] else { return Err(usage("call needs a tool name; `railwatch tools` lists them")) };
    let mut c = Client::connect(o)?;
    let mut args = o.scope();
    for kv in o.all("arg") {
        let (k, v) = kv.split_once('=').ok_or_else(|| usage(format!("--arg {kv}: expected key=value")))?;
        args.insert(k.to_string(), serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.to_string())));
    }
    match c.call_raw(tool, &args) {
        Ok(text) => print(o, &ToolResult::parse(text), render::generic),
        Err(Rpc::UnknownTool(m)) => Err(fail(NOT_FOUND, format!("{m} on {}; `railwatch tools` lists the tools it has", c.endpoint))),
        Err(Rpc::Failed(f)) => Err(f),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_end_in_mcp_whatever_was_given() {
        assert_eq!(endpoint("https://rw.example.com"), "https://rw.example.com/mcp");
        assert_eq!(endpoint("https://rw.example.com/"), "https://rw.example.com/mcp");
        assert_eq!(endpoint("http://localhost:3000/monitoring"), "http://localhost:3000/monitoring/mcp");
        assert_eq!(endpoint("http://localhost:3000/monitoring/mcp/"), "http://localhost:3000/monitoring/mcp");
    }

    #[test]
    fn targets_are_told_apart() {
        assert!(looks_like_issue_key("SHOP-12") && looks_like_issue_key("acme/3/SHOP-12"));
        assert!(!looks_like_issue_key("GET /orders/:id") && !looks_like_issue_key("/orders/12") && !looks_like_issue_key("SHOP-"));
        assert!(looks_like_execution_id("ee416231-d074-46ff-a0b1-a0f27ba34095"));
        assert!(!looks_like_execution_id("REBULK-98") && !looks_like_execution_id("ee416231-d074-46ff-a0b1"));
    }

    #[test]
    fn options_parse_with_values_inline_or_next_and_refuse_unknown_ones() {
        let o = Opts::parse(&["/x", "--app", "shop", "--window=7d", "--json"], &["window"]).unwrap();
        assert_eq!((o.get("app"), o.get("window"), o.json(), o.positional.as_slice()), (Some("shop"), Some("7d"), true, &["/x".to_string()][..]));
        assert_eq!(Value::Object(o.args(&[("window", "window")])), json!({"application_slug": "shop", "window": "7d"}));
        assert_eq!(Opts::parse(&["--nope"], &[]).err().map(|f| f.code), Some(NOT_FOUND));
        assert_eq!(Opts::parse(&["--app"], &[]).err().map(|f| f.code), Some(NOT_FOUND));
        assert!(Opts::parse(&["--json=1"], &[]).is_err());
    }

    #[test]
    fn the_caps_wrapper_comes_off() {
        let r = ToolResult::parse(r#"{"truncated":{"hint":"use limit"},"result":[1,2]}"#.into());
        assert_eq!((r.value, r.truncated.map(|t| t["hint"].clone())), (json!([1, 2]), Some(json!("use limit"))));
        let e = ToolResult::parse(r#"{"error":"issue not found","hint":"Pass key"}"#.into());
        assert_eq!(e.error().as_deref(), Some("issue not found\nhint: Pass key"));
    }
}
