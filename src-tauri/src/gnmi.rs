//! gNMI client support (Capabilities + one-shot Get), built on `tonic`/`prost` against the
//! vendored openconfig `gnmi.proto`. Phase 1 only: no YANG-driven path tree, no `Subscribe`
//! (streaming) and no `Set`. A future `Subscribe` implementation is expected to mirror
//! `trap.rs`'s background-thread + ring-buffer + poll pattern, since both are long-lived,
//! server-pushed streams that this app's frontend can only pull from on an interval.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tonic::Request;

/// tonic applies no connect timeout by default, so an unreachable-but-not-actively-refusing
/// target (a firewall silently dropping packets, a dead IP on a live subnet) would otherwise
/// hang for the OS's TCP connect timeout - a minute or more - rather than failing promptly.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounds the RPC itself, in case the connection succeeds but the target never replies.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Generated protobuf/gRPC code for the `gnmi` and `gnmi_ext` packages, mirroring the
/// hand-written-glue-wraps-generated-code split `mib.rs` uses around `tree-sitter`.
pub mod pb {
    pub mod gnmi_ext {
        include!(concat!(env!("OUT_DIR"), "/gnmi_ext.rs"));
    }
    pub mod gnmi {
        include!(concat!(env!("OUT_DIR"), "/gnmi.rs"));
    }
}

use pb::gnmi::{
    g_nmi_client::GNmiClient, get_request, typed_value, CapabilityRequest, Encoding, GetRequest, Path, PathElem, TypedValue,
};

