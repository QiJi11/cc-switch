use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use toml_edit::{value, DocumentMut, Item, Table, TableLike};

use crate::error::AppError;

const TRUST_ENV_KEY: &str = "NODE_REPL_TRUSTED_BROWSER_CLIENT_SHA256S";
const OFFICIAL_BROWSER_PLUGIN: &str = "browser@openai-bundled";
const REPAIR_BROWSER_PLUGIN: &str = "browser@browser-repair";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserTrustFile {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "trustedBrowserClientSha256")]
    trusted_browser_client_sha256: Vec<String>,
}

fn browser_config_paths() -> Result<(PathBuf, PathBuf), AppError> {
    let home = crate::config::get_home_dir();
    let trust = home.join(".codex").join("browser-client-trust.json");
    let runtime = home
        .join(".prodex")
        .join("manual-homes")
        .join("ccswitch-current")
        .join("config.toml");
    Ok((trust, runtime))
}

fn is_sha256(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read_document(path: &Path, description: &str) -> Result<DocumentMut, AppError> {
    let source = std::fs::read_to_string(path).map_err(|err| {
        AppError::Message(format!(
            "Failed to read {description} '{}': {err}",
            path.display()
        ))
    })?;
    source.parse::<DocumentMut>().map_err(|err| {
        AppError::Message(format!("Invalid {description} '{}': {err}", path.display()))
    })
}

fn load_trusted_hashes(path: &Path) -> Result<Vec<String>, AppError> {
    let source = std::fs::read_to_string(path).map_err(|err| {
        AppError::Message(format!(
            "Failed to read Browser trust file '{}': {err}",
            path.display()
        ))
    })?;
    let trust: BrowserTrustFile = serde_json::from_str(&source).map_err(|err| {
        AppError::Message(format!(
            "Invalid Browser trust JSON '{}': {err}",
            path.display()
        ))
    })?;
    validate_trust_file(trust, path)
}

fn validate_trust_file(trust: BrowserTrustFile, path: &Path) -> Result<Vec<String>, AppError> {
    if trust.schema_version != 1 || trust.trusted_browser_client_sha256.is_empty() {
        return Err(AppError::Message(format!(
            "Unsupported or empty Browser trust file '{}'",
            path.display()
        )));
    }
    deduplicate_hashes(trust.trusted_browser_client_sha256, "Browser trust file")
}

fn deduplicate_hashes<I>(hashes: I, source: &str) -> Result<Vec<String>, AppError>
where
    I: IntoIterator<Item = String>,
{
    let mut unique = Vec::new();
    for hash in hashes {
        if !is_sha256(&hash) {
            return Err(AppError::Message(format!(
                "{source} contains an invalid SHA-256"
            )));
        }
        if !unique.contains(&hash) {
            unique.push(hash);
        }
    }
    Ok(unique)
}

fn table_like<'a>(
    document: &'a DocumentMut,
    section: &str,
    description: &str,
) -> Result<Option<&'a dyn TableLike>, AppError> {
    document
        .get(section)
        .map(|item| {
            item.as_table_like().ok_or_else(|| {
                AppError::Message(format!("{description} {section} must be a TOML table"))
            })
        })
        .transpose()
}

fn required_nested_item<'a>(
    document: &'a DocumentMut,
    section: &str,
    key: &str,
    description: &str,
) -> Result<&'a Item, AppError> {
    table_like(document, section, description)?
        .and_then(|table| table.get(key))
        .ok_or_else(|| AppError::Message(format!("{description} is missing {section}.{key}")))
}

fn trusted_hashes(document: &DocumentMut, description: &str) -> Result<Vec<String>, AppError> {
    let Some(servers) = table_like(document, "mcp_servers", description)? else {
        return Ok(Vec::new());
    };
    let Some(node_repl) = servers.get("node_repl") else {
        return Ok(Vec::new());
    };
    let node_repl = node_repl.as_table_like().ok_or_else(|| {
        AppError::Message(format!(
            "{description} mcp_servers.node_repl must be a TOML table"
        ))
    })?;
    hashes_from_node_repl(node_repl, description)
}

fn hashes_from_node_repl(
    node_repl: &dyn TableLike,
    description: &str,
) -> Result<Vec<String>, AppError> {
    let Some(env) = node_repl.get("env") else {
        return Ok(Vec::new());
    };
    let env = env.as_table_like().ok_or_else(|| {
        AppError::Message(format!("{description} node_repl.env must be a TOML table"))
    })?;
    let Some(hashes) = env.get(TRUST_ENV_KEY) else {
        return Ok(Vec::new());
    };
    let hashes = hashes.as_str().ok_or_else(|| {
        AppError::Message(format!("{description} {TRUST_ENV_KEY} must be a string"))
    })?;
    deduplicate_hashes(
        hashes
            .split(',')
            .map(str::trim)
            .filter(|hash| !hash.is_empty())
            .map(str::to_owned),
        description,
    )
}

