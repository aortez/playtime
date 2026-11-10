WITH ranked AS (
  SELECT resource_id, version, event_id, deleted, payload,
         ROW_NUMBER() OVER (
           PARTITION BY resource_id ORDER BY version DESC, event_id DESC
         ) AS rn
  FROM events
  WHERE version <= :cutoff
)
SELECT resource_id, version, event_id, payload
FROM ranked
WHERE rn = 1 AND deleted = 0
ORDER BY resource_id COLLATE BINARY;
