# Basic LRU Policy — Implementation Guide

This document describes **exactly** what the `basic_lru` policy implements and the **gotchas** encountered during implementation. It is intended for an agent (or human) implementing a **new policy** so they can reuse patterns and avoid the same pitfalls.

---

## 1. What We Implemented (Specifics)

### 1.1 Policy Semantics

- **One hot tier** (capacity-limited) and **one cold tier** (index 0). `basic_lru` requires exactly one cold storage path; `validate_config` enforces this.
- **LRU queue:** A `VecDeque<PathBuf>` of **logical hot paths** (paths under `hot_root`). Order is **LRU at back, MRU at front**. All paths in the queue are stored in **canonical form** (see §3).
- **Touch semantics:** Any access event that we treat as a “touch” either (a) **promotes** the file from cold to hot (if it’s currently a symlink to cold), or (b) moves it to **MRU** (front of queue) if it’s already a regular file in hot.
- **Eviction:** When hot is over capacity or we need room to promote, we evict **LRU** (back of queue) to cold via `tier_state.move_to_tier(back, cold_root(0))`, then update `hot_sizes`/`cold_sizes` and `hot_bytes`/`cold_bytes` ourselves (see §1.4).
- **Initial fill:** On first `reorganize`, if the queue is empty we call `list_hot_files(hot_root, hot_root)` to discover all **regular files** under hot (we skip symlinks and dirs), seed `hot_sizes` and the queue. We do **not** rescan on later reorganizes; new files enter via events (touched) or reconciliation (see §5).

### 1.2 Data Structures (basic_lru.rs)

| Field | Type | Purpose |
|-------|------|--------|
| `tier_state` | `TierState` | Hot/cold roots, capacities, `hot_bytes`/`cold_bytes`. Policy must call `adjust_hot_bytes` / `adjust_cold_bytes` after every move; `move_to_tier` does **not** update these. |
| `queue` | `VecDeque<PathBuf>` | LRU order: back = LRU, front = MRU. Contains only paths under hot root (logical hot paths). |
| `hot_sizes` | `HashMap<PathBuf, u64>` | Size in bytes for each path we believe is a **regular file** in hot. Key = canonical hot path. |
| `cold_sizes` | `HashMap<PathBuf, u64>` | Size for each path we believe is **in cold** (hot path is symlink). Key = canonical **hot** path (not cold path). |
| `touched` | `Vec<(PathBuf, SystemTime)>` | Events from last `ingest` that we treat as “touch”; rebuilt every `ingest`, drained in `reorganize`. |
| `last_modified` | `HashSet<PathBuf>` | Paths we **modified in the last reorganize** (evicted or promoted). Cleared at **start** of each `reorganize`. Used to avoid reacting to our own Create/Remove/Modify (see §7). |

### 1.3 Main Loop Contract

The **runner** (e.g. `main.rs`) does, every poll cycle:

1. `policy_engine.ingest(&events)` — feed raw watcher events.
2. `policy_engine.reorganize()` — run policy logic (evict/promote, update queue).

So: **ingest** runs **before** **reorganize** in the same cycle. When building `touched` in `ingest`, `last_modified` still contains paths from the **previous** reorganize (because we clear it at the **start** of `reorganize`). This ordering is critical for the touch-filter logic (§7).

### 1.4 Byte Accounting

- **TierState** holds `hot_bytes` and `cold_bytes[i]`. The policy is responsible for keeping them correct.
- **`tier_state.init_bytes()`** is called once at startup; it sets `hot_bytes` and `cold_bytes` from disk (e.g. `tier_fs::tier_size_bytes`). After that, the policy must maintain them.
- **`move_to_tier`** only performs the filesystem move; it does **not** update `hot_bytes` or `cold_bytes`. After every non-no-op move we must:
  - **Evict (hot → cold):** `adjust_hot_bytes(sz, 0)`, `adjust_cold_bytes(0, 0, sz)` (tier index 0), remove from `hot_sizes`, insert into `cold_sizes`.
  - **Promote (cold → hot):** `adjust_hot_bytes(0, sz)`, `adjust_cold_bytes(0, sz, 0)`, remove from `cold_sizes`, insert into `hot_sizes`.
