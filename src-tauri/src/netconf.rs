//! NETCONF client support (RFC 6241/6242), built on `russh` over SSH and `quick-xml` for framing
//! and message parsing - the SSH/XML-shaped counterpart to `gnmi.rs`'s gRPC/protobuf one. Phase 1
//! only, mirroring gnmi.rs's own scoping: `<hello>` capability exchange and a one-shot `<get>`,
//! no `<get-config>`/`<edit-config>` and no notifications. Password authentication only, and
//! every server host key is accepted without verification (see `Client::check_server_key`) -
//! the SSH-transport counterpart to gNMI's "Skip Verify" TLS mode being the path of least
//! friction for a Phase 1 browsing tool.
//!
//! A NETCONF `<get>` is filtered by an XPath `select` expression built directly from the same
//! `module-name:node-name`-qualified path a YANG tree node already carries (see `yang.rs`) - the
//! same "browse the YANG tree, fetch by its path" flow gNMI uses, just carried over NETCONF's own
//! filter mechanism. That requires each module-name qualifier in the path to be bound to its real
//! XML namespace URI via an `xmlns:` declaration, which is why `get()` takes the active YANG
//! profile's `module_namespaces` map (see `yang::YangParseResult`) - unlike gNMI, whose target
//! resolves module-qualified path segments against its own loaded schema without any namespace
//! plumbing from this app. A target that doesn't advertise the `:xpath` capability can still be
//! browsed at the root (path `"/"` or empty, which omits the filter and fetches everything).

use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::{BytesRef, BytesStart, Event};
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

/// Resolves an `Event::GeneralRef` (`&name;` or `&#<number>;`) to the text it stands for, falling
/// back to the reference written out literally (e.g. `&custom;`) for anything that isn't a
/// character reference or one of the five predefined XML entities - this app has no DTD to
/// resolve custom entities against.
fn resolve_general_ref(r: &BytesRef) -> Result<String, String> {
    if let Some(c) = r.resolve_char_ref().map_err(|e| format!("invalid character reference: {e}"))? {
        return Ok(c.to_string());
    }
    if let Some(s) = resolve_xml_entity(r) {
        return Ok(s.to_string());
    }
    Ok(format!("&{};", &**r))
}

