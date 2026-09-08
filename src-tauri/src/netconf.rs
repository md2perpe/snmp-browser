//! NETCONF client support (RFC 6241/6242), built on `russh` over SSH and `quick-xml` for framing
//! and message parsing - the SSH/XML-shaped counterpart to `gnmi.rs`'s gRPC/protobuf one. Password
//! authentication only, and every server host key is accepted without verification (see
//! `Client::check_server_key`) - the SSH-transport counterpart to gNMI's "Skip Verify" TLS mode
//! being the path of least friction for a browsing tool. No notifications yet.
//!
//! Three operations, sharing one connect+`<hello>`+single-RPC pipeline (`exchange()`):
//! - `get()` - a one-shot `<get>`, filtered by an XPath `select` expression built directly from
//!   the same `module-name:node-name`-qualified path a YANG tree node already carries (see
//!   `yang.rs`) - the same "browse the YANG tree, fetch by its path" flow gNMI uses, just carried
//!   over NETCONF's own filter mechanism. That requires each module-name qualifier in the path to
//!   be bound to its real XML namespace URI via an `xmlns:` declaration, which is why `get()`
//!   takes the active YANG profile's `module_namespaces` map (see `yang::YangParseResult`) -
//!   unlike gNMI, whose target resolves module-qualified path segments against its own loaded
//!   schema without any namespace plumbing from this app. A target that doesn't advertise the
//!   `:xpath` capability can still be browsed at the root (path `"/"` or empty, which omits the
//!   filter and fetches everything).
//! - `edit_config()` - sends a caller-supplied `<config>` payload as an `<edit-config>` against a
//!   chosen target datastore (`running` or `candidate`), with an optional default-operation.
//! - `raw_rpc()` - wraps a caller-supplied inner XML fragment directly in `<rpc>...</rpc>` and
//!   sends it verbatim. This is deliberately how this app reaches a YANG-1.1 `<action>`, `<commit/>`,
//!   `<validate>`, `<discard-changes/>`, `<lock>`/`<unlock>`, or any vendor RPC: modeling `rpc`/
//!   `action`/`input` YANG statements into a proper schema-driven form (the way `get()`'s path is
//!   driven by the parsed data-node tree) would need a substantial extension to `yang.rs`, which
//!   doesn't parse those statements at all today. A raw-XML passthrough gets real work done now
//!   without that upfront cost; a schema-driven form is a natural follow-up if it's worth it.
//!
//! `edit_config()` and `raw_rpc()` mutate the target, unlike `get()` - the frontend is expected to
//! confirm with the user before calling either.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use russh::client::{self, Msg};
use russh::keys::PublicKeyOrCertificate;
use russh::{Channel, ChannelMsg, Disconnect};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetconfConnectionParams {
    pub host_addr: String,
    pub host_port: String,
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetconfCapabilities {
    pub session_id: String,
    pub capabilities: Vec<String>,
}

/// One element of a decoded NETCONF `<get>` reply tree - a local (unqualified) element name, its
/// text value (present on leaves), and any children. Mirrors `gnmi::GnmiNode`'s shape.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct NetconfNode {
    pub name: String,
    pub value: Option<String>,
    pub children: Vec<NetconfNode>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetconfTree {
    pub roots: Vec<NetconfNode>,
    pub timestamp: i64,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[derive(Clone, Copy)]
enum Framing {
    /// RFC 6242 "old" framing: a message is terminated by the literal byte sequence `]]>]]>`.
    /// Always used for the `<hello>` exchange itself, and for everything else when either side
    /// doesn't advertise `urn:ietf:params:netconf:base:1.1`.
    Eom,
    /// RFC 6242 chunked framing (`base:1.1`): `\n#<size>\n<size bytes>` chunks terminated by
    /// `\n##\n`. Only a single chunk is ever sent - the message sizes this app deals with don't
    /// need splitting, and RFC 6242 doesn't require more than one.
    Chunked,
}

struct Client;

impl client::Handler for Client {
    type Error = russh::Error;

    /// Phase 1 has no known-hosts-style trust store, so every server key is accepted - see this
    /// module's doc comment.
    async fn check_server_key(&mut self, _server_public_key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Opens the SSH connection, authenticates, opens a channel, and starts the `netconf` subsystem.
/// Returns the still-open session handle (must be kept alive for the channel to stay usable), the
/// channel, and any bytes the target already sent before the subsystem-start confirmation arrived
/// (some targets start streaming the `<hello>` right away) - these belong to the caller's first
/// `read_message` call, not to this function's own bookkeeping.
async fn connect(p: &NetconfConnectionParams) -> Result<(client::Handle<Client>, Channel<Msg>, Vec<u8>), String> {
    let port: u16 = p.host_port.parse().map_err(|_| format!("invalid port '{}'", p.host_port))?;
    let config = Arc::new(client::Config::default());

    let mut session = tokio::time::timeout(CONNECT_TIMEOUT, client::connect(config, (p.host_addr.clone(), port), Client))
        .await
        .map_err(|_| format!("timed out connecting to {}:{}", p.host_addr, port))?
        .map_err(|e| format!("failed to connect to {}:{}: {e}", p.host_addr, port))?;

    let auth = tokio::time::timeout(REQUEST_TIMEOUT, session.authenticate_password(p.username.clone(), p.password.clone()))
        .await
        .map_err(|_| "timed out authenticating".to_string())?
        .map_err(|e| format!("authentication failed: {e}"))?;
    if !auth.success() {
        return Err("authentication failed".to_string());
    }

    let mut channel = session.channel_open_session().await.map_err(|e| format!("failed to open an SSH channel: {e}"))?;
    channel
        .request_subsystem(true, "netconf")
        .await
        .map_err(|e| format!("failed to request the 'netconf' SSH subsystem: {e}"))?;

    let mut leftover = Vec::new();
    loop {
        let event = tokio::time::timeout(REQUEST_TIMEOUT, channel.wait())
            .await
            .map_err(|_| "timed out waiting for the 'netconf' subsystem to start".to_string())?
            .ok_or_else(|| "SSH channel closed before the 'netconf' subsystem started".to_string())?;
        match event {
            ChannelMsg::Success => break,
            ChannelMsg::Failure => return Err("target rejected the 'netconf' SSH subsystem request".to_string()),
            ChannelMsg::Data { data } => leftover.extend_from_slice(&data),
            ChannelMsg::Eof | ChannelMsg::Close => return Err("SSH channel closed before the 'netconf' subsystem started".to_string()),
            _ => {}
        }
    }

    Ok((session, channel, leftover))
}

async fn close(session: client::Handle<Client>, channel: Channel<Msg>) {
    let _ = channel.close().await;
    let _ = session.disconnect(Disconnect::ByApplication, "", "English").await;
}

fn client_hello_xml() -> &'static str {
    concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        r#"<hello xmlns="urn:ietf:params:xml:ns:netconf:base:1.0">"#,
        r#"<capabilities>"#,
        r#"<capability>urn:ietf:params:netconf:base:1.0</capability>"#,
        r#"<capability>urn:ietf:params:netconf:base:1.1</capability>"#,
        r#"</capabilities>"#,
        r#"</hello>"#,
    )
}

fn frame_message(framing: Framing, xml: &str) -> Vec<u8> {
    match framing {
        Framing::Eom => {
            let mut bytes = xml.as_bytes().to_vec();
            bytes.extend_from_slice(b"]]>]]>");
            bytes
        }
        Framing::Chunked => format!("\n#{}\n{xml}\n##\n", xml.len()).into_bytes(),
    }
}

async fn write_message(channel: &Channel<Msg>, framing: Framing, xml: &str) -> Result<(), String> {
    channel.data_bytes(frame_message(framing, xml)).await.map_err(|e| format!("failed to send NETCONF message: {e}"))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Tries to parse one complete chunked-framing message (RFC 6242 §4.2) from the front of `buf`.
/// Returns `Ok(None)` when `buf` doesn't yet hold a complete message (the caller should read more
/// and retry), the assembled message plus how many leading bytes of `buf` it consumed on success,
/// or `Err` for framing that's outright malformed (not just incomplete).
fn try_parse_chunked(buf: &[u8]) -> Result<Option<(Vec<u8>, usize)>, String> {
    let mut pos = 0usize;
    let mut out = Vec::new();
    loop {
        if buf.len() < pos + 2 {
            return Ok(None);
        }
        if &buf[pos..pos + 2] != b"\n#" {
            return Err("malformed NETCONF chunked-framing message".to_string());
        }
        pos += 2;
        if buf.get(pos) == Some(&b'#') {
            pos += 1;
            if buf.len() < pos + 1 {
                return Ok(None);
            }
            if buf[pos] != b'\n' {
                return Err("malformed NETCONF end-of-chunks marker".to_string());
            }
            pos += 1;
            return Ok(Some((out, pos)));
        }
        let digits_start = pos;
        while buf.get(pos).map(u8::is_ascii_digit).unwrap_or(false) {
            pos += 1;
        }
        if pos == digits_start {
            return Err("malformed NETCONF chunk size".to_string());
        }
        if buf.len() < pos + 1 {
            return Ok(None);
        }
        if buf[pos] != b'\n' {
            return Err("malformed NETCONF chunk framing (missing newline after chunk size)".to_string());
        }
        let size: usize =
            std::str::from_utf8(&buf[digits_start..pos]).ok().and_then(|s| s.parse().ok()).ok_or("invalid NETCONF chunk size")?;
        pos += 1;
        if buf.len() < pos + size {
            return Ok(None);
        }
        out.extend_from_slice(&buf[pos..pos + size]);
        pos += size;
    }
}

/// Reads one complete framed message from `channel`, consuming and updating `leftover` (bytes
/// already read from the channel that haven't been claimed by a previous message).
async fn read_message(channel: &mut Channel<Msg>, framing: Framing, leftover: &mut Vec<u8>) -> Result<Vec<u8>, String> {
    loop {
        match framing {
            Framing::Eom => {
                if let Some(pos) = find_subslice(leftover, b"]]>]]>") {
                    let msg = leftover[..pos].to_vec();
                    leftover.drain(..pos + 6);
                    return Ok(msg);
                }
            }
            Framing::Chunked => {
                if let Some((msg, consumed)) = try_parse_chunked(leftover)? {
                    leftover.drain(..consumed);
                    return Ok(msg);
                }
            }
        }
        let event = tokio::time::timeout(REQUEST_TIMEOUT, channel.wait())
            .await
            .map_err(|_| "timed out waiting for a NETCONF response".to_string())?
            .ok_or_else(|| "SSH channel closed before a complete NETCONF message was received".to_string())?;
        match event {
            ChannelMsg::Data { data } => leftover.extend_from_slice(&data),
            ChannelMsg::Eof | ChannelMsg::Close => {
                return Err("SSH channel closed before a complete NETCONF message was received".to_string())
            }
            _ => {}
        }
    }
}

fn local_name(e: &BytesStart) -> String {
    e.name().local_name().into_inner().to_string()
}

fn parse_hello(xml: &str) -> Result<(Vec<String>, String), String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut capabilities = Vec::new();
    let mut session_id = String::new();
    let mut stack: Vec<String> = Vec::new();
    loop {
        match reader.read_event().map_err(|e| format!("failed to parse NETCONF hello: {e}"))? {
            Event::Start(e) => stack.push(local_name(&e)),
            Event::End(_) => {
                stack.pop();
            }
            Event::Text(t) => {
                let text = t.into_inner().trim().to_string();
                if text.is_empty() {
                    continue;
                }
                match stack.last().map(String::as_str) {
                    Some("capability") => capabilities.push(text),
                    Some("session-id") => session_id = text,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok((capabilities, session_id))
}

/// Scans a path for `prefix:` qualifiers (e.g. the `org-openroadm-device` in
/// `org-openroadm-device:circuit-packs`), skipping over quoted predicate values so a value like
/// `[name='eth0:1']` doesn't get misread as a qualifier. Order-preserving and de-duplicated.
fn collect_module_prefixes(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = path.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => {
                quote = Some(b);
                i += 1;
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-') {
                    i += 1;
                }
                if bytes.get(i) == Some(&b':') {
                    let prefix = path[start..i].to_string();
                    if !out.contains(&prefix) {
                        out.push(prefix);
                    }
                }
            }
            _ => i += 1,
        }
    }
    out
}

fn xml_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Builds the `<rpc><get>...</get></rpc>` message for `path`. `path` "" or "/" fetches the whole
/// datastore (no filter); anything else needs the target's `:xpath` capability, since that's the
/// only NETCONF filter type this app can build directly from a YANG-tree path without a
/// module-by-module subtree walk (see this module's doc comment).
fn get_rpc_xml(path: &str, xpath_supported: bool, module_namespaces: &HashMap<String, String>) -> Result<String, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return Ok(r#"<rpc message-id="1" xmlns="urn:ietf:params:xml:ns:netconf:base:1.0"><get/></rpc>"#.to_string());
    }
    if !xpath_supported {
        return Err(
            "target doesn't advertise the NETCONF :xpath capability needed to filter by path - use \"/\" to fetch the whole datastore"
                .to_string(),
        );
    }
    let mut xmlns = String::new();
    for prefix in collect_module_prefixes(trimmed) {
        let uri = module_namespaces
            .get(&prefix)
            .ok_or_else(|| format!("unknown YANG module '{prefix}' in path - no matching module found in the active YANG profile's directories"))?;
        xmlns.push_str(&format!(r#" xmlns:{prefix}="{}""#, xml_escape_attr(uri)));
    }
    Ok(format!(
        r#"<rpc message-id="1" xmlns="urn:ietf:params:xml:ns:netconf:base:1.0"><get><filter type="xpath" select="{}"{xmlns}/></get></rpc>"#,
        xml_escape_attr(trimmed)
    ))
}

/// Builds the full element tree of an `<rpc-reply>` message and returns that `<rpc-reply>` node
/// itself - its content varies by operation (`<data>` for `get`, `<ok/>` for most others). An
/// `<rpc-error>` among its direct children is surfaced as `Err` instead.
fn parse_rpc_reply(xml: &str) -> Result<NetconfNode, String> {
    struct Frame {
        name: String,
        children: Vec<NetconfNode>,
        text: String,
    }

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<Frame> = vec![Frame { name: String::new(), children: Vec::new(), text: String::new() }];
    loop {
        match reader.read_event().map_err(|e| format!("failed to parse NETCONF response: {e}"))? {
            Event::Start(e) => stack.push(Frame { name: local_name(&e), children: Vec::new(), text: String::new() }),
            Event::Empty(e) => {
                let frame = stack.last_mut().ok_or("unbalanced NETCONF response XML")?;
                frame.children.push(NetconfNode { name: local_name(&e), value: None, children: Vec::new() });
            }
            Event::Text(t) => {
                stack.last_mut().ok_or("unbalanced NETCONF response XML")?.text.push_str(&t.into_inner());
            }
            Event::End(_) => {
                let frame = stack.pop().ok_or("unbalanced NETCONF response XML")?;
                let value = if frame.children.is_empty() {
                    let trimmed = frame.text.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                } else {
                    None
                };
                let node = NetconfNode { name: frame.name, value, children: frame.children };
                stack.last_mut().ok_or("unbalanced NETCONF response XML")?.children.push(node);
            }
            Event::Eof => break,
            _ => {}
        }
    }

    let root = stack.into_iter().next().ok_or("empty NETCONF response")?;
    let rpc_reply = root.children.into_iter().find(|n| n.name == "rpc-reply").ok_or("no <rpc-reply> in NETCONF response")?;
    if let Some(err) = rpc_reply.children.iter().find(|n| n.name == "rpc-error") {
        let message = err
            .children
            .iter()
            .find(|n| n.name == "error-message")
            .and_then(|n| n.value.clone())
            .unwrap_or_else(|| "target returned a NETCONF rpc-error".to_string());
        return Err(message);
    }
    Ok(rpc_reply)
}

/// Returns the children of a `<get>` reply's `<data>` element (an `<rpc-error>` reply is surfaced
/// as `Err`, via `parse_rpc_reply`).
fn parse_get_reply(xml: &str) -> Result<Vec<NetconfNode>, String> {
    let rpc_reply = parse_rpc_reply(xml)?;
    let data = rpc_reply.children.into_iter().find(|n| n.name == "data").ok_or("no <data> in NETCONF response")?;
    Ok(data.children)
}

/// Confirms a reply carries no `<rpc-error>`, for operations (`edit-config`, `raw_rpc`) whose
/// successful reply is just `<ok/>` (or operation-specific data not worth modeling as a tree).
fn ensure_rpc_ok(xml: &str) -> Result<(), String> {
    parse_rpc_reply(xml).map(|_| ())
}

/// Confirms `xml` is well-formed by parsing it wrapped in a throwaway root element - used to
/// reject a malformed `edit_config`/`raw_rpc` payload locally with a clear message, rather than
/// sending broken XML to the target and getting back an opaque failure (or, worse, a NETCONF
/// server that's lenient about framing but not content, and does something unintended with it).
fn validate_well_formed_fragment(xml: &str) -> Result<(), String> {
    let trimmed = xml.trim();
    if trimmed.is_empty() {
        return Err("payload is empty".to_string());
    }
    let wrapped = format!("<root>{trimmed}</root>");
    let mut reader = Reader::from_str(&wrapped);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(e) => return Err(format!("malformed XML: {e}")),
        }
    }
}

/// Builds the `<rpc><edit-config>...</edit-config></rpc>` message. `target` must be `"running"` or
/// `"candidate"`; `default_operation`, if given, must be `"merge"`, `"replace"`, or `"none"` (the
/// only values RFC 6241 §7.2 defines). `config_xml` becomes the content of `<config>` verbatim -
/// see this module's doc comment for why it's taken as raw XML rather than built from a schema.
fn edit_config_rpc_xml(target: &str, default_operation: Option<&str>, config_xml: &str) -> Result<String, String> {
    let target_elem = match target {
        "running" | "candidate" => target,
        other => return Err(format!("unknown edit-config target '{other}' - expected \"running\" or \"candidate\"")),
    };
    validate_well_formed_fragment(config_xml)?;
    let default_operation_xml = match default_operation {
        None => String::new(),
        Some(op @ ("merge" | "replace" | "none")) => format!("<default-operation>{op}</default-operation>"),
        Some(other) => return Err(format!("unknown default-operation '{other}' - expected \"merge\", \"replace\", or \"none\"")),
    };
    Ok(format!(
        r#"<rpc message-id="1" xmlns="urn:ietf:params:xml:ns:netconf:base:1.0"><edit-config><target><{target_elem}/></target>{default_operation_xml}<config>{}</config></edit-config></rpc>"#,
        config_xml.trim()
    ))
}

/// Builds the `<rpc>{inner_xml}</rpc>` message for `raw_rpc()`.
fn raw_rpc_xml(inner_xml: &str) -> Result<String, String> {
    validate_well_formed_fragment(inner_xml)?;
    Ok(format!(r#"<rpc message-id="1" xmlns="urn:ietf:params:xml:ns:netconf:base:1.0">{}</rpc>"#, inner_xml.trim()))
}

pub async fn capabilities(params: &NetconfConnectionParams) -> Result<NetconfCapabilities, String> {
    let (session, mut channel, mut leftover) = connect(params).await?;
    write_message(&channel, Framing::Eom, client_hello_xml()).await?;
    let hello = read_message(&mut channel, Framing::Eom, &mut leftover).await?;
    let hello_xml = String::from_utf8(hello).map_err(|e| format!("NETCONF hello was not valid UTF-8: {e}"))?;
    let (capabilities, session_id) = parse_hello(&hello_xml)?;
    close(session, channel).await;
    Ok(NetconfCapabilities { session_id, capabilities })
}

/// Runs one connect + `<hello>` exchange + single-RPC round trip against `params`, returning the
/// raw `<rpc-reply>` XML text. `build_rpc` receives the target's negotiated capabilities (so
/// `get()` can check for `:xpath` support) and returns the `<rpc>...</rpc>` XML to send; an `Err`
/// from it aborts (after politely closing the connection) before anything is sent.
async fn exchange(params: &NetconfConnectionParams, build_rpc: impl FnOnce(&[String]) -> Result<String, String>) -> Result<String, String> {
    let (session, mut channel, mut leftover) = connect(params).await?;
    write_message(&channel, Framing::Eom, client_hello_xml()).await?;
    let hello = read_message(&mut channel, Framing::Eom, &mut leftover).await?;
    let hello_xml = String::from_utf8(hello).map_err(|e| format!("NETCONF hello was not valid UTF-8: {e}"))?;
    let (capabilities, _session_id) = parse_hello(&hello_xml)?;

    let framing = if capabilities.iter().any(|c| c.contains("base:1.1")) { Framing::Chunked } else { Framing::Eom };

    let rpc_xml = match build_rpc(&capabilities) {
        Ok(xml) => xml,
        Err(e) => {
            close(session, channel).await;
            return Err(e);
        }
    };
    write_message(&channel, framing, &rpc_xml).await?;
    let reply = read_message(&mut channel, framing, &mut leftover).await?;
    let reply_xml = String::from_utf8(reply).map_err(|e| format!("NETCONF response was not valid UTF-8: {e}"))?;
    close(session, channel).await;
    Ok(reply_xml)
}

pub async fn get(params: &NetconfConnectionParams, path: &str, module_namespaces: &HashMap<String, String>) -> Result<NetconfTree, String> {
    let reply_xml = exchange(params, |capabilities| {
        let xpath_supported = capabilities.iter().any(|c| c.contains("capability:xpath"));
        get_rpc_xml(path, xpath_supported, module_namespaces)
    })
    .await?;
    Ok(NetconfTree { roots: parse_get_reply(&reply_xml)?, timestamp: now_ms() })
}

/// Sends `config_xml` as an `<edit-config>` against `target` ("running" or "candidate"), with an
/// optional `default_operation` ("merge"/"replace"/"none"). Returns the raw `<rpc-reply>` XML on
/// success (typically just `<ok/>`) for display - see this module's doc comment for why the
/// payload is raw XML rather than schema-built, and why the caller should confirm with the user
/// first, since unlike `get()` this mutates the target's configuration.
pub async fn edit_config(
    params: &NetconfConnectionParams,
    target: &str,
    default_operation: Option<&str>,
    config_xml: &str,
) -> Result<String, String> {
    let reply_xml = exchange(params, |_capabilities| edit_config_rpc_xml(target, default_operation, config_xml)).await?;
    ensure_rpc_ok(&reply_xml)?;
    Ok(reply_xml)
}

/// Wraps `inner_xml` in `<rpc>...</rpc>` and sends it verbatim - the target may interpret it as a
/// YANG-1.1 `<action>`, `<commit/>`, `<validate>`, a vendor RPC, or anything else the target
/// accepts. Returns the raw `<rpc-reply>` XML on success. Like `edit_config()`, this can mutate
/// the target - see this module's doc comment.
pub async fn raw_rpc(params: &NetconfConnectionParams, inner_xml: &str) -> Result<String, String> {
    let reply_xml = exchange(params, |_capabilities| raw_rpc_xml(inner_xml)).await?;
    ensure_rpc_ok(&reply_xml)?;
    Ok(reply_xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_framing_round_trips_through_write_and_parse() {
        let framed = frame_message(Framing::Chunked, "hello world");
        let (msg, consumed) = try_parse_chunked(&framed).unwrap().expect("a fully-framed message should parse in one pass");
        assert_eq!(msg, b"hello world");
        assert_eq!(consumed, framed.len());
    }

    #[test]
    fn chunked_framing_reports_incomplete_rather_than_erroring() {
        let framed = frame_message(Framing::Chunked, "hello world");
        assert_eq!(try_parse_chunked(&framed[..framed.len() - 3]).unwrap(), None);
    }

    #[test]
    fn eom_framing_finds_the_delimiter_and_leaves_no_trailing_bytes_consumed_twice() {
        let mut leftover = b"<hello/>]]>]]>".to_vec();
        let pos = find_subslice(&leftover, b"]]>]]>").unwrap();
        assert_eq!(&leftover[..pos], b"<hello/>");
        leftover.drain(..pos + 6);
        assert!(leftover.is_empty());
    }

    #[test]
    fn collects_module_prefixes_while_ignoring_a_colon_inside_a_quoted_value() {
        let prefixes = collect_module_prefixes("/oc-if:interfaces/oc-if:interface[oc-if:name='eth0:1']");
        assert_eq!(prefixes, vec!["oc-if".to_string()]);
    }

    #[test]
    fn root_path_omits_the_filter_even_without_xpath_support() {
        let xml = get_rpc_xml("/", false, &HashMap::new()).unwrap();
        assert!(xml.contains("<get/>"));
        assert!(!xml.contains("filter"));
    }

    #[test]
    fn a_qualified_path_without_xpath_support_is_rejected() {
        assert!(get_rpc_xml("/oc-if:interfaces", false, &HashMap::new()).is_err());
    }

    #[test]
    fn a_qualified_path_binds_its_modules_namespace() {
        let mut ns = HashMap::new();
        ns.insert("oc-if".to_string(), "urn:example:oc-if".to_string());
        let xml = get_rpc_xml("/oc-if:interfaces", true, &ns).unwrap();
        assert!(xml.contains(r#"xmlns:oc-if="urn:example:oc-if""#));
        assert!(xml.contains(r#"select="/oc-if:interfaces""#));
    }

    #[test]
    fn parses_server_hello_capabilities_and_session_id() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hello xmlns="urn:ietf:params:xml:ns:netconf:base:1.0">
  <capabilities>
    <capability>urn:ietf:params:netconf:base:1.0</capability>
    <capability>urn:ietf:params:netconf:base:1.1</capability>
    <capability>urn:ietf:params:netconf:capability:xpath:1.0</capability>
  </capabilities>
  <session-id>42</session-id>
</hello>"#;
        let (caps, session_id) = parse_hello(xml).unwrap();
        assert_eq!(session_id, "42");
        assert_eq!(caps.len(), 3);
        assert!(caps.iter().any(|c| c.contains("base:1.1")));
    }

    #[test]
    fn parses_a_get_reply_into_a_node_tree() {
        let xml = r#"<rpc-reply xmlns="urn:ietf:params:xml:ns:netconf:base:1.0" message-id="1">
  <data>
    <interfaces xmlns="urn:example:oc-if">
      <interface>
        <name>eth0</name>
        <state>
          <enabled>true</enabled>
        </state>
      </interface>
    </interfaces>
  </data>
</rpc-reply>"#;
        let roots = parse_get_reply(xml).unwrap();
        assert_eq!(roots.len(), 1);
        let interfaces = &roots[0];
        assert_eq!(interfaces.name, "interfaces");
        let interface = &interfaces.children[0];
        assert_eq!(interface.name, "interface");
        let name = interface.children.iter().find(|n| n.name == "name").unwrap();
        assert_eq!(name.value.as_deref(), Some("eth0"));
        let state = interface.children.iter().find(|n| n.name == "state").unwrap();
        let enabled = state.children.iter().find(|n| n.name == "enabled").unwrap();
        assert_eq!(enabled.value.as_deref(), Some("true"));
    }

    #[test]
    fn an_rpc_error_reply_is_surfaced_as_err() {
        let xml = r#"<rpc-reply xmlns="urn:ietf:params:xml:ns:netconf:base:1.0" message-id="1">
  <rpc-error>
    <error-type>application</error-type>
    <error-tag>invalid-value</error-tag>
    <error-message>no such element</error-message>
  </rpc-error>
</rpc-reply>"#;
        let err = parse_get_reply(xml).unwrap_err();
        assert_eq!(err, "no such element");
    }

    #[test]
    fn ensure_rpc_ok_accepts_a_plain_ok_reply() {
        let xml = r#"<rpc-reply xmlns="urn:ietf:params:xml:ns:netconf:base:1.0" message-id="1"><ok/></rpc-reply>"#;
        assert!(ensure_rpc_ok(xml).is_ok());
    }

    #[test]
    fn ensure_rpc_ok_surfaces_an_rpc_error_the_same_way_as_get() {
        let xml = r#"<rpc-reply xmlns="urn:ietf:params:xml:ns:netconf:base:1.0" message-id="1">
  <rpc-error>
    <error-type>protocol</error-type>
    <error-tag>operation-failed</error-tag>
    <error-message>candidate datastore is locked</error-message>
  </rpc-error>
</rpc-reply>"#;
        assert_eq!(ensure_rpc_ok(xml).unwrap_err(), "candidate datastore is locked");
    }

    #[test]
    fn validate_well_formed_fragment_rejects_an_unclosed_tag() {
        assert!(validate_well_formed_fragment("<system><hostname>router1</hostname>").is_err());
    }

    #[test]
    fn validate_well_formed_fragment_rejects_an_empty_payload() {
        assert!(validate_well_formed_fragment("   ").is_err());
    }

    #[test]
    fn validate_well_formed_fragment_accepts_multiple_top_level_elements() {
        // <config> (and an <rpc> body) can legally hold more than one top-level element, unlike a
        // normal XML document - the throwaway <root> wrapper exists precisely to allow that.
        assert!(validate_well_formed_fragment("<a/><b/>").is_ok());
    }

    #[test]
    fn edit_config_rpc_xml_rejects_an_unknown_target() {
        assert!(edit_config_rpc_xml("startup", None, "<a/>").is_err());
    }

    #[test]
    fn edit_config_rpc_xml_rejects_an_unknown_default_operation() {
        assert!(edit_config_rpc_xml("running", Some("delete"), "<a/>").is_err());
    }

    #[test]
    fn edit_config_rpc_xml_rejects_a_malformed_config_payload() {
        assert!(edit_config_rpc_xml("running", None, "<a><b></a>").is_err());
    }

    #[test]
    fn edit_config_rpc_xml_builds_the_expected_message() {
        let xml = edit_config_rpc_xml("candidate", Some("merge"), r#"<system xmlns="urn:example"><hostname>router1</hostname></system>"#).unwrap();
        assert!(xml.contains("<target><candidate/></target>"));
        assert!(xml.contains("<default-operation>merge</default-operation>"));
        assert!(xml.contains(r#"<config><system xmlns="urn:example"><hostname>router1</hostname></system></config>"#));
    }

    #[test]
    fn edit_config_rpc_xml_omits_default_operation_when_not_given() {
        let xml = edit_config_rpc_xml("running", None, "<a/>").unwrap();
        assert!(!xml.contains("default-operation"));
    }

    #[test]
    fn raw_rpc_xml_wraps_the_payload_in_rpc_verbatim() {
        let xml = raw_rpc_xml("<commit/>").unwrap();
        assert_eq!(xml, r#"<rpc message-id="1" xmlns="urn:ietf:params:xml:ns:netconf:base:1.0"><commit/></rpc>"#);
    }

    #[test]
    fn raw_rpc_xml_rejects_malformed_input() {
        assert!(raw_rpc_xml("<commit>").is_err());
    }
}