- **In-place edits** (file stayed in same tier but size changed): in `ingest` on `FsEventKind::Modify`, if the path is in `hot_sizes`, we read new size and call `adjust_hot_bytes(old, new)` and update `hot_sizes`. Cold in-place edits could be handled similarly if we tracked cold file sizes per path; currently we only track cold total via `cold_sizes` and don’t adjust on cold-file modify.

---

## 2. Paths: Canonical vs Raw — Critical

Watcher events and filesystem APIs can give **non-canonical** paths (e.g. with `..`, or different from the path we stored). We need **stable keys** for `hot_sizes`, `cold_sizes`, `queue`, and `last_modified`.

- **We use a helper `canonical(path)`** that calls `fs::canonicalize(path)` and on failure falls back to canonicalizing the parent and joining the file name (so we can still resolve paths that don’t exist yet). Every path we **store** or **compare** (in maps, sets, queue) is canonicalized.
- **All event paths in `ingest`** are canonicalized before we look up in `hot_sizes`/`cold_sizes` or add to `touched`. When we build `touched`, we canonicalize each event path before applying the filter.
- **Cold path → logical hot path:** When the watcher reports an event on the **cold** path (e.g. user wrote via the symlink), we map to the **logical hot path** by `hot_root.join(rel)` where `rel = cold_path.strip_prefix(cold_abs)`. We use this both in the touch filter (is this event about a path we moved?) and in the reorganize loop when processing `touched` (if `path.starts_with(cold_abs)` we rewrite to `hot_root.join(rel)`).
- **Consistency:** If you mix raw and canonical paths in the same set/map, lookups will fail and you get “ghost” entries or missed updates. Always canonicalize at the boundary (event in, path from FS out).

---

## 3. Handling Deleted Files (No Drift)

If a file is deleted and we miss the Remove event (e.g. coalesced or lost), our in-memory state would still count its bytes and we might hand a non-existent path to `move_to_tier` or make wrong eviction decisions.

### 3.1 In ingest (Remove events)

- On **Remove**, we first check `path_modified_last_reorganize(&path)`. If true, we **skip** updating state (that path was removed by us when we evicted/promoted, so we don’t double-remove from hot_sizes/cold_sizes).
- Otherwise we remove from `hot_sizes` or `cold_sizes`, call `adjust_hot_bytes` or `adjust_cold_bytes`, and `queue.retain(|p| p != &path)`.

### 3.2 In reorganize (path gone when processing touched)

- When we process a path from `touched`, we do `fs::symlink_metadata(&path)`. If it fails (path gone), we remove from `hot_sizes` or `cold_sizes`, adjust bytes, and `queue.retain`; we do **not** treat it as a touch.

### 3.3 Reconciliation at start of reorganize (must not persist drift)

- **Before** we use the queue or evict/promote, we **reconcile** state with the filesystem so drift doesn’t persist and affect future policy decisions:
  - **Cold:** For each `(p, sz)` in `cold_sizes`, if `fs::symlink_metadata(p).is_err()` (the hot-path symlink is gone), remove from `cold_sizes`, `adjust_cold_bytes(0, sz, 0)`, and remove `p` from the queue.
  - **Hot:** For each `(p, sz)` in `hot_sizes`, if `fs::metadata(p).is_err()` (file gone), remove from `hot_sizes`, `adjust_hot_bytes(sz, 0)`, and remove `p` from the queue.
- We do this **every** reorganize so that even if we missed Removes or had coalesced events, we correct within one cycle.

---

## 4. Hot Edits Showing Up as Cold (Edit via Symlink)

When the user opens a file via the **hot path** (which is a symlink to cold) and writes, the watcher may report the event on the **cold** path (the backing file), not the hot path.

