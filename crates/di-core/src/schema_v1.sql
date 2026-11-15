-- drive-index catalog schema, version 1 (PLAN.md §5.2).
-- Timestamps *_ns: INTEGER nanoseconds since the Unix epoch (UTC).
-- Bookkeeping *_at: INTEGER unix seconds. Booleans: 0/1, NULL = unknown.

CREATE TABLE drives (
  id                   INTEGER PRIMARY KEY,
  fingerprint          TEXT NOT NULL,
  label                TEXT,
  notes                TEXT,
  location             TEXT,
  policy               TEXT,
  vendor               TEXT,
  model                TEXT,
  serial               TEXT,
  wwn                  TEXT,
  firmware             TEXT,
  form_factor          TEXT,
  bus                  TEXT,
  transport_driver     TEXT,
  usb_vendor_id        INTEGER,
  usb_product_id       INTEGER,
  usb_serial           TEXT,
  usb_manufacturer     TEXT,
  usb_product          TEXT,
  usb_speed_mbps       INTEGER,
  capacity_bytes       INTEGER NOT NULL,
  logical_block_size   INTEGER,
  physical_block_size  INTEGER,
  medium               TEXT,
  rotation_rpm         INTEGER,
  removable            INTEGER,
  ptable_type          TEXT,
  ptable_uuid          TEXT,
  ptable_raw           BLOB,
  smart_available      INTEGER,
  smart_unavailable_reason TEXT,
  smart_device_type    TEXT,
  tuned_threads        INTEGER,
  first_seen_at        INTEGER NOT NULL,
  last_seen_at         INTEGER NOT NULL,
  last_scanned_at      INTEGER,
  connect_count        INTEGER NOT NULL DEFAULT 0,
  raw_info             TEXT
);

CREATE TABLE drive_identifiers (
  drive_id       INTEGER NOT NULL REFERENCES drives(id) ON DELETE CASCADE,
  kind           TEXT NOT NULL,
  value          TEXT NOT NULL,
  strength       INTEGER NOT NULL,
  first_seen_at  INTEGER NOT NULL,
  last_seen_at   INTEGER NOT NULL,
  PRIMARY KEY (kind, value, drive_id)
) WITHOUT ROWID;

CREATE TABLE partitions (
  id            INTEGER PRIMARY KEY,
  drive_id      INTEGER NOT NULL REFERENCES drives(id) ON DELETE CASCADE,
  number        INTEGER NOT NULL,
  scheme        TEXT NOT NULL,
  start_bytes   INTEGER NOT NULL,
  size_bytes    INTEGER NOT NULL,
  type_id       TEXT,
  type_name     TEXT,
  part_uuid     TEXT,
  part_label    TEXT,
  attr_flags    INTEGER,
  UNIQUE (drive_id, number)
);

CREATE TABLE filesystems (
  id                 INTEGER PRIMARY KEY,
  drive_id           INTEGER NOT NULL REFERENCES drives(id) ON DELETE CASCADE,
  partition_id       INTEGER REFERENCES partitions(id) ON DELETE SET NULL,
  container_uuid     TEXT,
  fs_type            TEXT NOT NULL,
  fs_subtype         TEXT,
  fs_version         TEXT,
  fs_uuid            TEXT,
  fs_serial          TEXT,
  volume_label       TEXT,
  user_label         TEXT,
  apfs_role          TEXT,
  size_bytes         INTEGER,
  used_bytes         INTEGER,
  free_bytes         INTEGER,
  block_size         INTEGER,
  inodes_total       INTEGER,
  inodes_used        INTEGER,
  case_sensitive     INTEGER,
  encrypted          INTEGER,
  locked             INTEGER,
  scannable          INTEGER NOT NULL DEFAULT 1,
  unscannable_reason TEXT,
  last_mount_point   TEXT,
  first_seen_at      INTEGER NOT NULL,
  last_seen_at       INTEGER NOT NULL,
  current_scan_id    INTEGER REFERENCES scans(id),
  file_count         INTEGER,
  dir_count          INTEGER,
  file_bytes         INTEGER,
  raw_info           TEXT
);
CREATE INDEX filesystems_drive ON filesystems(drive_id);
CREATE INDEX filesystems_uuid  ON filesystems(fs_uuid);