fn parse_hello(xml: &str) -> Result<(Vec<String>, String), String> {
    let mut reader = Reader::from_str(xml);
    // Not `trim_text(true)`: that trims each individual `Event::Text` fragment, which would eat
    // whitespace bordering an entity/char reference (its own separate `Event::GeneralRef`, split
    // out of the surrounding text). Trimming only the fully-accumulated `text` per element (below)
    // gets the same "ignore incidental whitespace" behavior without that side effect.
    let mut capabilities = Vec::new();
    let mut session_id = String::new();
    let mut stack: Vec<String> = Vec::new();
    let mut text = String::new();
    loop {
        match reader.read_event().map_err(|e| format!("failed to parse NETCONF hello: {e}"))? {
            Event::Start(e) => {
                stack.push(local_name(&e));
                text.clear();
            }
            Event::End(_) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    match stack.last().map(String::as_str) {
                        Some("capability") => capabilities.push(trimmed.to_string()),
                        Some("session-id") => session_id = trimmed.to_string(),
                        _ => {}
                    }
                }
                text.clear();
                stack.pop();
            }
            Event::Text(t) => text.push_str(&t.into_inner()),
            Event::GeneralRef(r) => text.push_str(&resolve_general_ref(&r)?),
            Event::CData(c) => text.push_str(&c.into_inner()),
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
pub(crate) fn get_rpc_xml(path: &str, xpath_supported: bool, module_namespaces: &HashMap<String, String>) -> Result<String, String> {
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

/// Builds the full element tree of an `<rpc-reply>` message, then returns the children of its
/// `<data>` element (an `<rpc-error>` reply is surfaced as `Err` instead).
fn parse_get_reply(xml: &str) -> Result<Vec<NetconfNode>, String> {
    struct Frame {
        name: String,
        children: Vec<NetconfNode>,
        text: String,
    }

    let mut reader = Reader::from_str(xml);
    // See `parse_hello`'s comment on why this doesn't use `trim_text(true)`: `frame.text` is
    // trimmed as a whole once fully accumulated (below), instead of trimming each `Event::Text`
    // fragment individually and eating whitespace next to an `Event::GeneralRef`.
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
            Event::GeneralRef(r) => {
                let resolved = resolve_general_ref(&r)?;
                stack.last_mut().ok_or("unbalanced NETCONF response XML")?.text.push_str(&resolved);
            }
            Event::CData(c) => {
                stack.last_mut().ok_or("unbalanced NETCONF response XML")?.text.push_str(&c.into_inner());
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
    let data = rpc_reply.children.into_iter().find(|n| n.name == "data").ok_or("no <data> in NETCONF response")?;
    Ok(data.children)
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

pub async fn get(params: &NetconfConnectionParams, path: &str, module_namespaces: &HashMap<String, String>) -> Result<NetconfTree, String> {
    let (session, mut channel, mut leftover) = connect(params).await?;
    write_message(&channel, Framing::Eom, client_hello_xml()).await?;
    let hello = read_message(&mut channel, Framing::Eom, &mut leftover).await?;
    let hello_xml = String::from_utf8(hello).map_err(|e| format!("NETCONF hello was not valid UTF-8: {e}"))?;
    let (capabilities, _session_id) = parse_hello(&hello_xml)?;

    let framing = if capabilities.iter().any(|c| c.contains("base:1.1")) { Framing::Chunked } else { Framing::Eom };
    let xpath_supported = capabilities.iter().any(|c| c.contains("capability:xpath"));

    let rpc_xml = match get_rpc_xml(path, xpath_supported, module_namespaces) {
        Ok(xml) => xml,
        Err(e) => {
            close(session, channel).await;
            return Err(e);
        }
    };
    write_message(&channel, framing, &rpc_xml).await?;
    let reply = read_message(&mut channel, framing, &mut leftover).await?;
    let reply_xml = String::from_utf8(reply).map_err(|e| format!("NETCONF response was not valid UTF-8: {e}"))?;
    let roots = parse_get_reply(&reply_xml);
    close(session, channel).await;
    Ok(NetconfTree { roots: roots?, timestamp: now_ms() })
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
    fn get_reply_values_preserve_entity_and_char_references() {
        let xml = r#"<rpc-reply xmlns="urn:ietf:params:xml:ns:netconf:base:1.0" message-id="1">
  <data>
    <description>Port 1 &amp; 2 &lt;uplink&gt; &#65;&#x42;</description>
  </data>
</rpc-reply>"#;
        let roots = parse_get_reply(xml).unwrap();
        assert_eq!(roots[0].value.as_deref(), Some("Port 1 & 2 <uplink> AB"));
    }

    #[test]
    fn get_reply_values_preserve_cdata_content() {
        let xml = r#"<rpc-reply xmlns="urn:ietf:params:xml:ns:netconf:base:1.0" message-id="1">
  <data>
    <config><![CDATA[<not-an-element>&stays-literal]]></config>
  </data>
</rpc-reply>"#;
        let roots = parse_get_reply(xml).unwrap();
        assert_eq!(roots[0].value.as_deref(), Some("<not-an-element>&stays-literal"));
    }

    #[test]
    fn hello_capabilities_preserve_entity_references() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hello xmlns="urn:ietf:params:xml:ns:netconf:base:1.0">
  <capabilities>
    <capability>urn:example:a&amp;b</capability>
  </capabilities>
  <session-id>1</session-id>
</hello>"#;
        let (caps, _) = parse_hello(xml).unwrap();
        assert_eq!(caps, vec!["urn:example:a&b".to_string()]);
    }
}
