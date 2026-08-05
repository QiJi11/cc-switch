from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import sqlite3
import sys
from datetime import datetime
from pathlib import Path
from typing import Any

import tomllib


HOME = Path(r"C:\Users\tianh")
DB_PATH = HOME / ".cc-switch" / "cc-switch.db"
SETTINGS_PATH = HOME / ".cc-switch" / "settings.json"
CODEX_CONFIG_PATH = HOME / ".codex" / "config.toml"
CODEX_AUTH_PATH = HOME / ".codex" / "auth.json"
TRUST_PATH = HOME / ".codex" / "browser-client-trust.json"
OVERLAY_PATH = HOME / ".prodex" / "bin" / "apply-browser-trust-overlay.py"
RUNTIME_CONFIG_PATH = (
    HOME / ".prodex" / "manual-homes" / "ccswitch-current" / "config.toml"
)
EXPECTED_BROWSER_HASH = (
    "8039a3e2ee944e0867708719a27871dcf24952c1841b2c10b5f47cdcfddaaf32"
)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8-sig"))
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return value


def load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as stream:
        value = tomllib.load(stream)
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain a TOML table")
    return value


def connect(*, readonly: bool = False) -> sqlite3.Connection:
    if readonly:
        connection = sqlite3.connect(f"file:{DB_PATH}?mode=ro", uri=True, timeout=20)
    else:
        connection = sqlite3.connect(DB_PATH, timeout=20)
    connection.row_factory = sqlite3.Row
    return connection


def table_columns(connection: sqlite3.Connection, table: str) -> list[str]:
    return [str(row[1]) for row in connection.execute(f"PRAGMA table_info({table})")]


def first_nonempty(mapping: dict[str, Any], names: tuple[str, ...]) -> Any:
    for name in names:
        value = mapping.get(name)
        if value not in (None, ""):
            return value
    return None


def parse_json_objects(row: dict[str, Any]) -> list[dict[str, Any]]:
    objects: list[dict[str, Any]] = []
    for key, value in row.items():
        if not isinstance(value, str) or not value.strip().startswith(("{", "[")):
            continue
        try:
            parsed = json.loads(value)
        except json.JSONDecodeError:
            continue
        if isinstance(parsed, dict):
            objects.append(parsed)
    return objects


def parse_embedded_toml(objects: list[dict[str, Any]]) -> list[dict[str, Any]]:
    parsed_documents: list[dict[str, Any]] = []
    for obj in objects:
        for key, value in obj.items():
            if key.lower() not in {"config", "config_toml", "toml"}:
                continue
            if not isinstance(value, str) or not value.strip():
                continue
            try:
                parsed = tomllib.loads(value)
            except tomllib.TOMLDecodeError:
                continue
            if isinstance(parsed, dict):
                parsed_documents.append(parsed)
    return parsed_documents


def flatten_dict(value: dict[str, Any], prefix: str = "") -> dict[str, Any]:
    flattened: dict[str, Any] = {}
    for key, item in value.items():
        path = f"{prefix}.{key}" if prefix else key
        if isinstance(item, dict):
            flattened.update(flatten_dict(item, path))
        else:
            flattened[path] = item
    return flattened


