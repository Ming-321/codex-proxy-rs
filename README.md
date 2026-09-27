# Native shared limits verification

Source: `Ming-321/codex-proxy-rs@0f814a3e7944694abf3aae12d5afc7c1c6b9cd35`, based on upstream `3915aa259bb558bb285784b57ba6b3b43abefd93`.
These files are PR evidence only, on a separate fork branch; they are not part of the upstream code diff.

## Behavior evidence

A real Host binary, dedicated PostgreSQL/Redis, and a local synthetic HTTP/SSE upstream were used. No production account or paid provider call was used. All 13 scenarios in the two JSON reports passed against the final implementation. A separate readback checked bindingConfigRevision <= configRevision and the serving instance's loadedConfigRevision.

Synthetic A and B have distinct client profiles and account groups, local concurrency 99, and share source X with concurrency 1. Unrelated C stays unbound. Source RPM=2 and a tiny postpaid weekly budget are tested separately. Synthetic upstream account requestIntervalMs is 0 so the independent account-spacing rule cannot mask key-quota results; this does not alter production code or the shared limits under test. The initial final-binary attempt with account spacing 50ms returned account_capacity_unavailable for C; after fixing this fixture, the entire behavior run passed.

Real downstream HTTP JSON, SSE and WebSocket (store=true, supported HTTP/SSE upstream) succeeded. Core tests also cover WebSocket execution ownership and detached settlement; no real external WebSocket provider was contacted. PostgreSQL/Redis integration tests cover historical-source recovery, inherited nested admissions, late charges after member deletion/last unbind, transactional CAS/retry/audit rollback and migration from schema 19.

## Checks

- Rust 1.97.0: cargo fmt --all --check; cargo clippy -j2 --workspace --all-targets --all-features --locked -- -D warnings: passed
- All 20 migration checksums: passed
- pnpm --dir frontend format:check; pnpm --dir frontend build: passed
- cargo test -j2 --workspace --test main --locked --no-fail-fast -- --test-threads=2: 3185 passed, 16 failed (excluding duplicate subprocess result lines). PG/Redis and plugin PG/Redis variables were set to the dedicated services
- Gateway 45, Admin 248, API 459, Core 444, Host 159, Store 323, SDK 72, protocol 64, plugin CLI 12, xAI 429: all passed
- Plugin runtime 131/132 and OpenAI provider 799/814 passed in the full run. The callback-expiry failure, cold-WebSocket fallback failure and concurrent-WebSocket-opening failure each passed on isolated recheck without code changes. Cold fallback initially failed its first isolated recheck too, then passed when rechecked without a competing build
- The other 13 OpenAI failures below also reproduced on unmodified upstream baseline 3bf7030fba6273553cfbd021fd7d3390b81d0308. Provider and plugin-runtime sources did not change between this baseline and 3915aa25. This establishes existing timing failures for those 13, not a claim that the full suite passed
- Optional external/live-provider paths were not enabled; a passing harness does not mean these paths ran

### Failures reproduced on baseline

- `transport::latency::downstream_websocket_new_chain_should_preserve_continuation_after_slow_opening`
- `transport::latency::external_continuation_should_wait_for_a_cold_websocket`
- `transport::latency::pool_shutdown_should_cancel_and_join_a_background_websocket_opening`
- `transport::latency::timed_out_websocket_should_finish_in_background_and_serve_the_next_request`
- `transport::websocket::codex_backend_client_should_timeout_when_upstream_is_silent`
- `transport::websocket::codex_backend_client_stream_should_keep_socket_after_structural_activity`
- `transport::websocket::codex_backend_client_stream_should_timeout_when_active_websocket_stalls`
- `transport::websocket::codex_backend_client_stream_should_wait_for_terminal_after_active_websocket_gap`
- `transport::websocket::websocket_stream_should_allow_silence_below_idle_timeout`
- `transport::websocket_pool::codex_backend_client_should_close_idle_pooled_websocket_after_liveness_timeout`
- `transport::websocket_pool::codex_backend_client_should_treat_active_business_frames_as_ping_liveness`
- `transport::websocket_pool::codex_backend_client_stream_should_keep_reused_socket_after_structural_activity`
- `transport::websocket_pool::websocket_pool_should_replace_idle_connection_after_pong_deadline`

## Screenshots

Actual Chromium captures. Current screenshots use the final implementation above. Before screenshots use unmodified frontend 3bf7030f against the same synthetic current API dataset to isolate display differences; they do not claim the old Host supported sharing. Existing component/layout/theme is reused. Source IDs and masked keys are synthetic. 1920x1100 except keys-narrow (390x844).

| State | Before | After |
| --- | --- | --- |
| /keys | [keys-before](keys-before.png) | [keys-after](keys-after.png) |
| /key-usage | [usage-before](usage-before.png) | [usage-after](usage-after.png) |

Additional states: [source popover](keys-source-popover.png), [local-limit editor](keys-local-edit.png), [narrow viewport](keys-narrow.png), [dark usage](usage-dark.png).

Browser interactions verified shared-source popover, local-budget editing/saving (shared budget remains unchanged), self-service source display and dark/narrow layouts, with no page errors. Shared binding controls and plugin business UI were not added.