CREATE TABLE scans (
  id              INTEGER PRIMARY KEY,
  filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id) ON DELETE CASCADE,
  status          TEXT NOT NULL,
  method          TEXT NOT NULL,
  started_at      INTEGER NOT NULL,
  finished_at     INTEGER,
  active_ms       INTEGER,
  host            TEXT,
  host_os         TEXT,
  host_tz         TEXT,
  app_version     TEXT,
  mount_point     TEXT,
  mount_options   TEXT,
  threads_final   INTEGER,
  files           INTEGER NOT NULL DEFAULT 0,
  dirs            INTEGER NOT NULL DEFAULT 0,
  symlinks        INTEGER NOT NULL DEFAULT 0,
  bytes           INTEGER NOT NULL DEFAULT 0,
  alloc_bytes     INTEGER,
  errors          INTEGER NOT NULL DEFAULT 0,
  hashed_quick    INTEGER NOT NULL DEFAULT 0,
  hashed_full     INTEGER NOT NULL DEFAULT 0,
  media_extracted INTEGER NOT NULL DEFAULT 0,
  content_pending INTEGER,
  options         TEXT
);
CREATE INDEX scans_fs ON scans(filesystem_id, started_at);

CREATE TABLE scan_errors (
  scan_id   INTEGER NOT NULL REFERENCES scans(id) ON DELETE CASCADE,
  path      TEXT NOT NULL,
  op        TEXT NOT NULL,
  errno     INTEGER,
  message   TEXT
);
CREATE INDEX scan_errors_scan ON scan_errors(scan_id);

CREATE TABLE dirs (
  id               INTEGER PRIMARY KEY,
  filesystem_id    INTEGER NOT NULL REFERENCES filesystems(id),
  parent_id        INTEGER REFERENCES dirs(id),
  name             TEXT NOT NULL,
  name_raw         BLOB,
  path             TEXT NOT NULL,
  depth            INTEGER NOT NULL,
  mtime_ns         INTEGER,
  btime_ns         INTEGER,
  mode             INTEGER,
  uid              INTEGER,
  gid              INTEGER,
  inode            INTEGER,
  flags            INTEGER NOT NULL DEFAULT 0,
  entry_count      INTEGER,
  tree_files       INTEGER,
  tree_dirs        INTEGER,
  tree_bytes       INTEGER,
  tree_alloc_bytes INTEGER
);
CREATE INDEX dirs_parent  ON dirs(parent_id);
CREATE INDEX dirs_fs_path ON dirs(filesystem_id, path);

CREATE TABLE files (
  id             INTEGER PRIMARY KEY,
  dir_id         INTEGER NOT NULL REFERENCES dirs(id),
  filesystem_id  INTEGER NOT NULL REFERENCES filesystems(id),
  name           TEXT NOT NULL,
  name_raw       BLOB,
  ext            TEXT,
  kind           INTEGER NOT NULL,
  size           INTEGER NOT NULL,
  alloc_size     INTEGER,
  mtime_ns       INTEGER,
  ctime_ns       INTEGER,
  atime_ns       INTEGER,
  btime_ns       INTEGER,
  mode           INTEGER,
  uid            INTEGER,
  gid            INTEGER,
  inode          INTEGER,
  nlink          INTEGER,
  flags          INTEGER NOT NULL DEFAULT 0,
  link_target    TEXT
);
CREATE INDEX files_dir       ON files(dir_id);
CREATE INDEX files_fs        ON files(filesystem_id);
CREATE INDEX files_ext_cover ON files(ext, filesystem_id, size);
CREATE INDEX files_size      ON files(size);
CREATE INDEX files_mtime     ON files(mtime_ns);

CREATE VIRTUAL TABLE files_fts USING fts5(
  name, content='files', content_rowid='id', tokenize='trigram', detail='none'
);

CREATE TABLE file_hashes (
  file_id    INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
  quick      BLOB,
  full       BLOB,
  hashed_at  INTEGER NOT NULL
);
CREATE INDEX file_hashes_quick ON file_hashes(quick);
CREATE INDEX file_hashes_full  ON file_hashes(full) WHERE full IS NOT NULL;

CREATE TABLE media (
  file_id             INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
  kind                TEXT NOT NULL,
  status              INTEGER NOT NULL,
  extractor           TEXT NOT NULL,
  extracted_at        INTEGER NOT NULL,
  captured_at         TEXT,
  captured_offset_min INTEGER,
  captured_offset_inferred INTEGER,
  width               INTEGER,
  height              INTEGER,
  duration_ms         INTEGER,
  gps_lat             REAL,
  gps_lon             REAL,
  gps_alt_m           REAL,
  camera_make         TEXT,
  camera_model        TEXT,
  camera_serial       TEXT,
  lens_model          TEXT,
  orientation         INTEGER,
  iso                 INTEGER,
  exposure_s          REAL,
  f_number            REAL,
  focal_mm            REAL,
  container           TEXT,
  bitrate_bps         INTEGER,
  title               TEXT,
  artist              TEXT,
  album               TEXT,
  album_artist        TEXT,
  track_no            INTEGER,
  disc_no             INTEGER,
  year                INTEGER,
  genre               TEXT,
  tags                TEXT
);
CREATE INDEX media_captured ON media(captured_at);
CREATE INDEX media_camera   ON media(camera_model);
CREATE INDEX media_audio    ON media(artist, album);

