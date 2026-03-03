# Design Motivation

This document explains why the benchmark is built the way it is: the daemon
simulation model, why we dropped throughput as a metric, what we measure
instead, and how filesystem tiering policies differ from the eviction and
scheduling policies you might study elsewhere.

---

## 1. The Daemon: What It Is and Why We Simulate It Synchronously

In production, a tiering daemon is a long-running background process that
watches filesystem events (inotify on Linux, FSEvents on macOS) and
periodically reorganizes file placement — promoting frequently-accessed files
to fast storage and demoting cold files to cheap storage. The daemon is
completely invisible to the application; it runs out-of-band, touching the
same files the client is reading and writing.

### The async attempt and why it broke

Our first implementation used a real background thread: the workload thread
produced `AccessEvent`s, a daemon thread consumed them and called
`reorganize()`, and both threads shared the filesystem simultaneously. This
is architecturally correct for production, but it created a race condition in
the benchmark:

- The workload opens a file path to write it.
- Concurrently, the daemon calls `move_to_tier`, which renames the file
  from the hot directory to cold storage and replaces it with a symlink.
- The workload's in-flight write now targets a path that no longer exists
  as a regular file → `ENOENT`.

On Linux with inotify this race is narrow but real. On macOS, FSEvents adds
a second problem: event delivery is coalesced and delayed by the kernel,
sometimes by hundreds of milliseconds, so the daemon's view of recent access
is stale by the time it acts. The result was wild, unrepeatable numbers:
0 promotions, demotion percentages above 100%, throughput swings of 3×
between identical runs. No amount of locking inside individual operations
fixes this without a full per-file coordination layer (e.g., a `DashMap`
of in-flight file sets that both threads consult before acting), and that
coordination layer would itself become a benchmark artifact.

### The synchronous model

Rather than instrument the coordination overhead into the benchmark, we
simulate the daemon synchronously: every `poll_interval_ops` workload
operations, the workload pauses, flushes accumulated `AccessEvent`s to
`policy.ingest()`, calls `policy.reorganize()`, and then resumes. This
matches the daemon's *logical* behavior — it wakes on a schedule, ingests
recent events, and moves files — while eliminating the race entirely.

**This is not a simplification that changes what we're measuring.** The
daemon's job is to make placement decisions based on access history. Whether
those decisions are made in a background thread or in a periodic synchronous
pause does not affect the quality of the decisions. What matters for policy
evaluation is the sequence of events the policy sees and the file placement
it achieves, both of which are identical in the two models.

The synchronous model also makes results deterministic and reproducible,
which is a prerequisite for fair multi-policy comparison.

---

## 2. Why We Dropped Throughput

Throughput (operations per second) is the natural first instinct for a
storage benchmark, and it is the right metric when you are measuring an I/O
subsystem. It is the wrong metric for a *policy* benchmark. Here is why.

### The serialization problem

In our synchronous model, `reorganize()` executes on the same thread as the
workload. A policy that moves many files — either because it is aggressive
about promoting promising candidates or because it is thrashing — will spend
more wall-clock time in `reorganize()` and therefore appear slower, even if
its placement decisions are *better*. A policy that never moves anything has
zero reorganization overhead and maximal throughput but will score 0% hit
rate. Throughput optimizes for the wrong objective.

### It is invisible in production

In a real deployment the daemon runs asynchronously. Its compute time and
I/O cost are completely invisible to the application making filesystem calls.
The application sees only whether its target file is on fast storage or slow
storage — i.e., whether the policy kept the right files hot. Throughput of
the daemon itself is not something operators monitor or optimize.

### What throughput can tell you

Throughput is meaningful when you are benchmarking the *I/O subsystem*:
comparing NVMe vs. HDD, measuring the cost of a specific filesystem call,
or sizing a storage array. It is not meaningful when the variable under test
is which files the policy decided to move. We are testing the latter.

---

## 3. What We Measure and Why

### Hit rate (primary metric)

```
hit rate = hot edits / total edits   (during measurement window)
```

An "edit" is a write to an existing file. An edit is a **hot hit** if the
file at that path is a regular file (content is on hot storage). It is a
**cold miss** if the path is a symlink (content has been demoted to cold
storage). Hit rate directly measures placement quality: a policy that keeps
the working set hot approaches 1.0; a policy that evicts the wrong files
approaches 0.0.

This maps cleanly to the thing operators actually care about in a tiered
cloud filesystem: what fraction of client I/O is served from fast storage?

### Bytes written per tier (secondary metric)

The daemon moves files between tiers. Each move is a disk write — and in
cloud storage, writes are not free. We track:

- **`→hot_KB`** — bytes written to the hot tier (promotions, cold → hot)
- **`→cld_KB`** — bytes written to cold tier 0 (demotions, hot → cold)

Two policies might achieve the same hit rate but with very different I/O
costs. A policy that thrashes — repeatedly promoting and then re-demoting the
same files — wastes write budget and wears SSDs. The bytes-per-tier metric
reveals this.

In a cloud context, cold storage is typically object storage (Amazon S3,
Google Cloud Storage, Azure Blob). Every PUT request has a per-request charge
and a per-GB charge. Unnecessary demotions literally cost money. A policy
that achieves 80% hit rate with 100 MB of cold writes is better than one
that achieves 82% hit rate with 500 MB of cold writes, depending on the
pricing model.