fn validate_runtime(runtime: &DocumentMut) -> Result<(), AppError> {
    let node_repl = required_nested_item(runtime, "mcp_servers", "node_repl", "Browser runtime")?;
    let node_repl = node_repl.as_table_like().ok_or_else(|| {
        AppError::Message("Browser runtime mcp_servers.node_repl must be a TOML table".into())
    })?;
    let valid_command = node_repl
        .get("command")
        .and_then(Item::as_str)
        .is_some_and(|command| !command.trim().is_empty());
    if !valid_command || node_repl.get("env").and_then(Item::as_table_like).is_none() {
        return Err(AppError::Message(
            "Browser runtime node_repl command or env is incomplete".into(),
        ));
    }
    validate_official_browser_plugin(runtime)
}

fn validate_official_browser_plugin(runtime: &DocumentMut) -> Result<(), AppError> {
    let plugin = required_nested_item(
        runtime,
        "plugins",
        OFFICIAL_BROWSER_PLUGIN,
        "Browser runtime",
    )?;
    let enabled = plugin
        .as_table_like()
        .and_then(|table| table.get("enabled"))
        .and_then(Item::as_bool);
    if enabled != Some(true) {
        return Err(AppError::Message(
            "Browser runtime must enable browser@openai-bundled".into(),
        ));
    }
    Ok(())
}

fn target_table<'a>(
    document: &'a mut DocumentMut,
    section: &str,
) -> Result<&'a mut dyn TableLike, AppError> {
    if document.get(section).is_none() {
        document[section] = Item::Table(Table::new());
    }
    document[section].as_table_like_mut().ok_or_else(|| {
        AppError::Message(format!(
            "Codex provider config {section} must be a TOML table"
        ))
    })
}

fn install_browser_runtime(
    target: &mut DocumentMut,
    runtime: &DocumentMut,
) -> Result<(), AppError> {
    let node_repl = required_nested_item(runtime, "mcp_servers", "node_repl", "Browser runtime")?;
    target_table(target, "mcp_servers")?.insert("node_repl", node_repl.clone());

    let official = required_nested_item(
        runtime,
        "plugins",
        OFFICIAL_BROWSER_PLUGIN,
        "Browser runtime",
    )?;
    let plugins = target_table(target, "plugins")?;
    plugins.insert(OFFICIAL_BROWSER_PLUGIN, official.clone());
    ensure_repair_plugin_disabled(plugins)
}

fn ensure_repair_plugin_disabled(plugins: &mut dyn TableLike) -> Result<(), AppError> {
    if plugins.get(REPAIR_BROWSER_PLUGIN).is_none() {
        plugins.insert(REPAIR_BROWSER_PLUGIN, Item::Table(Table::new()));
    }
    let repair = plugins
        .get_mut(REPAIR_BROWSER_PLUGIN)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| {
            AppError::Message("Codex provider browser-repair plugin must be a TOML table".into())
        })?;
    repair.insert("enabled", value(false));
    Ok(())
}

fn set_trusted_hashes(document: &mut DocumentMut, hashes: Vec<String>) -> Result<(), AppError> {
    let node_repl = target_table(document, "mcp_servers")?
        .get_mut("node_repl")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| AppError::Message("Installed Browser runtime is invalid".into()))?;
    let env = node_repl
        .get_mut("env")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| AppError::Message("Installed Browser runtime env is invalid".into()))?;
    env.insert(TRUST_ENV_KEY, value(hashes.join(",")));
    Ok(())
}

fn merged_hashes(
    runtime: &DocumentMut,
    target: &DocumentMut,
    pinned: Vec<String>,
) -> Result<Vec<String>, AppError> {
    let all = trusted_hashes(runtime, "Browser runtime")?
        .into_iter()
        .chain(trusted_hashes(target, "Codex provider config")?)
        .chain(pinned);
    deduplicate_hashes(all, "Merged Browser trust list")
}

fn apply_browser_runtime_to_settings(
    settings: &Value,
    trust_path: &Path,
    runtime_path: &Path,
) -> Result<Value, AppError> {
    let pinned = load_trusted_hashes(trust_path)?;
    let runtime = read_document(runtime_path, "Browser runtime config")?;
    validate_runtime(&runtime)?;
    let config = settings
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Message("Codex provider settings have no config TOML".into()))?;
    let mut target = config.parse::<DocumentMut>().map_err(|err| {
        AppError::Message(format!(
            "Invalid Codex provider config before Browser overlay: {err}"
        ))
    })?;
    let hashes = merged_hashes(&runtime, &target, pinned)?;
    install_browser_runtime(&mut target, &runtime)?;
    set_trusted_hashes(&mut target, hashes)?;
    update_settings_config(settings, target)
}

fn update_settings_config(settings: &Value, document: DocumentMut) -> Result<Value, AppError> {
    let updated_toml = document.to_string();
    updated_toml.parse::<DocumentMut>().map_err(|err| {
        AppError::Message(format!("Invalid Codex config after Browser overlay: {err}"))
    })?;
    let mut updated = settings.clone();
    updated
        .as_object_mut()
        .ok_or_else(|| AppError::Message("Codex provider settings must be a JSON object".into()))?
        .insert("config".into(), Value::String(updated_toml));
    Ok(updated)
}