CREATE TABLE media_streams (
  file_id      INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  idx          INTEGER NOT NULL,
  type         TEXT NOT NULL,
  codec        TEXT,
  profile      TEXT,
  width        INTEGER,
  height       INTEGER,
  fps          REAL,
  bitrate_bps  INTEGER,
  sample_rate  INTEGER,
  channels     INTEGER,
  bit_depth    INTEGER,
  language     TEXT,
  duration_ms  INTEGER,
  PRIMARY KEY (file_id, idx)
) WITHOUT ROWID;
CREATE INDEX media_streams_codec ON media_streams(codec);

CREATE TABLE smart_snapshots (
  id                    INTEGER PRIMARY KEY,
  drive_id              INTEGER NOT NULL REFERENCES drives(id) ON DELETE CASCADE,
  taken_at              INTEGER NOT NULL,
  passed                INTEGER,
  temperature_c         INTEGER,
  power_on_hours        INTEGER,
  power_cycles          INTEGER,
  reallocated_sectors   INTEGER,
  pending_sectors       INTEGER,
  offline_uncorrectable INTEGER,
  udma_crc_errors       INTEGER,
  nvme_percentage_used  INTEGER,
  nvme_available_spare  INTEGER,
  nvme_media_errors     INTEGER,
  nvme_unsafe_shutdowns INTEGER,
  data_written_bytes    INTEGER,
  raw_json              TEXT
);
CREATE INDEX smart_drive ON smart_snapshots(drive_id, taken_at);

CREATE TABLE events (
  id        INTEGER PRIMARY KEY,
  ts        INTEGER NOT NULL,
  level     TEXT NOT NULL,
  drive_id  INTEGER REFERENCES drives(id) ON DELETE SET NULL,
  kind      TEXT NOT NULL,
  message   TEXT NOT NULL,
  data      TEXT
);

CREATE TABLE ext_categories (
  ext       TEXT PRIMARY KEY,
  category  TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX ext_categories_category ON ext_categories(category);

CREATE TABLE collections (
  id           INTEGER PRIMARY KEY,
  slug         TEXT NOT NULL UNIQUE,
  name         TEXT NOT NULL,
  query        TEXT NOT NULL,
  description  TEXT
);

CREATE TABLE meta (
  key    TEXT PRIMARY KEY,
  value  TEXT
) WITHOUT ROWID;

CREATE TABLE sessions (
  id           INTEGER PRIMARY KEY,
  uuid         TEXT NOT NULL UNIQUE,
  host         TEXT NOT NULL,
  host_os      TEXT,
  app_version  TEXT,
  started_at   INTEGER NOT NULL,
  ended_at     INTEGER,
  scans        INTEGER NOT NULL DEFAULT 0
);

CREATE VIEW v_files AS
SELECT f.id,
       d.id AS drive_id, d.label AS drive_label, d.fingerprint,
       fs.id AS filesystem_id, COALESCE(fs.user_label, fs.volume_label) AS fs_label, fs.fs_type,
       CASE di.path WHEN '/' THEN '/' || f.name ELSE di.path || '/' || f.name END AS path,
       f.name, f.ext, f.kind, f.size, f.alloc_size,
       datetime(f.mtime_ns / 1000000000, 'unixepoch') AS mtime,
       datetime(f.btime_ns / 1000000000, 'unixepoch') AS btime,
       f.flags, h.quick, h.full,
       m.captured_at, m.camera_model, m.width, m.height, m.duration_ms, m.artist, m.album, m.title
FROM files f
JOIN dirs di        ON di.id = f.dir_id
JOIN filesystems fs ON fs.id = f.filesystem_id
JOIN drives d       ON d.id = fs.drive_id
LEFT JOIN file_hashes h ON h.file_id = f.id
LEFT JOIN media m       ON m.file_id = f.id;

CREATE VIEW v_partitions AS
SELECT p.*, p.start_bytes / d.logical_block_size AS start_lba,
       (p.start_bytes + p.size_bytes) / d.logical_block_size - 1 AS end_lba
FROM partitions p JOIN drives d ON d.id = p.drive_id;
