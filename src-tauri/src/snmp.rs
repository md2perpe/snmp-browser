//! Real SNMP GET / table-walk support, built on the `snmp2` crate.

use crate::mib::{ColumnInfo, TableInfo};
use serde::{Deserialize, Serialize};
use snmp2::{v3, Oid, SyncSession, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(4);
const MAX_REPETITIONS: u32 = 25;
/// Safety cap on walk iterations so a misbehaving agent can't hang the app forever.
const MAX_ITERATIONS: usize = 5000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionParams {
    pub host_addr: String,
    pub host_port: String,
    pub version: String,
    pub community: String,
    pub v3_user: String,
    pub v3_auth: String,
    pub v3_priv: String,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchResult {
    pub columns: Vec<String>,
    pub rows: Vec<HashMap<String, String>>,
    /// DISPLAY-HINT per column that has one (e.g. `"d-1"`), for columns whose raw
    /// value the frontend can optionally reformat instead of showing as-is.
    pub display_hints: HashMap<String, String>,
}

fn open_session(p: &ConnectionParams) -> Result<SyncSession, String> {
    let addr = format!("{}:{}", p.host_addr, p.host_port);
    match p.version.as_str() {
        "v1" => SyncSession::new_v1(&addr, p.community.as_bytes(), Some(TIMEOUT), 0).map_err(|e| e.to_string()),
        "v2c" => SyncSession::new_v2c(&addr, p.community.as_bytes(), Some(TIMEOUT), 0).map_err(|e| e.to_string()),
        "v3" => {
            // The UI only exposes user/auth/priv text fields (no algorithm picker), so we
            // default to the most broadly-compatible modern choices: SHA1 + AES-128. Auth
            // and/or privacy are skipped when their password field is left empty.
            let auth_mode = match (p.v3_auth.is_empty(), p.v3_priv.is_empty()) {
                (true, _) => v3::Auth::NoAuthNoPriv,
                (false, true) => v3::Auth::AuthNoPriv,
                (false, false) => v3::Auth::AuthPriv { cipher: v3::Cipher::Aes128, privacy_password: p.v3_priv.as_bytes().to_vec() },
            };
            let security = v3::Security::new(p.v3_user.as_bytes(), p.v3_auth.as_bytes())
                .with_auth_protocol(v3::AuthProtocol::Sha1)
                .with_auth(auth_mode);
            let mut sess = SyncSession::new_v3(&addr, Some(TIMEOUT), 0, security).map_err(|e| e.to_string())?;
            sess.init().map_err(|e| format!("failed to discover SNMPv3 engine ID: {e}"))?;
            Ok(sess)
        }
        other => Err(format!("unknown SNMP version '{other}'")),
    }
}

fn oid_from_dotted(s: &str) -> Result<Oid<'static>, String> {
    let arcs: Result<Vec<u64>, _> = s.split('.').filter(|p| !p.is_empty()).map(str::parse::<u64>).collect();
    let arcs = arcs.map_err(|_| format!("invalid OID '{s}'"))?;
    Oid::from(&arcs).map_err(|e| format!("invalid OID '{s}': {e:?}"))
}

fn format_octet_string(bytes: &[u8]) -> String {
    if !bytes.is_empty() && bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
    }
}

fn format_timeticks(v: u32) -> String {
    let cs = v % 100;
    let total_s = v / 100;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let total_h = total_m / 60;
    let h = total_h % 24;
    let d = total_h / 24;
    if d > 0 {
        format!("{d}d {h:02}:{m:02}:{s:02}.{cs:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{cs:02}")
    }
}

pub(crate) fn format_value(value: &Value, enum_values: &[(i64, String)]) -> String {
    match value {
        Value::Integer(n) => enum_values.iter().find(|(v, _)| v == n).map(|(_, name)| format!("{name}({n})")).unwrap_or_else(|| n.to_string()),
        Value::OctetString(bytes) => format_octet_string(bytes),
        Value::ObjectIdentifier(oid) => oid.to_id_string(),
        Value::IpAddress(b) => format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]),
        Value::Counter32(v) | Value::Unsigned32(v) => v.to_string(),
        Value::Timeticks(v) => format_timeticks(*v),
        Value::Counter64(v) => v.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Null => String::new(),
        Value::NoSuchObject => "(no such object)".to_string(),
        Value::NoSuchInstance => "(no such instance)".to_string(),
        Value::EndOfMibView => "(end of MIB view)".to_string(),
        Value::Opaque(b) => b.iter().map(|b| format!("{b:02x}")).collect(),
        other => format!("{other:?}"),
    }
}

pub fn fetch_scalar(params: &ConnectionParams, oid_str: &str) -> Result<FetchResult, String> {
    // A MIB scalar's own OID (as parsed from its OBJECT-TYPE assignment) names the
    // object, not an instance - SNMP requires the ".0" instance sub-identifier for a
    // GET to resolve, or agents report noSuchInstance on the bare object OID.
    let oid = oid_from_dotted(&format!("{oid_str}.0"))?;
    let mut sess = open_session(params)?;
    let pdu = sess.get(&oid).map_err(|e| e.to_string())?;
    let mut row = HashMap::new();
    if let Some((_, v)) = pdu.varbinds.clone().next() {
        row.insert("Value".to_string(), format_value(&v, &[]));
    }
    Ok(FetchResult { columns: vec!["Value".to_string()], rows: vec![row], display_hints: HashMap::new() })
}

