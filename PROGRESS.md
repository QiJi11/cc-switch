# PROGRESS — CCSwitch post_switch delay fix

## Goal
Remove legacy post-switch runner from Codex provider UI switch path (blocks ≤15s on repair-fast.ps1).

## Status
- [x] git status checked; preserve uncommitted changes (many proxy/session WIP + lib.rs/live.rs)
- [x] Confirmed post_switch only referenced from services/provider/mod.rs
- [x] switch_normal ~L2292 calls post_switch::run_after_switch after write_live
- [x] Remove call + mod + post_switch.rs
- [x] Add regression test switch_provider_codex_does_not_run_legacy_repair_fast
- [x] cargo fmt --check + targeted tests + release build
- [x] Report; STOP before deploy (awaiting user confirmation)

## Do not touch
- lib.rs, live.rs
- user repair-fast.ps1 / DB / settings / credentials
- session usage scan (follow-up)
