# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

used-kinds-rs monitors the Nostr network for undocumented event kinds, stores what it sees in Qdrant, and serves a stats page that groups similar kinds into clusters.

## Development Commands

- `cargo build` / `cargo run` — the app hard-fails at startup without a reachable Qdrant
- `cargo test` — unit tests only (integration tests are `#[ignore]`d)
- `docker run --rm -p 6333:6333 -p 6334:6334 qdrant/qdrant:v1.15.5` then `cargo test -- --ignored --skip fetch_online` — Qdrant integration tests; a shared `QDRANT_TEST_GUARD` mutex serializes them because cluster apply/clear, re-vectorization and sentinel cleanup scan the whole collection, so disjoint point ids alone do not isolate tests
- `cargo test test_fetch_online_relays -- --ignored` — live NIP-66 probe against real monitor relays
- `cargo clippy --all-targets --all-features` and `cargo fmt` before committing

## Architecture

Three actors under a root `Supervisor` (`src/main.rs`, kept alive by `src/service_manager.rs`): **NostrActor** (`src/actors/nostr_actor.rs`) subscribes to relays and forwards events for undocumented kinds; **JsonActor** (`src/actors/json_actor.rs`) owns all Qdrant access and clustering; **HttpActor** (`src/actors/http_actor.rs`, axum) serves `/` (Handlebars UI), `/json`, and `/health`. `GetStatsVec` is answered from a 30s stale-while-revalidate snapshot — the HTTP path never scrolls Qdrant.

### Qdrant point schema

The only datastore. Collection `nostr_events` (`src/qdrant_client.rs`), one point per kind:

- **id**: the kind number (u64)
- **vector**: 64-dim feature vector, cosine distance
- **payload**: `kind`, `count`, `last_updated` (ms), `event_id`, `event` (full JSON of the latest event), `feature_version`, plus `recommended_app` and `cluster_id`/`cluster_similarity`

Payload keys have owners, and flows must only write the keys they own, using merge operations (`set_payload`, `update_vectors`) — never a whole-point upsert, which silently reverts concurrent writers (the only exception is creating a brand-new point):

- `count`/`last_updated`/`event_id`/`event`/`feature_version` — `persist_recorded_event` (RecordEvent)
- `cluster_id`/`cluster_similarity` — `apply_cluster_assignments` and the incremental new-kind assignment
- `recommended_app` — `set_recommended_app`

The count increment is read-modify-write and deliberately not atomic: concurrent updates for one kind can lose an increment, accepted for approximate popularity counts.

### Feature vectors and re-vectorization

`src/similarity.rs` builds the vector from a kind's latest event: six families (tag names, tag value patterns, JSON content keys, encoding, content stats, JSON shape) bucketed by FNV-1a hashing, each family normalized to unit energy, then the whole vector normalized. Updates blend the fresh vector into the stored one (`blend_vectors`, keep=0.8 EMA) so one unusual event cannot teleport a kind across the feature space.

Any change to featurization must bump `FEATURE_VERSION`. Each point's payload records the version it was vectorized with; at boot `revectorize_outdated_points` rebuilds outdated vectors from the stored event JSON. When updating an existing point, the vector is written before the payload so a partial failure can never stamp the current `feature_version` on an old-encoding vector.

### Clustering schedule

Batch clustering recomputes every 3h (and after cleanup): union-find over all pairs with cosine similarity ≥ `CLUSTER_SIMILARITY_THRESHOLD` (0.9), cluster id = lowest member kind. `apply_cluster_assignments` merges new assignments and deletes cluster keys from points that fell out — incremental assignments drift as EMA updates move vectors, and the recompute is what corrects them. `ClusteringSchedule` coalesces recompute requests that land mid-run and replays one when the run's writes finish. A kind seen for the first time gets an incremental assignment from its nearest stored neighbor above the threshold, preferring the neighbor's existing cluster id.

A 6h cleanup tick removes kinds that became documented and kinds not seen for over a month.

### Relay management (NIP-66 discovery)

`nostr_actor` fetches kind-30166 relay-discovery events from the monitor relays (`wss://relay.nostr.watch`, `wss://monitorlizard.nostr1.com`) under a 10s ceiling, keeps wss:// non-overlay relays, and interleaves candidates round-robin across publisher pubkeys so one bursting monitor cannot own the head of the list (rotation takes the first 5; this bounds a misbehaving monitor, not free-keypair sybils). Every 30 minutes five rotating relays are swapped in alongside the always-on set; if discovery yields nothing, a popular-relays fallback applies.

### Documented-kinds list

`src/nips_fetcher.rs` parses the NIPs README kind table (10s fetch deadline — the boot path awaits it), caches it next to `STATS_FILE`, refreshes daily, and falls back to a static list. `is_kind_free` gates which events get recorded at all.

### Recommended apps (NIP-89)

Kind-31990 handler events name apps for kinds. Precedence: content `display_name`/`name`, then the `alt` tag, then `website` — alt in the wild is a NIP-31 description, not a name. No usable name → nothing persisted.

### Boot, migration, supervision

`main` loads the NIPs list, then JsonActor's `pre_start` runs in order: one-time `stats.json` migration (only fresh <30-day undocumented kinds are imported; the file is renamed even when nothing survives; a migration error fails the boot because the point-count guard makes retries unsafe once anything landed), re-vectorization, sentinel cleanup, snapshot priming, initial clustering.

`start.sh` is the container entrypoint: Render web services have no sidecars, so Qdrant runs inside the app container (binary copied in the Dockerfile, storage under `/var/data/qdrant` on the persistent disk, memory knobs tuned for a 512MB instance). The script waits up to `QDRANT_READY_TIMEOUT_SECS` (default 300 — WAL replay after an unclean stop can be slow) for the gRPC port, starts the app, and exits when either process dies so the platform restarts the pair. With an external `QDRANT_URL` (docker compose dev) it only waits, then execs the app.

**Pushing to origin deploys**: Render autoDeploys this repo.

### Environment variables

- `PORT` — HTTP port (default 8080)
- `STATS_FILE` — legacy stats path, still controls migration source and cache dir (default `/var/data/stats.json`)
- `QDRANT_URL` — gRPC URL (default `http://localhost:6334`)
- `QDRANT_READY_TIMEOUT_SECS` — start.sh readiness wait (default 300)
