# Witness PIR Rebuild Strategy

The witness server maintains a YPIR database of Orchard note commitment sub-shard
roots. Every time a new block arrives, the database must be rebuilt and handed to
the YPIR engine. This document explains the rebuild pipeline and the incremental
sub-shard root cache that makes follow-mode updates nearly instant.

---

## Tree geometry

| Constant             | Value   | Meaning                                     |
|----------------------|---------|---------------------------------------------|
| `TREE_DEPTH`         | 32      | Full Orchard commitment tree depth           |
| `SHARD_HEIGHT`       | 16      | Levels per shard (2^16 = 65,536 leaves)      |
| `SUBSHARD_HEIGHT`    | 8       | Levels per sub-shard (2^8 = 256 leaves)      |
| `SUBSHARDS_PER_SHARD`| 256     | Sub-shards in one shard                      |
| `L0_MAX_SHARDS`      | 32      | Max shards in the PIR window                 |
| `L0_DB_ROWS`         | 8,192   | Total sub-shards in the window (32 x 256)    |
| `L0_DB_BYTES`        | 64 MB   | PIR database size (8,192 x 8,192 bytes)      |

The PIR database is a flat 64 MB buffer: one row per sub-shard, each row being
256 leaf commitments x 32 bytes = 8,192 bytes.

---

## Rebuild pipeline

`build_pir_db_and_broadcast()` produces two outputs in a single pass:

1. **PIR database** -- the 64 MB flat buffer of leaf commitments, used by YPIR `setup()`
2. **Broadcast payload** -- cap (shard roots) and per-shard sub-shard roots, sent to wallets

For each of the 8,192 sub-shards in the window, the function:

1. Reads 256 leaf commitments from `self.leaves` (padding with the Orchard empty leaf
   sentinel for positions beyond the tree frontier)
2. Writes them into the PIR database buffer
3. Computes the sub-shard root via Sinsemilla hashing (if not cached)

Then for each of the 32 window shards, it combines 256 sub-shard roots into a
shard root via Sinsemilla hashing.

### Cost breakdown

| Operation                        | Count                        | Total        |
|----------------------------------|------------------------------|--------------|
| Sinsemilla hash (sub-shard root) | 8,192 sub-shards x 255 hashes | **~170 s** |
| Sinsemilla hash (shard root)     | 32 shards x 255 hashes       | **~0.6 s** |
| Leaf data copy to db buffer      | 8,192 x 8,192 bytes          | **~0.5 s** |
| YPIR `engine.setup()`            | 1 x 64 MB                    | **~2.7 s** |

The sub-shard root hashing dominates at ~170 seconds. But in follow mode, almost
all sub-shard roots are unchanged between blocks.

---

## Incremental sub-shard root cache

`CommitmentTreeDb` maintains an in-memory cache:

```rust
ss_root_cache: Vec<Option<Hash>>   // length = L0_DB_ROWS (8,192)
```

Each slot maps to one sub-shard in the PIR window. `Some(root)` means the cached
root is valid; `None` means it must be recomputed.

### Cache invalidation

**On `append_commitments()`**: Only the sub-shard slots that received new leaves
are set to `None`. Typically a block touches 1 sub-shard (or 2 if it straddles a
boundary).

```
first_dirty = old_local_len / SUBSHARD_LEAVES
last_dirty  = (new_local_len - 1) / SUBSHARD_LEAVES

for slot in first_dirty..=last_dirty:
    ss_root_cache[slot] = None
```

**On `rollback_to()`**: All slots from the new frontier onward are invalidated,
since any sub-shard at or after the truncation point may have changed.

```
frontier_slot = (new_len - 1) / SUBSHARD_LEAVES
for slot in frontier_slot..L0_DB_ROWS:
    ss_root_cache[slot] = None
```

### Cache usage during rebuild

Inside `build_pir_db_and_broadcast()`, for each sub-shard:

```
if ss_global_start >= total:
    use empty_root (no hashing, no caching)
else if ss_root_cache[slot] is Some(root):
    use cached root (no hashing)          <-- CACHE HIT
else:
    compute root via Sinsemilla           <-- CACHE MISS
    store in ss_root_cache[slot]
```

The leaf data copy (step 2) always happens regardless of cache state -- the PIR
database buffer must always be fully populated for `engine.setup()`.

---

## Operating modes

### Cold start (no snapshot, or v1/v2 snapshot)

```
cache: all 8,192 slots = None
  -> 8,192 cache misses, ~2M Sinsemilla hashes
  -> build_ms ~ 170,000  (170 s)
  -> total_ms ~ 173,000  (+ 2.7 s YPIR setup)
```

### Follow mode (new block, warm cache)

```
cache: ~7,952 slots = Some, 1 slot = None (invalidated by append)
  -> 7,952 cache hits, 1 cache miss, 239 empty
  -> build_ms ~ 700      (0.7 s)
  -> total_ms ~ 3,400    (+ 2.7 s YPIR setup)
```

### Warm restart (v3 snapshot with persisted cache)

```
cache loaded from snapshot: ~7,953 slots = Some
sync catches up a few blocks -> 1-2 slots invalidated
  -> ~7,952 cache hits, ~1 miss
  -> build_ms ~ 700      (0.7 s)
  -> total_ms ~ 3,400    (+ 2.7 s YPIR setup)
```

---

## Snapshot format (v3)

The v3 snapshot (magic `0x434D_5452_4545_0003`) extends v2 with the sub-shard
root cache. After the leaf data section, it appends:

| Field           | Size     | Description                              |
|-----------------|----------|------------------------------------------|
| `cached_count`  | 8 bytes  | Number of non-None cache entries          |
| cached entries  | variable | `cached_count` x (slot: 4 + root: 32)    |

Only populated (`Some`) cache slots are written. On a warm cache this is ~7,953
entries x 36 bytes ~ 280 KB of additional snapshot data.

The server saves a snapshot with the warm cache immediately after the first PIR
rebuild, ensuring subsequent restarts benefit from the cached roots.

Backward compatibility: v1 and v2 snapshots load successfully with a cold cache
(all slots `None`). The first rebuild will be slow (~170 s) but subsequent blocks
will be fast once the cache warms.

---

## Performance summary

| Scenario                   | build_ms | setup_ms | total_ms | Speedup    |
|----------------------------|----------|----------|----------|------------|
| Cold cache (first startup) | 170,000  | 2,700    | 173,000  | baseline   |
| Warm cache (follow mode)   | 700      | 2,700    | 3,400    | **~50x**   |
| Warm restart (v3 snapshot) | 700      | 2,700    | 3,400    | **~50x**   |

The remaining 2.7 s is the YPIR `engine.setup()` cost, which processes the full
64 MB database regardless of how many sub-shards changed. This is a fixed cost
that cannot be reduced by caching.
