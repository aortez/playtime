CREATE TABLE events (
  resource_id TEXT NOT NULL, version INTEGER NOT NULL,
  event_id INTEGER PRIMARY KEY, deleted INTEGER NOT NULL,
  payload TEXT NOT NULL
);