pub fn fetch_table(params: &ConnectionParams, table: &TableInfo) -> Result<FetchResult, String> {
    if !table.resolved || table.oid.is_empty() {
        return Err("this table's OID could not be resolved (its ancestor chain isn't fully defined in the configured MIB directories)".into());
    }
    let base = oid_from_dotted(&table.oid)?;
    let base_len = base.iter().ok_or("invalid table OID")?.count();
    let mut sess = open_session(params)?;

    let by_arc: HashMap<u32, &ColumnInfo> = table.columns.iter().filter_map(|c| c.arc.map(|a| (a, c))).collect();
    let use_bulk = matches!(params.version.as_str(), "v2c" | "v3");

    let mut rows: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut row_order: Vec<String> = Vec::new();
    let mut current = base.clone();

    for _ in 0..MAX_ITERATIONS {
        let varbinds: Vec<(Oid<'_>, Value<'_>)> = if use_bulk {
            sess.getbulk(&[&current], 0, MAX_REPETITIONS).map_err(|e| e.to_string())?.varbinds.collect()
        } else {
            sess.getnext(&current).map_err(|e| e.to_string())?.varbinds.collect()
        };
        if varbinds.is_empty() {
            break;
        }

        let mut next_start: Option<Oid<'static>> = None;
        let mut finished = false;
        for (o, v) in &varbinds {
            if matches!(v, Value::EndOfMibView) || !o.starts_with(&base) {
                finished = true;
                break;
            }
            next_start = Some(o.to_owned());
            let Some(full) = o.iter().map(|it| it.collect::<Vec<u64>>()) else { continue };
            if full.len() < base_len + 2 {
                continue;
            }
            let suffix = &full[base_len..];
            let col_arc = suffix[1] as u32;
            let row_key = suffix[2..].iter().map(u64::to_string).collect::<Vec<_>>().join(".");
            let Some(col) = by_arc.get(&col_arc) else { continue };
            let formatted = format_value(v, &col.enum_values);
            if !rows.contains_key(&row_key) {
                row_order.push(row_key.clone());
            }
            rows.entry(row_key).or_default().insert(col.name.clone(), formatted);
        }
        if finished {
            break;
        }
        match next_start {
            Some(o) => current = o,
            None => break,
        }
    }

    let mut out_rows = Vec::with_capacity(row_order.len());
    for key in row_order {
        if let Some(mut r) = rows.remove(&key) {
            r.insert("Index".to_string(), key);
            out_rows.push(r);
        }
    }

    let mut columns = vec!["Index".to_string()];
    columns.extend(table.columns.iter().map(|c| c.name.clone()));

    let display_hints =
        table.columns.iter().filter_map(|c| c.display_hint.as_ref().map(|h| (c.name.clone(), h.clone()))).collect();

    Ok(FetchResult { columns, rows: out_rows, display_hints })
}

/// Result of one timed subtree walk, for the walk benchmark.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WalkTiming {
    /// Wall-clock time of the walk loop itself, in milliseconds. Session setup
    /// (including SNMPv3 engine discovery) happens before the clock starts, so
    /// repeated runs measure the same work.
    pub duration_ms: f64,
    /// Varbinds returned from inside the subtree; the one that walks past its
    /// end isn't counted.
    pub varbinds: usize,
    /// GETNEXT (v1) or GETBULK (v2c/v3) requests the walk needed.
    pub requests: usize,
    /// True when `MAX_ITERATIONS` cut the walk short, so the timing covers only
    /// part of the subtree.
    pub truncated: bool,
}

/// Walks everything under `oid_str` and times it, without collecting any
/// values - the benchmark only cares about how long the agent takes to serve
/// the subtree, not what's in it.
pub fn walk_timed(params: &ConnectionParams, oid_str: &str) -> Result<WalkTiming, String> {
    let base = oid_from_dotted(oid_str)?;
    let mut sess = open_session(params)?;
    let use_bulk = matches!(params.version.as_str(), "v2c" | "v3");

    let mut current = base.clone();
    let mut varbinds = 0usize;
    let mut requests = 0usize;
    let mut truncated = true;
    let started = Instant::now();

    for _ in 0..MAX_ITERATIONS {
        let received: Vec<(Oid<'_>, Value<'_>)> = if use_bulk {
            sess.getbulk(&[&current], 0, MAX_REPETITIONS).map_err(|e| e.to_string())?.varbinds.collect()
        } else {
            sess.getnext(&current).map_err(|e| e.to_string())?.varbinds.collect()
        };
        requests += 1;
        if received.is_empty() {
            truncated = false;
            break;
        }

        let mut next_start: Option<Oid<'static>> = None;
        let mut finished = false;
        for (o, v) in &received {
            if matches!(v, Value::EndOfMibView) || !o.starts_with(&base) {
                finished = true;
                break;
            }
            varbinds += 1;
            next_start = Some(o.to_owned());
        }
        if finished {
            truncated = false;
            break;
        }
        match next_start {
            Some(o) => current = o,
            None => {
                truncated = false;
                break;
            }
        }
    }

    Ok(WalkTiming { duration_ms: started.elapsed().as_secs_f64() * 1000.0, varbinds, requests, truncated })
}