### Promotion and demotion counts

Counts (not just bytes) matter because many cloud storage systems charge per
API call regardless of object size. A policy that promotes 500 small files
may cost more in API fees than one that promotes 5 large files, even if the
total byte volume is similar.

### What we do not measure (and why)

- **Latency of individual operations**: In a real tiered filesystem, a cold
  miss incurs a network round-trip to object storage (tens to hundreds of
  milliseconds). We do not simulate this delay because it would dominate
  every other signal. Hit rate already captures it implicitly: lower hit rate
  → more cold accesses → higher observed latency in production.

- **Throughput of the workload**: As discussed above, this conflates policy
  quality with reorganization overhead and is not meaningful for policy
  comparison.

---

## 4. How This Differs from Page Eviction and Thread Scheduling

### Page eviction (OS virtual memory)

| Dimension | Page eviction | Filesystem tiering |
|---|---|---|
| **Granularity** | 4 KB pages | Files (KB to GB) |
| **Timescale** | Nanoseconds to microseconds | Seconds to minutes |
| **Cold penalty** | Disk I/O (~ms), uniform | Network I/O (10–500 ms), variable |
| **Move cost** | Free (RAM ↔ swap is already accounted) | Charged per byte, per API call |
| **Policy latency** | Must be O(1) or O(log n), runs in interrupt context | Can be O(n log n), runs async |
| **Identity** | Anonymous address → no stable semantic | Named path → stable access pattern |
| **Deletes** | Pages are freed automatically | Deleting a just-demoted file = wasted move |

Page eviction operates at a timescale where the working set can shift within
a single system call. The policy has no wall-clock time budget; it must make
its decision in the time it takes to handle a page fault. Filesystem tiering
operates at a timescale where the daemon can afford to scan access logs,
consult frequency counts, and batch multiple moves in a single reorganization
pass.

Page eviction also has a uniform "cold" cost: a page miss always means a
swap read, which is a local disk I/O of roughly fixed latency. Filesystem
tiering has a heterogeneous cost structure: cold tier 0 might be an HDD
(20 ms), cold tier 1 might be object storage (200 ms), and the pricing model
differs for each.

Finally, page eviction has no economic cost model. Swapping a page is
"free" in the sense that you are not billed per swap operation. Demoting a
file to S3 costs money. This makes the write-efficiency of the policy a
first-class concern in a way it simply is not for page eviction.

### Thread scheduling

Thread scheduling is even further removed. The scheduler's job is to allocate
CPU time fairly and with low latency across competing threads. The working
set concept does not apply: threads do not have "hot" or "cold" states, and
there is no economic cost to context-switching. The policy must run in
microseconds and make a binary choice (run this thread or not) rather than a
placement decision with a continuous cost function. Frequency and recency of
access, the core signals exploited by LRU/LFU/ARC/LRU-2Q, are largely
irrelevant to CPU scheduling (though they appear in related problems like
cache partitioning for hyperthreaded cores).

### What tiering shares with page eviction

The *algorithmic intuition* is the same: given a limited fast-tier capacity
and an unbounded set of candidate objects, decide which objects to keep hot
and which to evict to slow storage, using only the history of past accesses.
This is why LRU, LFU, ARC, and 2Q all transfer from the page-eviction
literature to filesystem tiering. The policies are the same; the operating
constraints, cost model, and timescales are different.

---

## 5. What Production Would Actually Do

In a production cloud filesystem tiering system (e.g., a POSIX-compatible
layer fronting S3):

1. **Hot tier**: NVMe SSD or local instance storage. Low latency (< 1 ms),
   high throughput, limited capacity, high cost per GB.

2. **Cold tier**: Object storage (S3, GCS, Azure Blob). High latency
   (10–500 ms round-trip), high throughput for large sequential reads,
   virtually unlimited capacity, low cost per GB, non-trivial cost per
   API call.

3. **Daemon**: A process running on the storage node, watching inotify/FSEvents
   for `OPEN`, `CLOSE_WRITE`, and `ACCESS` events. It maintains access
   frequency and recency metadata in memory, wakes on a configurable interval
   (typically seconds to minutes), and issues batch PUT/GET operations to
   rebalance the tier distribution.

4. **Cost function**: Operators set a target hot-tier utilization (e.g.,
   "keep hot tier ≤ 80% full") and a cold-access SLA (e.g., "P99 latency
   < 50 ms"). The daemon's policy must maximize hit rate subject to the
   capacity constraint while minimizing cold-write API costs (because
   unnecessary demotions increase the monthly bill).

5. **Multi-tier**: Real systems have more than two tiers. A file might move
   from NVMe → HDD → object storage → archive (e.g., S3 Glacier) as its
   access frequency drops. Our benchmark supports multiple cold tiers
   (`bytes_written_to_tier[i+1]` for cold tier `i`), though current presets
   use a single cold tier for clarity.

Our benchmark captures the essential economics of this system at small scale:
the hot capacity is tightly constrained (a few dozen files fit), the working
set is larger than the hot tier, and the metric that determines whether the
policy is "good" is whether it keeps the right files in the fast tier while
minimizing unnecessary data movement.
