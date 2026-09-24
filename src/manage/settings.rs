//! Read-only form projection. Config remains the only semantic validator and
//! Manager remains the only owner of file validation, revisions and application.
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::config::Config;

const MAX_CONFIG: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Preview {
    pub toml: String,
    pub changes: Map<String, Value>,
}

/// Preserve the source document, including comments and relative paths.
pub(super) fn parse(text: &str) -> Result<Value> {
    ensure!(text.len() <= MAX_CONFIG, "configuration exceeds 256 KiB");
    let config = Config::parse(text)?;
    let source: toml::Value = toml::from_str(text)?;
    let mut settings = serde_json::to_value(&config)?;
    // Policy is a compiled trie, not a document. Project its already-validated
    // source rules without keeping a second rule copy in the DNS runtime.
    let mut filter = json!({
        "enabled": false, "block_exact": [], "block_suffix": [],
        "allow_exact": [], "allow_suffix": []
    });
    if let Some(rules) = source.get("filter").and_then(toml::Value::as_table) {
        for (key, value) in rules {
            filter[key] = serde_json::to_value(value)?;
        }
    }
    settings["filter"] = filter;
    Ok(json!({"settings": settings, "toml": text}))
}

/// Preview is pure: no filesystem reads, listener binding or saved-state access.
/// Null deletes an optional value, objects merge and arrays replace. Rendering
/// normalizes TOML formatting/comments but preserves untouched configuration.
pub(super) fn preview(request: Preview) -> Result<Value> {
    let mut projection = parse(&request.toml)?;
    // Retain nulls for schema validation so even deleting an unknown key is an
    // error. Optional fields accept null; required fields cannot be deleted.
    overlay(&mut projection["settings"], request.changes.clone(), false);
    serde_json::from_value::<Config>(projection["settings"].clone())?;
    let original: toml::Value = toml::from_str(&request.toml)?;
    let mut document = serde_json::to_value(original)?;
    overlay(&mut document, request.changes, true);
    let candidate = toml::to_string_pretty(&toml::Value::try_from(document)?)?;
    parse(&candidate)
}

fn overlay(target: &mut Value, changes: Map<String, Value>, delete_null: bool) {
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let target = target.as_object_mut().expect("object established above");
    for (key, value) in changes {
        match value {
            Value::Null if delete_null => {
                target.remove(&key);
            }
            Value::Object(changes) => overlay(
                target.entry(key).or_insert(Value::Null),
                changes,
                delete_null,
            ),
            Value::Array(mut values) if delete_null => {
                // Config's schema has already accepted these optional nulls.
                // TOML has no null representation, including within rule arrays.
                for value in &mut values {
                    omit_null_fields(value);
                }
                target.insert(key, Value::Array(values));
            }
            value => {
                target.insert(key, value);
            }
        }
    }
}