pub(crate) fn apply_global_browser_trust_to_settings(settings: &Value) -> Result<Value, AppError> {
    let (trust_path, runtime_path) = browser_config_paths()?;
    apply_browser_runtime_to_settings(settings, &trust_path, &runtime_path)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HASH_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn settings(config: &str) -> Value {
        json!({"config": config, "auth": {"OPENAI_API_KEY": "secret"}})
    }

    fn write_trust(path: &Path, hashes: &[&str]) {
        fs::write(
            path,
            serde_json::to_string(&json!({
                "schemaVersion": 1,
                "trustedBrowserClientSha256": hashes,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn runtime_config(hash: &str) -> String {
        format!(
            r#"model = "runtime-model"

[mcp_servers.node_repl]
command = "runtime-node-repl"
args = []
startup_timeout_sec = 120

[mcp_servers.node_repl.env]
NODE_REPL_NODE_PATH = "runtime-node"
{TRUST_ENV_KEY} = "{hash}"

[plugins."browser@openai-bundled"]
enabled = true

[plugins."browser@browser-repair"]
enabled = true
"#
        )
    }

    fn apply(config: &str, trust_hashes: &[&str], runtime: &str) -> Result<Value, AppError> {
        let dir = tempdir().unwrap();
        let trust_path = dir.path().join("trust.json");
        let runtime_path = dir.path().join("runtime.toml");
        write_trust(&trust_path, trust_hashes);
        fs::write(&runtime_path, runtime).unwrap();
        apply_browser_runtime_to_settings(&settings(config), &trust_path, &runtime_path)
    }

    #[test]
    fn missing_node_repl_receives_complete_runtime() {
        let original = r#"model = "provider-model"
model_provider = "custom"

[model_providers.custom]
base_url = "https://provider.example/v1"
"#;
        let updated = apply(original, &[HASH_C], &runtime_config(HASH_B)).unwrap();
        let document = updated["config"]
            .as_str()
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();

        assert_eq!(document["model"].as_str(), Some("provider-model"));
        assert_eq!(
            document["model_providers"]["custom"]["base_url"].as_str(),
            Some("https://provider.example/v1")
        );
        assert_eq!(
            document["mcp_servers"]["node_repl"]["command"].as_str(),
            Some("runtime-node-repl")
        );
        assert_eq!(
            document["plugins"][OFFICIAL_BROWSER_PLUGIN]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            document["plugins"][REPAIR_BROWSER_PLUGIN]["enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(updated["auth"], json!({"OPENAI_API_KEY": "secret"}));
    }

    #[test]
    fn preserves_and_deduplicates_all_trusted_hashes() {
        let config = format!(
            "[mcp_servers.node_repl]\ncommand = \"old\"\n\n[mcp_servers.node_repl.env]\n{TRUST_ENV_KEY} = \"{HASH_A},{HASH_C},{HASH_A}\"\n"
        );
        let updated = apply(&config, &[HASH_C, HASH_C], &runtime_config(HASH_B)).unwrap();
        let document = updated["config"]
            .as_str()
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let hashes = document["mcp_servers"]["node_repl"]["env"][TRUST_ENV_KEY]
            .as_str()
            .unwrap();

        assert_eq!(hashes, format!("{HASH_B},{HASH_A},{HASH_C}"));
        assert_eq!(hashes.split(',').filter(|hash| *hash == HASH_C).count(), 1);
    }

    #[test]
    fn invalid_inputs_stop_before_returning_updated_settings() {
        assert!(apply("[broken\n", &[HASH_A], &runtime_config(HASH_B)).is_err());
        assert!(apply("model = \"ok\"\n", &["invalid"], &runtime_config(HASH_B)).is_err());
        assert!(apply("model = \"ok\"\n", &[HASH_A], "[broken\n").is_err());
        assert!(apply(
            "model = \"ok\"\n",
            &[HASH_A],
            "[mcp_servers.node_repl]\ncommand = \"\"\n\n[plugins.\"browser@openai-bundled\"]\nenabled = true\n"
        )
        .is_err());
    }

    #[test]
    fn malformed_existing_browser_sections_stop_overlay() {
        assert!(apply(
            "[mcp_servers]\nnode_repl = \"invalid\"\n",
            &[HASH_A],
            &runtime_config(HASH_B)
        )
        .is_err());
        assert!(apply(
            "[plugins]\n\"browser@browser-repair\" = true\n",
            &[HASH_A],
            &runtime_config(HASH_B)
        )
        .is_err());
    }

    #[test]
    fn missing_files_stop_overlay() {
        let dir = tempdir().unwrap();
        let original = settings("model = \"gpt-test\"\n");
        assert!(apply_browser_runtime_to_settings(
            &original,
            &dir.path().join("missing-trust.json"),
            &dir.path().join("missing-runtime.toml")
        )
        .is_err());
    }
}
