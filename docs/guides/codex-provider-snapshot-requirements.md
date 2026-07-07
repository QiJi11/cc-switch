# Codex provider snapshot requirements

This fork note records a local recovery design for Codex provider switching. It
is intentionally limited to requirements and architecture notes. Do not commit
live provider data, API keys, authorization headers, local databases, generated
homes, or logs.

## Goal

When CC Switch changes the selected Codex provider, new Codex windows should use
the provider selected at launch time. Already running Codex windows should keep
using the provider snapshot they started with.

This avoids a shared-home race where a later provider switch rewrites the
configuration used by an older Codex window.

## Expected behavior

- Each new Codex launch reads the current CC Switch Codex provider.
- The launch creates a fresh Codex home or profile snapshot for that window.
- The snapshot contains only the selected provider configuration needed by that
  window.
- Later provider changes affect future launches only.
- Existing Codex windows, sessions, history, and state databases are not killed,
  rewritten, or deleted.
- The CC Switch database is read as source state; the workaround must not edit it.

## Recovery outline

1. Clone this fork and inspect this guide before applying any local scripts.
2. Recreate the launch wrapper that materializes a per-window Codex home before
   starting Codex.
3. Point the wrapper at local CC Switch and Codex/Prodex paths on the target
   machine.
4. Run a dry launch first and verify that the generated Codex home uses a
   placeholder or locally selected provider, not committed credentials.
5. Start two Codex windows with different selected providers and confirm that
   switching the provider for the second window does not mutate the first
   window's home.

## Security boundary

The following files or values must not be committed:

- API keys, bearer tokens, authorization headers, cookies, or session tokens.
- `auth.json`, generated `config.toml`, live CC Switch databases, SQLite state,
  run homes, histories, sessions, logs, backups, or screenshots.
- Real provider endpoint URLs, provider IDs, account names, or billing labels.
- Machine-specific absolute paths, except generic examples using placeholders.

Use placeholders such as `<provider-id>`, `<base-url>`, `<codex-home>`, and
`<cc-switch-db>` in documentation and examples.

## Validation checklist

- The fork is synced with upstream before adding local recovery notes.
- Sensitive-content scanning finds no secrets or local provider details.
- The only committed changes are documentation or sanitized templates.
- A dry run can prove that a new Codex home is created per launch.
- A two-window smoke test can prove that already running windows are not hot
  rewritten by later provider switches.