- We **always** treat the **logical** identity as the hot path. So when building `touched`, if the event path is under the cold root we compute `logical_hot = hot_root.join(rel)` and use that for “did we modify this path?” and for promotion.
- In the reorganize loop, when we drain `touched`, if `path.starts_with(&cold_abs)` we rewrite `path = hot_root.join(rel)` so the rest of the loop only deals with hot paths.
- So: **cold-path Modify** that is a genuine user edit (no Create in same batch for that path — see §7) is counted as a touch and will promote that file. The test `touch_via_cold_path_promotes_to_hot` asserts this.

---

## 5. Tracking Paths WE Modified in the Last Reorganize (`last_modified`)

We must not interpret **our own** moves as user activity. We track every path we modify during reorganize in `last_modified`:

- **On eviction (hot → cold):** we insert `canonical(back)` (hot path) and `canonical(cold_path)` (cold backing path).
- **On promotion (cold → hot):** we insert `canonical(&path)` (hot path) and `canonical(cold_backing)` (previous cold path).
- **When we clear:** at the **very start** of `reorganize()` we do `last_modified.clear()`. So when the **next** `ingest` runs (next poll), `last_modified` still holds the paths we modified in the **previous** reorganize.

We use this in two places:

1. **Remove handling in ingest:** If the Remove path (or its logical hot, when the event is on cold) is in `last_modified`, we skip updating state so we don’t double-remove.
2. **Building `touched`:** We exclude from `touched` any event that we consider “ours” (see §7). That uses both the event path and, for cold-path events, the logical hot path, checked against `last_modified`.

---

## 6. Infinite Loop (Promote ↔ Evict Every Poll)

**Symptom:** Every poll we promote and evict; hot_bytes and cold_bytes oscillate (e.g. hot=990 → hot=507 → hot=990 …).

**Cause:** After we evict a file to cold, the watcher sends events for the paths we just changed:

- **Create** (and often **Modify**) on the **cold** path (we created the file there).
- **Modify** on the **hot** path (we replaced the file with a symlink).

If we add those events to `touched`, we treat the evicted file as “touched” and **promote** it again; then we evict something else. Next poll: same events again → repeat.

**Fix (three-part filter when building `touched`):**

1. **Create/Remove on any path we moved:** If the event path (or, for cold-path events, the logical hot path) is in `last_modified`, and the kind is Create or Remove, **exclude** from `touched`.
2. **Modify on the hot path we changed:** If the event path is in `last_modified` and under `hot_root` and the kind is Modify, **exclude** (that’s our symlink write).
3. **Modify on a cold path that had Create in the same batch:** If the event is Modify and the path is under the **cold** root and that path had a **Create** in this same `ingest` batch, **exclude**. So our “Create + Modify” on the cold file we just wrote doesn’t count as a touch.  
   - We **only** apply this rule when the path is under the cold root. So **new files created in hot** with Create+Modify in the same batch still count as touch (we don’t exclude them).

**Test that would have caught the bug:** `no_loop_when_ingest_events_from_our_own_eviction`: after one reorganize (so one file is evicted), ingest Create(cold_path), Modify(cold_path), Modify(hot_path), then reorganize again; assert the evicted path is still a symlink and hot_bytes/cold_bytes are unchanged. If you remove the “Modify after Create on cold” (or the “Modify on hot we modified”) part of the filter, this test fails.

---

## 7. Known Tradeoffs and Shortfalls (Benchmarking / Correctness)

