# Basic LRU Policy — Implementation Guide

This document is the **primary reference for anyone (human or agent) implementing a new policy from scratch**. It describes exactly what the `basic_lru` policy implements and **every complexity and gotcha** encountered. Read it before writing a new policy so you can reuse patterns and avoid the same pitfalls. More complex policies will need to address all of these (paths, byte accounting, renames, reconciliation, touch filter, loop prevention).

**How to use this doc:** Read §9 first for a compact list of what every new policy must handle. Then work through §1–§8 for details, §10 for renames, and §12 for benchmark compliance. Use the checklist in §8 as a final pass before you ship.

---

## 1. What We Implemented (Specifics)

### 1.1 Policy Semantics

- **One hot tier** (capacity-limited) and **one cold tier** (index 0). `basic_lru` requires exactly one cold storage path; `validate_config` enforces this.
- **Model:** The **user** only touches the **hot** namespace (create, rename, delete, edit). **Cold is just where data lives** when we evict; we do not expect the user to create, rename, or delete files directly in the cold repo. All user-visible paths are under hot (either regular files or symlinks we created to cold). We **still track cold** (`cold_sizes`, `cold_bytes`) because that info informs accounting and other policies that may use tier sizes. Events we see are on hot, or on cold only when the user accesses a file via the symlink and the watcher reports the cold path.
- **LRU queue:** A `VecDeque<PathBuf>` of **logical hot paths** (paths under `hot_root`). Order is **LRU at back, MRU at front**. All paths in the queue are stored in **canonical form** (see §2).
- **Touch semantics:** Any access event that we treat as a "touch" either (a) **promotes** the file from cold to hot (if it's currently a symlink to cold), or (b) moves it to **MRU** (front of queue) if it's already a regular file in hot.
- **Eviction:** When hot is over capacity or we need room to promote, we evict **LRU** (back of queue) to cold via `tier_state.move_to_tier(back, cold_root(0))`, then update `hot_sizes`/`cold_sizes` and `hot_bytes`/`cold_bytes` ourselves (see §1.4).
- **Initial fill:** On first `reorganize`, if the queue is empty we call `list_hot_files(hot_root, hot_root)` to discover all **regular files** under hot (we skip symlinks and dirs), seed `hot_sizes` and the queue. We do **not** rescan on later reorganizes; new files enter via events (touched) or reconciliation (see §3, §5).

### 1.2 Data Structures (basic_lru.rs)

| Field | Type | Purpose |
|-------|------|---------|
| `tier_state` | `TierState` | Hot/cold roots, capacities, `hot_bytes`/`cold_bytes`. Policy must call `adjust_hot_bytes` / `adjust_cold_bytes` after every move; `move_to_tier` does **not** update these. |
| `queue` | `VecDeque<PathBuf>` | LRU order: back = LRU, front = MRU. Contains only paths under hot root (logical hot paths). |
| `hot_sizes` | `HashMap<PathBuf, u64>` | Size in bytes for each path we believe is a **regular file** in hot. Key = canonical hot path. |
| `cold_sizes` | `HashMap<PathBuf, u64>` | Size for each path we believe is **in cold** (hot path is symlink). Key = canonical **hot** path (not cold path). |
| `touched` | `Vec<(PathBuf, SystemTime)>` | Events from last `ingest` that we treat as "touch"; rebuilt every `ingest`, drained in `reorganize`. |
| `last_modified` | `HashSet<PathBuf>` | Paths we **modified in the last reorganize** (evicted or promoted). Cleared at **start** of each `reorganize`. Used to avoid reacting to our own Create/Remove/Modify (see §6). |

### 1.3 Main Loop Contract

The **runner** (e.g. `main.rs`) does, every poll cycle:

1. `policy_engine.ingest(&events)` — feed raw watcher events.
2. `policy_engine.reorganize()` — run policy logic (evict/promote, update queue).

So: **ingest** runs **before** **reorganize** in the same cycle. When building `touched` in `ingest`, `last_modified` still contains paths from the **previous** reorganize (because we clear it at the **start** of `reorganize`). This ordering is critical for the touch-filter logic (§6).

### 1.4 Byte Accounting

- **TierState** holds `hot_bytes` and `cold_bytes[i]`. The policy is responsible for keeping them correct.
- **`tier_state.init_bytes()`** is called once at startup; it sets `hot_bytes` and `cold_bytes` from disk (e.g. `tier_fs::tier_size_bytes`). After that, the policy must maintain them.
- **`move_to_tier`** only performs the filesystem move; it does **not** update `hot_bytes` or `cold_bytes`. After every non-no-op move we must:
  - **Evict (hot → cold):** `adjust_hot_bytes(sz, 0)`, `adjust_cold_bytes(0, 0, sz)` (tier index 0), remove from `hot_sizes`, insert into `cold_sizes`.
  - **Promote (cold → hot):** `adjust_hot_bytes(0, sz)`, `adjust_cold_bytes(0, sz, 0)`, remove from `cold_sizes`, insert into `hot_sizes`.
- **In-place edits** (file stayed in same tier but size changed): in `ingest` on `FsEventKind::Modify`, if the path is in `hot_sizes`, we read new size and call `adjust_hot_bytes(old, new)` and update `hot_sizes`. Cold in-place edits could be handled similarly if we tracked cold file sizes per path; currently we only track cold total via `cold_sizes` and don't adjust on cold-file modify.

**Critical — when to subtract `cold_bytes`:**  
`cold_sizes` is keyed by **hot path** (the symlink path); the actual bytes live in the **cold backing file** at `cold_root/rel`. We must **only** subtract from `cold_bytes` when content actually leaves cold:

1. **On promote:** We move the backing from cold to hot, so we call `adjust_cold_bytes(0, sz, 0)` in the promote block. This is the only place we are certain content left cold.
2. **In reconcile (cold):** When we drop a hot path from `cold_sizes` because that hot path no longer exists (e.g. user deleted the symlink, or **renamed** it to another hot path), we must **not** subtract from `cold_bytes` solely because the hot path is gone — the backing file may still be in cold (rename case). We only subtract when the **cold backing file** is actually gone: we compute `cold_backing = cold.join(rel)` and call `adjust_cold_bytes(0, sz, 0)` only if `fs::metadata(&cold_backing).is_err()`.
3. **On Remove (ingest):** We do **not** remove the path from `cold_sizes` in the Remove handler. We leave that to **reconcile** so we can check the cold backing file and only then subtract if it's gone. If we removed from `cold_sizes` and subtracted on every Remove, a **rename** (Remove(old) + Create(new)) would incorrectly drop `cold_bytes` even though the backing never left cold — leading to `cold_bytes` going to zero while content still exists in cold.
4. **When path is "gone" in reorganize** (touched path missing, or back gone in evict loop): We remove from `cold_sizes` but do **not** call `adjust_cold_bytes`; the backing may still be in cold (e.g. path renamed).

**Summary for implementers:** For any policy that tracks cold by "hot path → size", only subtract `cold_bytes` when (a) you **promote** (move backing to hot), or (b) you **reconcile** and confirm the **cold backing file** is missing. Never subtract when you merely drop a hot path from your map (Remove, path gone, or reconcile "hot path doesn't exist").

---

## 2. Paths: Canonical vs Raw — Critical

Watcher events and filesystem APIs can give **non-canonical** paths (e.g. with `..`, or different from the path we stored). We need **stable keys** for `hot_sizes`, `cold_sizes`, `queue`, and `last_modified`.

- **We use a helper `canonical(path)`** that calls `fs::canonicalize(path)` and on failure falls back to canonicalizing the parent and joining the file name (so we can still resolve paths that don't exist yet). Every path we **store** or **compare** (in maps, sets, queue) is canonicalized.
- **All event paths in `ingest`** are canonicalized before we look up in `hot_sizes`/`cold_sizes` or add to `touched`. When we build `touched`, we canonicalize each event path before applying the filter.
- **Cold path → logical hot path:** When the watcher reports an event on the **cold** path (e.g. user wrote via the symlink), we map to the **logical hot path** by `hot_root.join(rel)` where `rel = cold_path.strip_prefix(cold_abs)`. We use this both in the touch filter (is this event about a path we moved?) and in the reorganize loop when processing `touched` (if `path.starts_with(cold_abs)` we rewrite to `hot_root.join(rel)`). **Do not** canonicalize the path at the start of the touch loop after this mapping — on some systems that can break `path.starts_with(hot_root)` for paths we derived from cold.
- **Consistency:** If you mix raw and canonical paths in the same set/map, lookups will fail and you get "ghost" entries or missed updates. Always canonicalize at the boundary (event in, path from FS out).

---

## 3. Handling Deleted Files (No Drift)

If a file is deleted and we miss the Remove event (e.g. coalesced or lost), our in-memory state would still count its bytes and we might hand a non-existent path to `move_to_tier` or make wrong eviction decisions.

### 3.1 In ingest (Remove events)

- On **Remove**, we first check `path_modified_last_reorganize(&path)`. If true, we **skip** updating state (that path was removed by us when we evicted/promoted, so we don't double-remove from hot_sizes/cold_sizes).
- Otherwise we remove from `hot_sizes` (and call `adjust_hot_bytes`, `queue.retain`) if the path was in `hot_sizes`. We **do not** remove from `cold_sizes` or subtract `cold_bytes` in ingest — we leave that to **reconcile** so we can check whether the **cold backing file** is actually gone (see §1.4 and §3.3). If we subtracted on every Remove of a path in `cold_sizes`, a rename (Remove(old) + Create(new)) would incorrectly drop `cold_bytes` while the backing still exists in cold.

### 3.2 In reorganize (path gone when processing touched)

- When we process a path from `touched`, we do `fs::symlink_metadata(&path)`. If it fails (path gone), we remove from `hot_sizes` or `cold_sizes` and adjust bytes **only for hot_sizes**; for `cold_sizes` we remove the entry but **do not** call `adjust_cold_bytes` (the backing may still be in cold, e.g. path renamed). Then `queue.retain`; we do **not** treat the path as a touch.

### 3.3 Reconciliation at start of reorganize (must not persist drift)

- **Before** we use the queue or evict/promote, we **reconcile** state with the filesystem so drift doesn't persist and affect future policy decisions:
  - **Cold:** For each `(p, sz)` in `cold_sizes`, if `fs::symlink_metadata(p).is_err()` (the **hot** path is gone), we remove `p` from `cold_sizes` and from the queue. We **only** call `adjust_cold_bytes(0, sz, 0)` when the **cold backing file** is actually gone: we compute `cold_backing = cold.join(rel)` where `rel = p.strip_prefix(hot_root)` and call `adjust_cold_bytes(0, sz, 0)` only if `fs::metadata(&cold_backing).is_err()`. This way, a **rename** (hot path gone but backing still in cold) does not reduce `cold_bytes`; only a real deletion of the backing does.
  - **Hot:** For each `(p, sz)` in `hot_sizes`, if the path is **gone or is a symlink** (content in cold), we remove from `hot_sizes`, `adjust_hot_bytes(sz, 0)`, and remove `p` from the queue. Dropping symlinks ensures we don't count them as hot and we have correct headroom for promotion (e.g. after a rename the old path is gone and the new path is a symlink; we must not leave the new path in hot_sizes as if it were a regular file).
- We do this **every** reorganize so that even if we missed Removes or had coalesced events, we correct within one cycle.

---

## 4. Hot Edits Showing Up as Cold (Edit via Symlink)

When the user opens a file via the **hot path** (which is a symlink to cold) and writes, the watcher may report the event on the **cold** path (the backing file), not the hot path.

- We **always** treat the **logical** identity as the hot path. So when building `touched`, if the event path is under the cold root we compute `logical_hot = hot_root.join(rel)` and use that for "did we modify this path?" and for promotion.
- In the reorganize loop, when we drain `touched`, if `path.starts_with(&cold_abs)` we rewrite `path = hot_root.join(rel)` so the rest of the loop only deals with hot paths.
- So: **cold-path Modify** that is a genuine user edit (no Create in same batch for that path — see §6) is counted as a touch and will promote that file. The test `touch_via_cold_path_promotes_to_hot` asserts this.

---

## 5. Tracking Paths WE Modified in the Last Reorganize (`last_modified`)

We must not interpret **our own** moves as user activity. We track every path we modify during reorganize in `last_modified`:

- **On eviction (hot → cold):** we insert `canonical(back)` (hot path) and `canonical(cold_path)` (cold backing path).
- **On promotion (cold → hot):** we insert `canonical(&path)` (hot path) and `canonical(cold_backing)` (previous cold path).
- **When we clear:** at the **very start** of `reorganize()` we do `last_modified.clear()`. So when the **next** `ingest` runs (next poll), `last_modified` still holds the paths we modified in the **previous** reorganize.

We use this in two places:

1. **Remove handling in ingest:** If the Remove path (or its logical hot, when the event is on cold) is in `last_modified`, we skip updating state so we don't double-remove.
2. **Building `touched`:** We exclude from `touched` any event that we consider "ours" (see §6). That uses both the event path and, for cold-path events, the logical hot path, checked against `last_modified`.

---

## 6. Infinite Loop (Promote ↔ Evict Every Poll)

**Symptom:** Every poll we promote and evict; hot_bytes and cold_bytes oscillate (e.g. hot=990 → hot=507 → hot=990 …).

**Cause:** After we evict a file to cold, the watcher sends events for the paths we just changed:

- **Create** (and often **Modify**) on the **cold** path (we created the file there).
- **Modify** on the **hot** path (we replaced the file with a symlink).

If we add those events to `touched`, we treat the evicted file as "touched" and **promote** it again; then we evict something else. Next poll: same events again → repeat.

**Fix (three-part filter when building `touched`):**

1. **Create/Remove on any path we moved:** If the event path (or, for cold-path events, the logical hot path) is in `last_modified`, and the kind is Create or Remove, **exclude** from `touched`.
2. **Modify on the hot path we changed:** If the event path is in `last_modified` and under `hot_root` and the kind is Modify, **exclude** (that's our symlink write).
3. **Modify on a cold path that had Create in the same batch:** If the event is Modify and the path is under the **cold** root and that path had a **Create** in this same `ingest` batch, **exclude**. So our "Create + Modify" on the cold file we just wrote doesn't count as a touch.  
   - We **only** apply this rule when the path is under the cold root. So **new files created in hot** with Create+Modify in the same batch still count as touch (we don't exclude them).

**Exception for rename (Create on path in last_modified that is now a symlink):** If we would exclude a **Create** because the path is in `last_modified` (e.g. we evicted that path), but the path **currently exists as a symlink** (user renamed the evicted symlink to this path), we **include** it in `touched` so we can promote. Otherwise a renamed symlink would never get promoted.

**Test that would have caught the bug:** `no_loop_when_ingest_events_from_our_own_eviction`: after one reorganize (so one file is evicted), ingest Create(cold_path), Modify(cold_path), Modify(hot_path), then reorganize again; assert the evicted path is still a symlink and hot_bytes/cold_bytes are unchanged. If you remove the "Modify after Create on cold" (or the "Modify on hot we modified") part of the filter, this test fails.

---

## 7. Known Tradeoffs and Shortfalls (Benchmarking / Correctness)

- **Modify on hot path we modified ignored for one cycle:** We exclude Modify on the hot path we just turned into a symlink (or just promoted to). So if the user touches that **exact** hot path in the same poll cycle (e.g. read/stat), we might not move it to MRU until the next cycle. Acceptable; `last_modified` is cleared at start of next reorganize.
- **New file in hot with Create+Modify in same batch:** We only exclude "Modify when Create in same batch" for paths **under cold**. So new files in hot with Create+Modify still count as touch. If we had excluded all Create+Modify, new hot files could be invisible for one cycle.
- **Watcher only sends Modify (no Create) for cold file after eviction:** If the platform/watcher never sends Create when we write the cold file and only sends Modify, our "Modify after Create on cold" heuristic wouldn't apply and we could still get a loop. In that case we'd need another rule (e.g. exclude Modify on cold path when logical hot is in `last_modified` for one cycle), with the risk of not promoting on "edit via symlink" in that same cycle.
- **Queue is only seeded when empty:** We never do a full rescan of hot after the first run. New files appear via events (touched) or when we reconcile and then process them (we don't auto-add new files that appear without any event). So workloads that create many new files under hot without events might not get them all into the queue until some event touches them or we add a periodic rescan.
- **Cold in-place edits:** We don't currently adjust `cold_sizes` or `cold_bytes` when a file in cold is modified in place (e.g. user edits via symlink and we get Modify on cold path but don't promote). So cold byte count can be slightly wrong if cold files are edited in place. For benchmarking, prefer measuring behavior (evict/promote counts, hot/cold sizes from disk) rather than relying solely on internal `hot_bytes`/`cold_bytes` for correctness.

---

## 8. Checklist for a New Policy Implementation

When implementing a new policy (e.g. another eviction strategy), use this list so you don't repeat the same pitfalls. Every item applies to a policy that moves files between hot and cold and reacts to watcher events.

- [ ] **Paths:** Canonicalize every path before storing or comparing. Use one helper and use it at event ingest and when building any set/map/key. See §2.
- [ ] **Byte accounting:** After every `move_to_tier` that returns non-zero size, call `adjust_hot_bytes` and/or `adjust_cold_bytes`. Never rely on `move_to_tier` to update tier state. **cold_bytes:** Only subtract when content actually leaves cold (on promote, or in reconcile when the **cold backing file** is missing). Never subtract when you merely remove a hot path from your cold map (Remove event, path gone, or reconcile "hot path doesn't exist") — otherwise renames can drive cold_bytes to zero while content still exists in cold. See §1.4, §10.
- [ ] **Remove events:** If you move files yourself, ignore Remove (and Create) for paths you modified in the last reorganize; otherwise you double-remove from your size maps and bytes. Do **not** remove from cold_sizes or subtract cold_bytes in the Remove handler; let reconcile do it so you can check the cold backing file. See §3.1.
- [ ] **Touch/event filter:** Decide which events count as "touch". Exclude (1) Create/Remove on paths you moved, (2) Modify on paths you just wrote (e.g. symlink at hot, or file at cold from eviction). If your eviction creates a file in cold, exclude Modify on that cold path when it had Create in the **same event batch** (and only for cold paths if you want new hot files with Create+Modify to still count). Add an exception: include Create when the path exists as a symlink (rename of evicted file). See §6.
- [ ] **Cold-path events:** Map cold-path events to logical hot path (same relative path under hot_root) so promotion and "path we modified" checks use the same identity. See §4.
- [ ] **Reconciliation:** At start of reorganize, walk your size maps and drop entries whose path no longer exists on disk (or, for hot, is a symlink); adjust bytes and queue. For **cold**, if your map is keyed by hot path: when dropping an entry (hot path gone), only subtract from `cold_bytes` when the **cold backing file** (`cold_root/rel`) is actually missing — not when the hot path is merely gone (e.g. rename). Prevents drift and keeps cold_bytes correct. See §1.4, §3.3.
- [ ] **Eviction when path is gone:** When you pop a path for eviction, if `path.exists()` is false, don't call `move_to_tier`; remove from hot_sizes/cold_sizes. **Adjust bytes only for hot_sizes**; for cold_sizes remove the entry but do **not** subtract cold_bytes (the backing may still be in cold). Then continue. Same for any "evict LRU" loop.
- [ ] **Don't evict the path you're promoting:** When making room for a promotion, if the path you pop from the queue is the same (logical) path you're about to promote, push it back and break. Otherwise you evict the file you're trying to promote and never promote. Use canonical path comparison so you don't evict the current touch path.
- [ ] **Hot reconcile: drop symlinks from hot_sizes.** When reconciling hot_sizes, drop entries whose path is **gone or is a symlink** (not just gone). Subtract their size from hot_bytes. Otherwise after a rename you still count the symlink as hot and may think you have no room to promote.
- [ ] **Initial fill: only regular files in hot_sizes.** When seeding the queue from disk (first reorganize), only add **regular files** to hot_sizes (skip symlinks). So hot_bytes reflects only real hot content and you have correct headroom for promotion.
- [ ] **Test "no loop":** Add a test that, after one reorganize that evicts a file, ingests the exact event pattern the watcher would send (Create+Modify on cold, Modify on hot symlink), runs reorganize again, and asserts no re-promotion and stable hot/cold bytes. This would have caught the basic_lru loop.
- [ ] **Main loop order:** Your `ingest` sees `last_modified` from the **previous** reorganize because the runner clears nothing between ingest and reorganize; we clear `last_modified` at the **start** of reorganize. Design your filter with that in mind.
- [ ] **Renames:** See §10. We do not detect rename (Remove + Create). **cold_bytes** must only be subtracted when content actually leaves cold (promote or cold backing file gone); never subtract when you merely drop a hot path from your map (Remove or reconcile). Otherwise renames can drive cold_bytes to zero while content still exists in cold.
- [ ] **Benchmark compliance:** If you want the benchmark (`tiering_bench`, `scripts/bench_eval.sh`) to report move counts for your policy, implement `stats(&self) -> PolicyStats`: track total promotions and demotions (and `demotions_to_tier` per cold tier). Increment on every successful promote and on every evict to cold. The trait default returns zeros; policies that never move (e.g. dummy) are still runnable by the bench but will show 0 promotions/demotions. See §12.

---

## 9. Implementation Requirements Summary (Must-Have for Any New Policy)

Every new policy that moves files between hot and cold and reacts to watcher events must address **all** of the following. Use this as a quick checklist; details are in the sections cited.

| Requirement | Why | See |
|-------------|-----|-----|
| Canonicalize all paths before store/compare | Raw vs canonical mismatch causes ghost entries and wrong lookups | §2 |
| Never subtract cold_bytes when only dropping a hot path | Rename leaves backing in cold; subtracting would drive cold_bytes to zero with content still there | §1.4, §3, §10 |
| Reconcile cold: only subtract when cold backing file is missing | Same as above; hot path gone ≠ backing gone | §3.3 |
| Do not remove from cold_sizes / subtract in ingest Remove | Reconcile must do it so we can check cold backing | §3.1 |
| When path is gone in reorganize, don't subtract cold_bytes | Backing may still be in cold (rename) | §3.2 |
| Touch filter: exclude Create/Remove on paths we moved, Modify on paths we wrote | Otherwise promote↔evict loop every poll | §6 |
| Touch filter: exclude Modify on cold path when Create in same batch (cold only) | Our eviction creates cold file → Create+Modify; don't count as touch | §6 |
| Exception: include Create when path is symlink (rename of evicted) | So we can promote the renamed symlink | §6 |
| Map cold-path events to logical hot path | So promotion and "our move" checks use same identity | §4 |
| Reconcile hot: drop symlinks and subtract their size | So we don't count symlinks as hot; need headroom for promotion | §3.3 |
| Initial fill: only add regular files to hot_sizes | Symlinks are not hot content | §1.1 |
| When making room for promotion, don't evict the path you're promoting | Otherwise we evict the file we're trying to promote | §8 checklist |
| Clear last_modified at **start** of reorganize | So next ingest sees previous reorganize's paths | §5 |
| Eviction when back is gone: adjust only hot_sizes, not cold_bytes | Backing may still be in cold | §8 checklist |
| Implement `stats()` for benchmarking | So tiering_bench and bench_eval.sh can report promotions/demotions | §12 |

---

## 10. Handling Renames (Filename Changes)

We **do not** explicitly detect renames (e.g. by matching Remove(old) + Create(new) in the same batch). We treat each event independently.

**Rename only in hot — regular file:** If the user renames a **regular file** in hot (e.g. `hot/a` → `hot/b`), we see Remove(hot/a) and Create(hot/b). We drop `a` from hot_sizes/queue and subtract its size; we add `b` from touched with the new size. Hot state and bytes stay correct. **No problem.**

**Rename only in hot — symlink (current behavior):** If the user renames a **symlink** in hot (e.g. `hot/a` → `hot/b`; the backing file stays in cold), we see Remove(hot/a) and Create(hot/b). We **do not** remove from `cold_sizes` or subtract `cold_bytes` in the Remove handler (§3.1). In the next reorganize, **reconcile** drops the old hot path from `cold_sizes` (hot path gone), but we only subtract `cold_bytes` when the **cold backing file** is actually gone (§1.4, §3.3). After a rename the backing is still in cold, so we do **not** subtract. Thus **cold_bytes stays correct**. We allow Create(hot/new) to count as a touch when that path exists as a symlink (§6 exception), so we can promote the renamed symlink. **Result:** cold_bytes correct; promotion possible when we have capacity.

**Correctness and performance in this policy:** basic_lru **never** branches on `cold_bytes` — it only updates it (and logs it). With the accounting rule in §1.4, **cold_bytes** remains correct after renames. Renames do not cause wrong eviction/promotion decisions or loops.

### 10.1 Rename of a **hot** file (regular file at hot path)

- Watcher typically sends **Remove(old_path)** and **Create(new_path)** (and often Modify(new_path)).
- **Remove(old_path):** We drop `old_path` from `hot_sizes`, `queue`, and call `adjust_hot_bytes(old_sz, 0)`. Correct.
- **Create(new_path):** Create is included in `touched` (new path is not in `last_modified`). In reorganize we process `new_path`; it's a regular file under hot and not in `hot_sizes`, so we add it (`new_in_hot`), call `adjust_hot_bytes(0, sz)`, and push to front of queue.
- **Result:** Old path removed, new path added with correct size. **No correctness or performance impact.**

### 10.2 Rename of a **cold** file's symlink (hot path was a symlink → user renames it)

- On Unix, renaming the symlink renames the symlink inode; the **backing file** stays at the same cold path (e.g. `cold/a`).
- Watcher sends **Remove(hot/old)** and **Create(hot/new)**; `hot/new` is a symlink still pointing at `cold/a`.
- **Remove(hot/old):** We do **not** remove from `cold_sizes` or subtract `cold_bytes` in ingest (§3.1). That way we never drop `cold_bytes` when the backing is still in cold.
- **Reconcile:** We drop `hot/old` from `cold_sizes` (hot path gone). We only call `adjust_cold_bytes(0, sz, 0)` if the **cold backing file** `cold/rel` is missing (§3.3). After a rename it is still there, so we do **not** subtract. **cold_bytes** stays correct.
- **Create(hot/new):** We include it in `touched` (exception in §6 when the path exists as a symlink). In reorganize we can **promote** it; promoting moves the backing from cold to hot and we subtract from cold_bytes there. If we don't promote, the backing remains in cold and `cold_bytes` is unchanged (correct).
- **Result:** **cold_bytes** remains correct after rename. No undercount; no double-subtract if we later promote.

### 10.3 Note for other policies

If a policy **does** use `cold_bytes` or `cold_bytes_left()` for decisions (e.g. cold capacity), symlink renames could cause wrong behavior unless the policy explicitly handles rename (e.g. transfer cold_sizes from old to new path when Create(new) is a symlink pointing at the same backing as a just-removed path). With the cold_bytes rule in §1.4, cold_bytes stays correct so capacity decisions are sound.

---

## 12. Benchmark Compliance (Being Complicit with the Bench)

The benchmark (`tiering_bench` binary and `scripts/bench_eval.sh`) runs a synthetic create/delete/edit workload for a **fixed time**: a warmup phase, then a measurement window. It reports **throughput** (ops/s during the measure window) and **policy stats** (promotions, demotions during that window, and as % of ops). To make your policy evaluable by the bench, you must be **complicit**: report accurate move counts so that comparisons across policies (and across runs, e.g. hot vs cold on external drive) are meaningful.

**What the bench expects:** The trait `PolicyEngine` has `fn stats(&self) -> PolicyStats` with a default that returns zeros. If your policy moves files (promote or evict), override it and return real counts. `PolicyStats` has: `promotions` (total cold-to-hot moves), `demotions` (total hot-to-cold moves), and `demotions_to_tier: Vec<u64>` (one per cold tier). For basic_lru we have one cold tier so `demotions_to_tier.len() == 1`.

**What you must do:** (1) Add fields to your policy (e.g. `total_promotions: u64`, `total_demotions: u64`, `demotions_to_tier: Vec<u64>`). (2) Increment them whenever you perform a promote or evict (after a successful `move_to_tier` that returns non-zero size). (3) Implement `fn stats(&self) -> PolicyStats` to return those values. Do not rely on the default if you move files — the bench would then report 0 moves and comparisons would be misleading.

**Why it matters:** When cold is on a slow device, more promotions/demotions can mean slower runs; the bench reports both throughput and move counts. Accurate stats let you compare policies fairly. Reference: `basic_lru` implements `stats()` and increments on every promote and on every evict (both in the make-room loop and in the over-capacity evict loop).

---

## 11. File Layout Reference

- **Policy trait and events:** `src/policy_engine.rs` (`PolicyEngine`, `AccessEvent`, `FsEventKind`).
- **Tier state and move API:** `src/tier_state.rs` (`TierState`, `move_to_tier`, `adjust_hot_bytes`, `adjust_cold_bytes`, `init_bytes`).
- **Actual filesystem move and size:** `src/tier_fs.rs` (`move_to_tier`, `tier_size_bytes`; hot tier size ignores symlinks).
- **basic_lru implementation:** `src/policies/basic_lru.rs`.
- **Runner:** `src/main.rs` (poll → ingest → reorganize).
- **Watcher:** `src/watcher.rs` (paths come from notify; can be relative or absolute; policy canonicalizes).
- **Benchmark:** `src/bin/tiering_bench.rs` (CLI); workload and stats in `src/bench/workload.rs`. Script: `scripts/bench_eval.sh`.

---

**Using this document:** Work through §9 and §8 checklist when implementing a new policy. Use §1–§7 and §10 for the reasoning and edge cases; §12 for benchmark compliance (stats, promotions/demotions). Together with the code, this covers the detailed complexities so your implementation avoids drift, infinite loops, wrong cold_bytes, incorrect handling of renames and self-modified paths, and is evaluable by the benchmark.