/// Accepts a gNMI target's TLS certificate without verifying it - only reachable when the user
/// explicitly picks "Skip Verify" for a target with a self-signed or otherwise unverifiable
/// cert, which is common enough for gNMI devices that it's a real Phase 1 requirement.
mod insecure_tls {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, Error, SignatureScheme};
    use std::sync::Arc;

    #[derive(Debug)]
    struct SkipServerVerification(CryptoProvider);

    pub(crate) fn verifier() -> Arc<dyn ServerCertVerifier> {
        Arc::new(SkipServerVerification(rustls::crypto::ring::default_provider()))
    }

    impl ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
        }

        fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GnmiConnectionParams {
    pub host_addr: String,
    pub host_port: String,
    /// "insecure" (plaintext), "tls" (verified against `ca_cert_path`), or "tlsSkipVerify".
    pub tls_mode: String,
    pub ca_cert_path: Option<String>,
    pub client_cert_path: Option<String>,
    pub client_key_path: Option<String>,
    /// Sent as gNMI's conventional "username"/"password" gRPC metadata, not TLS client auth.
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GnmiModelData {
    pub name: String,
    pub organization: String,
    pub version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GnmiCapabilities {
    pub gnmi_version: String,
    pub supported_encodings: Vec<String>,
    pub supported_models: Vec<GnmiModelData>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GnmiNode {
    pub name: String,
    pub value: Option<String>,
    pub children: Vec<GnmiNode>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GnmiTree {
    pub roots: Vec<GnmiNode>,
    pub timestamp: i64,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

async fn connect(p: &GnmiConnectionParams) -> Result<GNmiClient<Channel>, String> {
    let scheme = if p.tls_mode == "insecure" { "http" } else { "https" };
    let uri = format!("{scheme}://{}:{}", p.host_addr, p.host_port);
    let mut endpoint = Endpoint::from_shared(uri)
        .map_err(|e| format!("invalid target '{}:{}': {e}", p.host_addr, p.host_port))?
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT);

    endpoint = match p.tls_mode.as_str() {
        "insecure" => endpoint,
        "tls" => {
            let ca_path = p.ca_cert_path.as_deref().filter(|s| !s.is_empty()).ok_or(
                "TLS mode requires a CA certificate path - use \"Skip Verify\" instead if the target's cert can't be verified against a CA",
            )?;
            let pem = std::fs::read(ca_path).map_err(|e| format!("failed to read CA cert '{ca_path}': {e}"))?;
            let tls = ClientTlsConfig::new().domain_name(p.host_addr.clone()).ca_certificate(Certificate::from_pem(pem));
            endpoint.tls_config(tls).map_err(|e| format!("TLS configuration error: {e}"))?
        }
        "tlsSkipVerify" => {
            let tls = ClientTlsConfig::new().domain_name(p.host_addr.clone());
            endpoint.tls_config_with_verifier(tls, insecure_tls::verifier()).map_err(|e| format!("TLS configuration error: {e}"))?
        }
        other => return Err(format!("unknown TLS mode '{other}'")),
    };

    let channel = endpoint.connect().await.map_err(|e| format!("failed to connect to {}:{}: {e}", p.host_addr, p.host_port))?;
    Ok(GNmiClient::new(channel))
}

/// Wraps `msg` in a `Request`, attaching the gNMI username/password metadata when configured.
fn build_request<T>(msg: T, p: &GnmiConnectionParams) -> Result<Request<T>, String> {
    let mut req = Request::new(msg);
    if let Some(user) = p.username.as_deref().filter(|s| !s.is_empty()) {
        req.metadata_mut().insert("username", MetadataValue::try_from(user).map_err(|e| format!("invalid username: {e}"))?);
    }
    if let Some(pass) = p.password.as_deref().filter(|s| !s.is_empty()) {
        req.metadata_mut().insert("password", MetadataValue::try_from(pass).map_err(|e| format!("invalid password: {e}"))?);
    }
    Ok(req)
}

pub async fn capabilities(params: &GnmiConnectionParams) -> Result<GnmiCapabilities, String> {
    let mut client = connect(params).await?;
    let req = build_request(CapabilityRequest { extension: Vec::new() }, params)?;
    let resp = client.capabilities(req).await.map_err(|e| format!("Capabilities RPC failed: {e}"))?.into_inner();

    Ok(GnmiCapabilities {
        gnmi_version: resp.g_nmi_version,
        supported_encodings: resp.supported_encodings.iter().filter_map(|i| Encoding::try_from(*i).ok()).map(|e| e.as_str_name().to_string()).collect(),
        supported_models: resp.supported_models.into_iter().map(|m| GnmiModelData { name: m.name, organization: m.organization, version: m.version }).collect(),
    })
}

/// Parses a gNMI xpath-style path (e.g. `/interfaces/interface[name=eth0]/state`) into a `Path`.
/// A small hand-rolled parser is enough for Phase 1 - no need for a full grammar.
/// Splits a path into its `/`-separated elements, but ignores `/` (and quote characters) found
/// inside a `[...]` predicate - a key value very commonly contains a literal `/`, e.g.
/// `circuit-packs[circuit-pack-name='1/AWG']`, and naively splitting on every `/` breaks that
/// predicate in half.
fn split_path_elements(path: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let bytes = path.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            continue;
        }
        match b {
            b'\'' | b'"' => quote = Some(b),
            b'[' => depth += 1,
            b']' => depth -= 1,
            b'/' if depth == 0 => {
                if i > start {
                    parts.push(&path[start..i]);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < path.len() {
        parts.push(&path[start..]);
    }
    parts
}

/// Finds the `]` matching the `[` at `open`, skipping over any `]` inside a quoted value.
fn find_matching_bracket(raw: &str, open: usize) -> Result<usize, String> {
    let bytes = raw.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = open + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else {
            match b {
                b'\'' | b'"' => quote = Some(b),
                b']' => return Ok(i),
                _ => {}
            }
        }
        i += 1;
    }
    Err(format!("unterminated '[' in path element '{raw}'"))
}

/// Strips one layer of matching surrounding quotes from a predicate value, e.g. `'1/AWG'` -> `1/AWG`.
pub(crate) fn unquote(v: &str) -> String {
    let bytes = v.as_bytes();
    if bytes.len() >= 2 && (bytes[0] == b'\'' || bytes[0] == b'"') && bytes[bytes.len() - 1] == bytes[0] {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

fn parse_path(path: &str) -> Result<Path, String> {
    let mut elems = Vec::new();
    for raw in split_path_elements(path) {
        let mut name = String::new();
        let mut key = HashMap::new();
        let mut i = 0;
        while i < raw.len() {
            if raw.as_bytes()[i] == b'[' {
                let end = find_matching_bracket(raw, i)?;
                let (k, v) = raw[i + 1..end].split_once('=').ok_or_else(|| format!("expected key=value in '{}'", &raw[i + 1..end]))?;
                key.insert(k.to_string(), unquote(v));
                i = end + 1;
            } else {
                let next = raw[i..].find('[').map(|o| i + o).unwrap_or(raw.len());
                name.push_str(&raw[i..next]);
                i = next;
            }
        }
        elems.push(PathElem { name, key });
    }
    Ok(Path { elem: elems, ..Default::default() })
}

/// The inverse of `parse_path` for one element, used to label a result tree's nodes.
fn render_elem(e: &PathElem) -> String {
    if e.key.is_empty() {
        return e.name.clone();
    }
    let mut keys: Vec<_> = e.key.iter().collect();
    keys.sort_by(|a, b| a.0.cmp(b.0));
    let preds: String = keys.iter().map(|(k, v)| format!("[{k}={v}]")).collect();
    format!("{}{preds}", e.name)
}

fn find_or_create<'a>(nodes: &'a mut Vec<GnmiNode>, name: &str) -> &'a mut GnmiNode {
    match nodes.iter().position(|n| n.name == name) {
        Some(i) => &mut nodes[i],
        None => {
            nodes.push(GnmiNode { name: name.to_string(), value: None, children: Vec::new() });
            nodes.last_mut().unwrap()
        }
    }
}

fn json_scalar_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Merges a JSON value (from a `json_val`/`json_ietf_val` leaf) into `node`'s subtree - an
/// object's/array's members become child nodes rather than one opaque JSON blob, since a Get
/// against a container path very commonly comes back JSON-encoded and the whole point of a
/// browser is to let the user drill into it.
fn merge_json(node: &mut GnmiNode, value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                merge_json(find_or_create(&mut node.children, k), v);
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                merge_json(find_or_create(&mut node.children, &i.to_string()), v);
            }
        }
        other => node.value = Some(json_scalar_to_string(other)),
    }
}

fn format_scalar_typed_value(v: &TypedValue) -> String {
    match &v.value {
        Some(typed_value::Value::StringVal(s)) => s.clone(),
        Some(typed_value::Value::IntVal(n)) => n.to_string(),
        Some(typed_value::Value::UintVal(n)) => n.to_string(),
        Some(typed_value::Value::BoolVal(b)) => b.to_string(),
        Some(typed_value::Value::BytesVal(b)) => b.iter().map(|b| format!("{b:02x}")).collect(),
        #[allow(deprecated)]
        Some(typed_value::Value::FloatVal(f)) => f.to_string(),
        Some(typed_value::Value::DoubleVal(f)) => f.to_string(),
        #[allow(deprecated)]
        Some(typed_value::Value::DecimalVal(d)) => format!("{}e-{}", d.digits, d.precision),
        Some(typed_value::Value::LeaflistVal(arr)) => arr.element.iter().map(format_scalar_typed_value).collect::<Vec<_>>().join(", "),
        Some(typed_value::Value::AnyVal(_)) => "(protobuf Any value)".to_string(),
        Some(typed_value::Value::AsciiVal(s)) => s.clone(),
        Some(typed_value::Value::ProtoBytes(b)) => b.iter().map(|b| format!("{b:02x}")).collect(),
        Some(typed_value::Value::JsonVal(_)) | Some(typed_value::Value::JsonIetfVal(_)) => String::new(),
        None => String::new(),
    }
}

/// Attaches an update's value to its already-located leaf node - exploding it into child nodes
/// for a JSON-encoded leaf, or formatting it as a single scalar value otherwise.
fn attach_value(node: &mut GnmiNode, val: Option<&TypedValue>) {
    let Some(val) = val else { return };
    if let Some(typed_value::Value::JsonVal(b) | typed_value::Value::JsonIetfVal(b)) = &val.value {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(b) {
            merge_json(node, &parsed);
            return;
        }
    }
    node.value = Some(format_scalar_typed_value(val));
}

fn insert_update(mut nodes: &mut Vec<GnmiNode>, elems: &[PathElem], val: Option<&TypedValue>) {
    let Some((last, ancestors)) = elems.split_last() else { return };
    for e in ancestors {
        nodes = &mut find_or_create(nodes, &render_elem(e)).children;
    }
    attach_value(find_or_create(nodes, &render_elem(last)), val);
}

pub async fn get(params: &GnmiConnectionParams, path_str: &str) -> Result<GnmiTree, String> {
    let path = parse_path(path_str)?;
    let mut client = connect(params).await?;
    let req = build_request(
        GetRequest {
            prefix: None,
            path: vec![path],
            r#type: get_request::DataType::All as i32,
            encoding: Encoding::JsonIetf as i32,
            use_models: Vec::new(),
            extension: Vec::new(),
        },
        params,
    )?;
    let resp = client.get(req).await.map_err(|e| format!("Get RPC failed: {e}"))?.into_inner();

    let mut roots: Vec<GnmiNode> = Vec::new();
    for notif in &resp.notification {
        let prefix_elems = notif.prefix.as_ref().map(|p| p.elem.as_slice()).unwrap_or(&[]);
        for update in &notif.update {
            let path_elems = update.path.as_ref().map(|p| p.elem.as_slice()).unwrap_or(&[]);
            if prefix_elems.is_empty() && path_elems.is_empty() {
                continue;
            }
            let full: Vec<PathElem> = prefix_elems.iter().chain(path_elems).cloned().collect();
            insert_update(&mut roots, &full, update.val.as_ref());
        }
    }
    Ok(GnmiTree { roots, timestamp: now_ms() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_key_value_containing_a_slash() {
        let path =
            "/org-openroadm-device:org-openroadm-device/org-openroadm-device:circuit-packs[org-openroadm-device:circuit-pack-name='1/AWG']/org-openroadm-device:ports[org-openroadm-device:port-name='9210']";
        let parsed = parse_path(path).expect("path should parse");
        assert_eq!(parsed.elem.len(), 3);
        assert_eq!(parsed.elem[1].name, "org-openroadm-device:circuit-packs");
        assert_eq!(
            parsed.elem[1].key.get("org-openroadm-device:circuit-pack-name").map(String::as_str),
            Some("1/AWG")
        );
        assert_eq!(parsed.elem[2].name, "org-openroadm-device:ports");
        assert_eq!(parsed.elem[2].key.get("org-openroadm-device:port-name").map(String::as_str), Some("9210"));
    }

    #[test]
    fn parses_unquoted_key_value() {
        let parsed = parse_path("/interfaces/interface[name=eth0]/state").expect("path should parse");
        assert_eq!(parsed.elem.len(), 3);
        assert_eq!(parsed.elem[1].key.get("name").map(String::as_str), Some("eth0"));
    }

    #[test]
    fn rejects_a_truly_unterminated_bracket() {
        assert!(parse_path("/interfaces/interface[name=eth0").is_err());
    }

    #[test]
    fn render_elem_roundtrips_through_parse_path() {
        let elem = PathElem { name: "circuit-packs".to_string(), key: HashMap::from([("circuit-pack-name".to_string(), "1/AWG".to_string())]) };
        let rendered = render_elem(&elem);
        let reparsed = parse_path(&format!("/{rendered}")).expect("re-rendered path should parse");
        assert_eq!(reparsed.elem[0].key.get("circuit-pack-name").map(String::as_str), Some("1/AWG"));
    }
}