- **Modify on hot path we modified ignored for one cycle:** We exclude Modify on the hot path we just turned into a symlink (or just promoted to). So if the user touches that **exact** hot path in the same poll cycle (e.g. read/stat), we might not move it to MRU until the next cycle. Acceptable; `last_modified` is cleared at start of next reorganize.
- **New file in hot with Create+Modify in same batch:** We only exclude “Modify when Create in same batch” for paths **under cold**. So new files in hot with Create+Modify still count as touch. If we had excluded all Create+Modify, new hot files could be invisible for one cycle.
- **Watcher only sends Modify (no Create) for cold file after eviction:** If the platform/watcher never sends Create when we write the cold file and only sends Modify, our “Modify after Create on cold” heuristic wouldn’t apply and we could still get a loop. In that case we’d need another rule (e.g. exclude Modify on cold path when logical hot is in `last_modified` for one cycle), with the risk of not promoting on “edit via symlink” in that same cycle.
- **Queue is only seeded when empty:** We never do a full rescan of hot after the first run. New files appear via events (touched) or when we reconcile and then process them (we don’t auto-add new files that appear without any event). So workloads that create many new files under hot without events might not get them all into the queue until some event touches them or we add a periodic rescan.
- **Cold in-place edits:** We don’t currently adjust `cold_sizes` or `cold_bytes` when a file in cold is modified in place (e.g. user edits via symlink and we get Modify on cold path but don’t promote). So cold byte count can be slightly wrong if cold files are edited in place. For benchmarking, prefer measuring behavior (evict/promote counts, hot/cold sizes from disk) rather than relying solely on internal `hot_bytes`/`cold_bytes` for correctness.

---

## 8. Checklist for a New Policy Implementation

When implementing a new policy (e.g. another eviction strategy), use this list so you don’t repeat the same pitfalls:

- [ ] **Paths:** Canonicalize every path before storing or comparing. Use one helper and use it at event ingest and when building any set/map/key.
- [ ] **Byte accounting:** After every `move_to_tier` that returns non-zero size, call `adjust_hot_bytes` and/or `adjust_cold_bytes`. Never rely on `move_to_tier` to update tier state.
- [ ] **Remove events:** If you move files yourself, ignore Remove (and Create) for paths you modified in the last reorganize; otherwise you double-remove from your size maps and bytes.
- [ ] **Touch/event filter:** Decide which events count as “touch”. Exclude (1) Create/Remove on paths you moved, (2) Modify on paths you just wrote (e.g. symlink at hot, or file at cold from eviction). If your eviction creates a file in cold, exclude Modify on that cold path when it had Create in the **same event batch** (and only for cold paths if you want new hot files with Create+Modify to still count).
- [ ] **Cold-path events:** Map cold-path events to logical hot path (same relative path under hot_root) so promotion and “path we modified” checks use the same identity.
- [ ] **Reconciliation:** At start of reorganize, walk your size maps and drop entries whose path no longer exists on disk; adjust bytes and queue. Prevents drift from missed Removes or coalesced events.
- [ ] **Eviction when path is gone:** When you pop a path for eviction, if `path.exists()` is false, don’t call `move_to_tier`; remove from hot_sizes/cold_sizes and adjust bytes, then continue (same for any “evict LRU” loop).
- [ ] **Test “no loop”:** Add a test that, after one reorganize that evicts a file, ingests the exact event pattern the watcher would send (Create+Modify on cold, Modify on hot symlink), runs reorganize again, and asserts no re-promotion and stable hot/cold bytes. This would have caught the basic_lru loop.
- [ ] **Main loop order:** Your `ingest` sees `last_modified` from the **previous** reorganize because the runner clears nothing between ingest and reorganize; we clear `last_modified` at the **start** of reorganize. Design your filter with that in mind.
- [ ] **Renames:** See §10. We do not detect rename (Remove + Create). In **this policy** renames do not cause correctness or performance issues; cold_bytes undercount after symlink rename is accounting/observability only.

---

## 10. Handling Renames (Filename Changes)

We **do not** explicitly detect renames (e.g. by matching Remove(old) + Create(new) in the same batch). We treat each event independently.