def provider_summary(row: sqlite3.Row) -> dict[str, Any]:
    raw = dict(row)
    settings_object: dict[str, Any] = {}
    if isinstance(raw.get("settings_config"), str):
        parsed_settings = json.loads(raw["settings_config"])
        if isinstance(parsed_settings, dict):
            settings_object = parsed_settings
    merged: dict[str, Any] = {}
    json_objects = parse_json_objects(raw)
    for obj in json_objects:
        merged.update(flatten_dict(obj))
    for document in parse_embedded_toml(json_objects):
        merged.update(flatten_dict(document))

    def by_suffix(*suffixes: str) -> Any:
        lowered = tuple(s.lower() for s in suffixes)
        for key, value in merged.items():
            leaf = key.rsplit(".", 1)[-1].lower()
            if leaf in lowered and value not in (None, ""):
                return value
        return None

    credentials: list[dict[str, Any]] = []
    for key, value in merged.items():
        leaf = key.rsplit(".", 1)[-1].lower()
        if leaf not in {"api_key", "apikey", "openai_api_key", "token", "auth_token"}:
            continue
        if not isinstance(value, str) or not value:
            continue
        credentials.append(
            {
                "field": key,
                "length": len(value),
                "sha256Prefix": hashlib.sha256(value.encode("utf-8")).hexdigest()[:12],
            }
        )

    return {
        "id": raw.get("id"),
        "name": raw.get("name"),
        "category": raw.get("category"),
        "isCurrent": raw.get("is_current"),
        "baseUrl": by_suffix("base_url", "baseurl", "openai_base_url"),
        "model": by_suffix("model", "model_name", "openai_model"),
        "wireApi": by_suffix("wire_api", "wireapi"),
        "apiFormat": first_nonempty(
            settings_object,
            ("api_format", "apiFormat"),
        ),
        "catalogModels": [
            model.get("model")
            for model in settings_object.get("modelCatalog", {}).get("models", [])
            if isinstance(model, dict) and isinstance(model.get("model"), str)
        ],
        "credentialSummaries": credentials,
        "jsonObjectCount": len(json_objects),
        "embeddedTomlCount": len(parse_embedded_toml(json_objects)),
    }


def audit() -> dict[str, Any]:
    settings = load_json(SETTINGS_PATH)
    config = load_toml(CODEX_CONFIG_PATH)
    trust = load_json(TRUST_PATH)
    trusted = trust.get("trustedBrowserClientSha256")
    if not isinstance(trusted, list):
        raise ValueError("browser-client-trust.json trusted hash list is invalid")

    connection = connect(readonly=True)
    try:
        integrity = str(connection.execute("PRAGMA integrity_check").fetchone()[0])
        provider_columns = table_columns(connection, "providers")
        proxy_columns = table_columns(connection, "proxy_config")
        settings_columns = table_columns(connection, "settings")
        request_log_columns = table_columns(connection, "proxy_request_logs")
        providers = connection.execute(
            "SELECT * FROM providers WHERE app_type = 'codex' ORDER BY name, id"
        ).fetchall()
        proxy_rows = [
            dict(row)
            for row in connection.execute(
                "SELECT * FROM proxy_config ORDER BY app_type"
            ).fetchall()
        ]
        current_rows = [
            {"key": row[0], "value": row[1]}
            for row in connection.execute(
                "SELECT key, value FROM settings "
                "WHERE lower(key) LIKE '%current%' OR lower(key) LIKE '%provider%' "
                "ORDER BY key"
            ).fetchall()
        ]
        recent_request_logs = [
            dict(row)
            for row in connection.execute(
                "SELECT request_id, provider_id, app_type, model, request_model, "
                "status_code, latency_ms, first_token_ms, duration_ms, "
                "error_message, is_streaming, created_at "
                "FROM proxy_request_logs WHERE app_type = 'codex' "
                "AND provider_id <> '_codex_session' "
                "ORDER BY created_at DESC LIMIT 8"
            ).fetchall()
        ]
    finally:
        connection.close()

    model_provider = config.get("model_provider")
    provider_table = config.get("model_providers", {})
    active_provider = (
        provider_table.get(model_provider, {})
        if isinstance(provider_table, dict) and isinstance(model_provider, str)
        else {}
    )
    node_env = (
        config.get("mcp_servers", {}).get("node_repl", {}).get("env", {})
        if isinstance(config.get("mcp_servers"), dict)
        else {}
    )
    trusted_live = str(node_env.get("NODE_REPL_TRUSTED_BROWSER_CLIENT_SHA256S", ""))

    return {
        "database": {
            "integrityCheck": integrity,
            "providerColumns": provider_columns,
            "proxyColumns": proxy_columns,
            "settingsColumns": settings_columns,
            "requestLogColumns": request_log_columns,
            "proxyRows": proxy_rows,
            "currentProviderSettings": current_rows,
            "recentCodexRequestLogs": recent_request_logs,
        },
        "providers": [provider_summary(row) for row in providers],
        "settings": {
            key: settings.get(key)
            for key in (
                "enableLocalProxy",
                "codexPortableHandoffOnProviderChange",
                "preserveCodexOfficialAuthOnSwitch",
                "unifyCodexSessionHistory",
            )
        },
        "codexConfig": {
            "model": config.get("model"),
            "modelProvider": model_provider,
            "baseUrl": active_provider.get("base_url") if isinstance(active_provider, dict) else None,
            "wireApi": active_provider.get("wire_api") if isinstance(active_provider, dict) else None,
            "browserHashPresent": EXPECTED_BROWSER_HASH in trusted_live.split(","),
        },
        "trustFile": {
            "schemaVersion": trust.get("schemaVersion"),
            "expectedHashPresent": EXPECTED_BROWSER_HASH in trusted,
            "allTrustHashesPresentInLiveConfig": all(
                item in trusted_live.split(",") for item in trusted
            ),
            "hashCount": len(trusted),
        },
    }