fn omit_null_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|_, value| !value.is_null());
            for value in fields.values_mut() {
                omit_null_fields(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                omit_null_fields(value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "# keep this comment\nlisten='127.0.0.1:5353'\nupstreams={servers=['udp://1.1.1.1:53']}\nquery_timeout_ms=1000\ntcp_io_timeout_ms=1000\nshutdown_grace_ms=500\nmax_inflight=128\nmax_tcp_connections=32\n";

    fn change(text: &str, changes: Value) -> Result<Value> {
        preview(serde_json::from_value(
            json!({"toml":text,"changes":changes}),
        )?)
    }

    #[test]
    fn projection_returns_defaults_and_optional_null_without_rewriting_source() {
        let result = parse(BASE).unwrap();
        assert_eq!(result["toml"], BASE);
        assert_eq!(
            result["settings"]["upstreams"]["servers"],
            json!(["udp://1.1.1.1:53"])
        );
        assert_eq!(result["settings"]["cache"]["max_bytes"], 8 * 1024 * 1024);
        assert_eq!(result["settings"]["source_limits"]["rate_per_sec"], 100);
        assert_eq!(result["settings"]["ecs"]["enabled"], false);
        assert_eq!(result["settings"]["coalescing"]["enabled"], true);
        assert_eq!(result["settings"]["filter"]["block_exact"], json!([]));
        for key in ["dot", "doh", "doq"] {
            assert_eq!(result["settings"][key], Value::Null);
        }
    }

    #[test]
    fn upstream_edits_preserve_servers_and_validate_required_fields() {
        let untouched = change(BASE, json!({"cache":{"enabled":false}})).unwrap();
        let source: toml::Value = toml::from_str(untouched["toml"].as_str().unwrap()).unwrap();
        assert_eq!(
            source["upstreams"]["servers"][0].as_str(),
            Some("udp://1.1.1.1:53")
        );

        let edited = change(BASE, json!({"upstreams":{"prefer_h3":true}})).unwrap();
        assert_eq!(
            edited["settings"]["upstreams"]["servers"],
            json!(["udp://1.1.1.1:53"])
        );
        assert_eq!(edited["settings"]["upstreams"]["prefer_h3"], true);
        assert!(change(BASE, json!({"upstreams":null})).is_err());
        assert!(change(BASE, json!({"upstreams":{"unknown":null}})).is_err());
        assert!(change(BASE, json!({"upstreams":{"prefer_h3":null}})).is_err());
    }

    #[test]
    fn removed_upstream_fields_are_rejected_in_preview() {
        for key in [
            "upstream",
            "upstream_tls",
            "scheduler",
            "upstream_pool",
            "doh3",
        ] {
            assert!(change(BASE, json!({key: null})).is_err(), "{key}");
        }
    }

    #[test]
    fn partial_changes_preserve_advanced_budgets_and_relative_paths_without_io() {
        let text = format!(
            "{BASE}filter_file='rules/missing.toml'\n[dot]\nlisten='127.0.0.1:853'\ncert_file='missing-cert.pem'\nkey_file='missing-key.pem'\n[cache]\nmax_bytes=16777216\nmax_variants=17\n[source_limits]\nburst=99\nmax_sources=100\n[metrics]\ninterval_secs=20\n"
        );
        let result = change(
            &text,
            json!({"cache":{"enabled":false},"source_limits":{"rate_per_sec":25}}),
        )
        .unwrap();
        let settings = &result["settings"];
        assert_eq!(settings["cache"]["enabled"], false);
        assert_eq!(settings["cache"]["max_bytes"], 16777216);
        assert_eq!(settings["cache"]["max_variants"], 17);
        assert_eq!(settings["source_limits"]["rate_per_sec"], 25);
        assert_eq!(settings["source_limits"]["burst"], 99);
        assert_eq!(settings["source_limits"]["max_sources"], 100);
        assert_eq!(settings["metrics"]["interval_secs"], 20);
        assert_eq!(settings["filter_file"], "rules/missing.toml");
        assert_eq!(settings["dot"]["cert_file"], "missing-cert.pem");
        assert_eq!(settings["dot"]["key_file"], "missing-key.pem");
        assert!(!result["toml"].as_str().unwrap().contains("# keep"));
    }

    #[test]
    fn arrays_replace_and_null_removes_optional_sections() {
        let text = format!(
            "{BASE}filter_file='missing.toml'\n[dot]\nlisten='127.0.0.1:853'\ncert_file='cert.pem'\nkey_file='key.pem'\n[filter]\nenabled=true\nblock_exact=['one.test','two.test']\nallow_suffix=['safe.test']\n"
        );
        let result = change(
            &text,
            json!({"filter_file":null,"dot":null,"filter":{"block_exact":["three.test"]}}),
        )
        .unwrap();
        assert_eq!(result["settings"]["dot"], Value::Null);
        assert_eq!(result["settings"]["filter_file"], Value::Null);
        assert_eq!(result["settings"]["filter"]["enabled"], true);
        assert_eq!(
            result["settings"]["filter"]["block_exact"],
            json!(["three.test"])
        );
        assert_eq!(
            result["settings"]["filter"]["allow_suffix"],
            json!(["safe.test"])
        );
    }

    #[test]
    fn rejects_unknown_fields_bad_types_missing_required_and_out_of_range() {
        for changes in [
            json!({"typo":true}),
            json!({"typo":null}),
            json!({"cache":{"typo":true}}),
            json!({"cache":{"typo":null}}),
            json!({"cache":{"max_bytes":"large"}}),
            json!({"max_inflight":null}),
            json!({"ecs":{"ipv4_prefix":33}}),
            json!({"filter":{"block_exact":[null]}}),
        ] {
            assert!(change(BASE, changes.clone()).is_err(), "{changes}");
        }
        for body in [
            json!({"toml":BASE,"changes":[],"revision":1}),
            json!({"toml":BASE,"changes":null}),
            json!({"toml":BASE,"changes":{},"revision":1}),
        ] {
            assert!(serde_json::from_value::<Preview>(body).is_err());
        }
    }

    #[test]
    fn limits_source_and_normalized_output_size() {
        assert!(parse(&format!("{BASE}#{}", "x".repeat(MAX_CONFIG))).is_err());
        assert!(change(BASE, json!({"filter_file":"x".repeat(MAX_CONFIG)})).is_err());
        assert_eq!(parse(BASE).unwrap()["toml"], BASE);
    }

    #[test]
    fn cache_rule_inheritance_round_trips_through_toml() {
        let rule = json!({"name":"example.test","suffix":true,"qtype":null,"bypass":false,
            "max_ttl_secs":null,"negative_ttl_cap_secs":30,"prefetch":null,"stale":false});
        let result = change(BASE, json!({"cache":{"rules":[rule.clone()]}})).unwrap();
        assert_eq!(result["settings"]["cache"]["rules"][0], rule);
        let removed = change(
            result["toml"].as_str().unwrap(),
            json!({"cache":{"rules":[]}}),
        )
        .unwrap();
        assert_eq!(removed["settings"]["cache"]["rules"], json!([]));
        assert!(change(BASE, json!({"cache":{"rules":[{"name":null}]}})).is_err());
    }
}