**Correctness and performance in this policy:** basic_lru **never** branches on `cold_bytes` — it only updates it (and logs it). Eviction and promotion are driven by **hot_bytes**, **hot_bytes_left()**, the **queue** (LRU order), and **touched** events. So **renames do not cause wrong eviction/promotion decisions, loops, or other correctness or performance issues** in this policy. The only effect of a symlink rename is that `cold_bytes` can be undercounted (observability); that does not change how the policy behaves.

### 10.1 Rename of a **hot** file (regular file at hot path)

- Watcher typically sends **Remove(old_path)** and **Create(new_path)** (and often Modify(new_path)).
- **Remove(old_path):** We drop `old_path` from `hot_sizes`, `queue`, and call `adjust_hot_bytes(old_sz, 0)`. Correct.
- **Create(new_path):** Create is included in `touched` (new path is not in `last_modified`). In reorganize we process `new_path`; it’s a regular file under hot and not in `hot_sizes`, so we add it (`new_in_hot`), call `adjust_hot_bytes(0, sz)`, and push to front of queue.
- **Result:** Old path removed, new path added with correct size. **No correctness or performance impact.**

### 10.2 Rename of a **cold** file’s symlink (hot path was a symlink → user renames it)

- On Unix, renaming the symlink renames the symlink inode; the **backing file** stays at the same cold path (e.g. `cold/a`).
- Watcher sends **Remove(hot/old)** and **Create(hot/new)**; `hot/new` is a symlink still pointing at `cold/a`.
- **Remove(hot/old):** We remove `hot/old` from `cold_sizes` and call `adjust_cold_bytes(0, sz, 0)`. We have now **subtracted** that file’s size from `cold_bytes`, but the backing file **still exists** in cold. So **cold_bytes is undercounted** until we correct it.
- **Create(hot/new):** We include it in `touched`. In reorganize we see `hot/new` is a symlink; we treat it as “in cold” and can **promote** it. Promoting moves the backing from cold to hot and we subtract from cold again (and add to hot). So we would **double-subtract** from cold (once on Remove, once on promote) — **cold_bytes** would be wrong (too low). If we **don’t** promote (e.g. no capacity), we never add `hot/new` to `cold_sizes`, so we have an orphan cold file (still on disk) and **cold_bytes** remains undercounted.
- **Reconciliation** only drops entries whose **hot path** is gone. After a rename, the hot path **is** gone (old path), so we drop it and subtract from cold_bytes — which we already did on Remove. The **new** hot path (`hot/new`) is a symlink; we don’t add it to `cold_sizes` in reconciliation (we only remove stale entries). So we never “re-add” the cold file under the new name in our maps.
- **Result:** **No correctness or performance impact.** Eviction and promotion logic are unchanged. The only effect is that **cold_bytes** can be too low (accounting/observability only); this policy does not use cold_bytes for any control flow.

### 10.3 Note for other policies

If a policy **does** use `cold_bytes` or `cold_bytes_left()` for decisions (e.g. cold capacity), symlink renames could cause wrong behavior unless the policy explicitly handles rename (e.g. transfer cold_sizes from old to new path when Create(new) is a symlink pointing at the same backing as a just-removed path).

---

## 11. File Layout Reference

- **Policy trait and events:** `src/policy_engine.rs` (`PolicyEngine`, `AccessEvent`, `FsEventKind`).
- **Tier state and move API:** `src/tier_state.rs` (`TierState`, `move_to_tier`, `adjust_hot_bytes`, `adjust_cold_bytes`, `init_bytes`).
- **Actual filesystem move and size:** `src/tier_fs.rs` (`move_to_tier`, `tier_size_bytes`; hot tier size ignores symlinks).
- **basic_lru implementation:** `src/policies/basic_lru.rs`.
- **Runner:** `src/main.rs` (poll → ingest → reorganize).
- **Watcher:** `src/watcher.rs` (paths come from notify; can be relative or absolute; policy canonicalizes).

Using this document together with the code, an agent can implement a new policy while avoiding the gotchas listed above and ensuring similar robustness (no drift, no infinite loop, correct handling of cold-path and self-modified paths).
