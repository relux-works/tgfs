# EXPLAIN evidence — synthetic large account

Fixture: `gramdrive_testkit::synthetic::SyntheticSpec::large_account()` (seed 0x6772616d64727601).

| rows | count |
|---|---|
| chats | 2048 |
| messages | 110000 |
| message_events | 117739 |
| attachments | 25339 |
| items | 41935 |
| transfers | 5192 |
| cache_entries | 2815 |
| render_state | 6434 |


## item_by_id

Serves: provider metadata lookup by stable ItemId (DOM-024).

```sql
SELECT kind, display_name, logical_size FROM items WHERE item_id = ?1
```

```text
SEARCH items USING INDEX sqlite_autoindex_items_1 (item_id=?)
```

## children_page

Serves: paged enumeration anchored at the last returned child (SYNC-003).

```sql
SELECT item_id, safe_name FROM items
              WHERE parent_item_id = ?1 AND deleted_at_ms IS NULL AND item_id > ?2
              ORDER BY item_id LIMIT 200
```

```text
SEARCH items USING INDEX items_children_by_id (parent_item_id=? AND item_id>?)
```

## child_by_name

Serves: path resolution one component at a time (DOM-005).

```sql
SELECT item_id FROM items
              WHERE parent_item_id = ?1 AND safe_name = ?2 AND deleted_at_ms IS NULL
```

```text
SEARCH items USING INDEX items_sibling_name (parent_item_id=? AND safe_name=?)
```

## appearances_of_canonical

Serves: propagating a canonical change to every view (SYNC-026).

```sql
SELECT item_id, view_kind FROM items WHERE canonical_item_id = ?1
```

```text
SEARCH items USING INDEX items_appearance (canonical_item_id=?)
```

## chat_list_order

Serves: order.json regeneration and app-UI order (POL-1).

```sql
SELECT chat_id, pinned, sort_order FROM chat_list_entries
              WHERE account_id = ?1 AND namespace_version = ?2
                AND list_kind = ?3 AND folder_id = ?4
              ORDER BY pinned DESC, sort_order DESC
```

```text
SEARCH chat_list_entries USING COVERING INDEX chat_list_entries_order (account_id=? AND namespace_version=? AND list_kind=? AND folder_id=?)
```

## chat_messages_by_id_range

Serves: resumable, idempotent history traversal (SYNC-021).

```sql
SELECT message_id, sent_at_ms FROM messages
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND message_id > ?4
              ORDER BY message_id LIMIT 500
```

```text
SEARCH messages USING PRIMARY KEY (account_id=? AND namespace_version=? AND chat_id=? AND message_id>?)
```

## chat_messages_by_time_window

Serves: month/year partition rendering (SYNC-031).

```sql
SELECT message_id FROM messages
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND sent_at_ms >= ?4 AND sent_at_ms < ?5
```

```text
SEARCH messages USING COVERING INDEX messages_by_time (account_id=? AND namespace_version=? AND chat_id=? AND sent_at_ms>? AND sent_at_ms<?)
```

## chat_event_tail

Serves: render catch-up from a watermark (SYNC-022, SYNC-024).

```sql
SELECT event_seq, event_kind FROM message_events
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND event_seq > ?4
              ORDER BY event_seq
```

```text
SEARCH message_events USING INDEX message_events_by_chat (account_id=? AND namespace_version=? AND chat_id=? AND event_seq>?)
```

## message_event_history

Serves: Audit-mode revision history of one message (POL-3).

```sql
SELECT event_seq, event_kind FROM message_events
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND message_id = ?4
              ORDER BY event_seq
```

```text
SEARCH message_events USING INDEX message_events_by_message (account_id=? AND namespace_version=? AND chat_id=? AND message_id=?)
```

## attachments_of_message

Serves: attachment listing while rendering a message.

```sql
SELECT attachment_index, original_name, logical_size FROM attachments
              WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                AND message_id = ?4
```

```text
SEARCH attachments USING PRIMARY KEY (account_id=? AND namespace_version=? AND chat_id=? AND message_id=?)
```

## attachments_by_blob

Serves: who still references these bytes (SYNC-052).

```sql
SELECT chat_id, message_id, attachment_index FROM attachments
              WHERE account_id = ?1 AND blob_hash_algo = 'sha256' AND blob_hash = ?2
```

```text
SEARCH attachments USING COVERING INDEX attachments_by_blob (account_id=? AND blob_hash_algo=? AND blob_hash=?)
```

## transfer_queue_head

Serves: scheduler picking the next hydration (SYNC-040).

```sql
SELECT transfer_id, item_id FROM transfers
              WHERE state = 'queued'
              ORDER BY priority DESC, transfer_id LIMIT 1
```

```text
SEARCH transfers USING INDEX transfers_queue (state=?)
```

## live_transfer_for_item_version

Serves: coalescing concurrent requests (SYNC-046).

```sql
SELECT transfer_id, state FROM transfers
              WHERE item_id = ?1 AND content_version = ?2
```

```text
SEARCH transfers USING INDEX transfers_by_item (item_id=? AND content_version=?)
```

## eviction_candidates

Serves: LRU eviction over eligible content only (POL-2, SYNC-051/052).

```sql
SELECT item_id, size FROM cache_entries
              WHERE pinned = 0 AND verification = 'verified'
              ORDER BY last_access_at_ms LIMIT 64
```

```text
SCAN cache_entries USING INDEX cache_entries_eviction
```

## cache_accounting

Serves: quota accounting by category (SYNC-050).

```sql
SELECT kind, sum(size) FROM cache_entries WHERE account_id = ?1 GROUP BY kind
```

```text
SEARCH cache_entries USING COVERING INDEX cache_entries_accounting (account_id=?)
```

## cursor_lookup

Serves: restoring the durable change-feed position (SYNC-004, SYNC-022).

```sql
SELECT cursor_text FROM change_cursors WHERE account_id = ?1 AND stream = ?2
```

```text
SEARCH change_cursors USING PRIMARY KEY (account_id=? AND stream=?)
```

## backfill_backlog

Serves: which chats still need history, least-recently synced first (SYNC-021).

```sql
SELECT chat_id FROM chat_sync_state
              WHERE account_id = ?1 AND namespace_version = ?2 AND history_complete = 0
              ORDER BY last_sync_at_ms LIMIT 32
```

```text
SEARCH chat_sync_state USING COVERING INDEX chat_sync_state_backlog (account_id=? AND namespace_version=?)
```

## dirty_render_docs

Serves: the re-render worklist (SYNC-024).

```sql
SELECT item_id FROM render_state WHERE dirty = 1
```

```text
SCAN render_state USING COVERING INDEX render_state_dirty
```