def backup(backup_root: Path) -> dict[str, Any]:
    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    destination = backup_root / f"ccswitch-proxy-repair-{timestamp}"
    destination.mkdir(parents=True, exist_ok=False)

    source = connect(readonly=True)
    target = sqlite3.connect(destination / DB_PATH.name)
    try:
        source.backup(target)
        integrity = str(target.execute("PRAGMA integrity_check").fetchone()[0])
        if integrity != "ok":
            raise RuntimeError(f"database backup integrity_check failed: {integrity}")
    finally:
        target.close()
        source.close()

    files = [SETTINGS_PATH, CODEX_CONFIG_PATH, CODEX_AUTH_PATH, TRUST_PATH]
    copied: list[dict[str, Any]] = []
    for source_path in files:
        if not source_path.exists():
            continue
        target_path = destination / source_path.name
        shutil.copy2(source_path, target_path)
        copied.append(
            {
                "name": target_path.name,
                "size": target_path.stat().st_size,
                "sha256": sha256(target_path),
            }
        )

    db_copy = destination / DB_PATH.name
    copied.append(
        {
            "name": db_copy.name,
            "size": db_copy.stat().st_size,
            "sha256": sha256(db_copy),
        }
    )
    manifest = {"createdAt": datetime.now().isoformat(), "files": copied}
    (destination / "manifest.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    return {"backupDirectory": str(destination), "databaseIntegrityCheck": integrity}


def arm() -> dict[str, Any]:
    settings = load_json(SETTINGS_PATH)
    load_toml(CODEX_CONFIG_PATH)
    load_json(CODEX_AUTH_PATH)
    load_json(TRUST_PATH)

    original_settings = SETTINGS_PATH.read_bytes()

    settings["enableLocalProxy"] = True
    settings["codexPortableHandoffOnProviderChange"] = True
    settings["preserveCodexOfficialAuthOnSwitch"] = True
    settings["silentStartup"] = True

    temporary = SETTINGS_PATH.with_suffix(".json.proxy-repair-tmp")
    temporary.write_text(
        json.dumps(settings, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    load_json(temporary)

    connection = connect()
    committed = False
    previous_proxy_enabled: list[tuple[str, int]] = []
    previous_codex_enabled = 0
    try:
        connection.execute("BEGIN IMMEDIATE")
        codex_rows = int(
            connection.execute(
                "SELECT COUNT(*) FROM proxy_config WHERE app_type = 'codex'"
            ).fetchone()[0]
        )
        if codex_rows != 1:
            raise RuntimeError(f"expected one codex proxy row, found {codex_rows}")
        previous_proxy_enabled = [
            (str(row[0]), int(row[1]))
            for row in connection.execute(
                "SELECT app_type, proxy_enabled FROM proxy_config"
            ).fetchall()
        ]
        previous_codex_enabled = int(
            connection.execute(
                "SELECT enabled FROM proxy_config WHERE app_type = 'codex'"
            ).fetchone()[0]
        )
        connection.execute("UPDATE proxy_config SET proxy_enabled = 1")
        connection.execute(
            "UPDATE proxy_config SET enabled = 1 WHERE app_type = 'codex'"
        )
        connection.commit()
        committed = True
    finally:
        if not committed:
            connection.rollback()
        connection.close()

    try:
        os.replace(temporary, SETTINGS_PATH)
    except BaseException:
        rollback = connect()
        try:
            rollback.execute("BEGIN IMMEDIATE")
            rollback.executemany(
                "UPDATE proxy_config SET proxy_enabled = ? WHERE app_type = ?",
                [(enabled, app_type) for app_type, enabled in previous_proxy_enabled],
            )
            rollback.execute(
                "UPDATE proxy_config SET enabled = ? WHERE app_type = 'codex'",
                (previous_codex_enabled,),
            )
            rollback.commit()
        finally:
            rollback.close()
        SETTINGS_PATH.write_bytes(original_settings)
        raise

    connection = connect(readonly=True)
    try:
        row = dict(
            connection.execute(
                "SELECT app_type, proxy_enabled, enabled, listen_address, listen_port "
                "FROM proxy_config WHERE app_type = 'codex'"
            ).fetchone()
        )
    finally:
        connection.close()
    return {"armed": True, "codexProxy": row, "settings": audit()["settings"]}


def restore_window_preference() -> dict[str, Any]:
    settings = load_json(SETTINGS_PATH)
    settings["silentStartup"] = False
    temporary = SETTINGS_PATH.with_suffix(".json.window-preference-tmp")
    temporary.write_text(
        json.dumps(settings, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    load_json(temporary)
    os.replace(temporary, SETTINGS_PATH)
    return {"silentStartup": load_json(SETTINGS_PATH).get("silentStartup")}


def apply_browser_common_config() -> dict[str, Any]:
    spec = importlib.util.spec_from_file_location("browser_trust_overlay", OVERLAY_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError("unable to load Browser trust overlay module")
    overlay = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(overlay)

    pinned_hashes = overlay.load_pinned_hashes(TRUST_PATH)
    runtime_source = overlay.read_text(RUNTIME_CONFIG_PATH)
    connection = connect()
    committed = False
    try:
        connection.execute("BEGIN IMMEDIATE")
        row = connection.execute(
            "SELECT value FROM settings WHERE key = 'common_config_codex'"
        ).fetchone()
        if row is None or not isinstance(row[0], str) or not row[0].strip():
            raise RuntimeError("common_config_codex is missing or empty")
        original = str(row[0])
        updated = overlay.merge_browser_runtime(original, runtime_source, pinned_hashes)
        cursor = connection.execute(
            "UPDATE settings SET value = ? "
            "WHERE key = 'common_config_codex' AND value = ?",
            (updated, original),
        )
        if cursor.rowcount != 1:
            raise RuntimeError("common_config_codex changed during update")
        connection.commit()
        committed = True
    finally:
        if not committed:
            connection.rollback()
        connection.close()

    document = tomllib.loads(updated)
    hashes = overlay.existing_hashes(document)
    return {
        "changed": original != updated,
        "originalLength": len(original),
        "updatedLength": len(updated),
        "pinnedHashesPresent": set(pinned_hashes).issubset(hashes),
        "officialBrowserEnabled": document.get("plugins", {}).get(
            "browser@openai-bundled"
        )
        == {"enabled": True},
        "localBrowserRepairDisabled": document.get("plugins", {}).get(
            "browser@browser-repair"
        )
        == {"enabled": False},
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "mode",
        choices=(
            "audit",
            "backup",
            "arm",
            "restore-window-preference",
            "apply-browser-common-config",
        ),
    )
    parser.add_argument(
        "--backup-root",
        type=Path,
        default=HOME / ".cc-switch" / "backups",
    )
    args = parser.parse_args()

    if args.mode == "audit":
        result = audit()
    elif args.mode == "backup":
        result = backup(args.backup_root)
    elif args.mode == "arm":
        result = arm()
    elif args.mode == "restore-window-preference":
        result = restore_window_preference()
    else:
        result = apply_browser_common_config()
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
