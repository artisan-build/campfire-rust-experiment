CREATE TABLE active_storage_attachments (id integer PRIMARY KEY AUTOINCREMENT NOT NULL, blob_id bigint NOT NULL, created_at datetime(6) NOT NULL, name varchar NOT NULL, record_id bigint NOT NULL, record_type varchar NOT NULL);
CREATE UNIQUE INDEX index_active_storage_attachments_uniqueness ON active_storage_attachments (record_type, record_id, name, blob_id);
CREATE TABLE active_storage_blobs (id integer PRIMARY KEY AUTOINCREMENT NOT NULL, byte_size bigint NOT NULL, checksum varchar, content_type varchar, created_at datetime(6) NOT NULL, filename varchar NOT NULL, key varchar NOT NULL, metadata text, service_name varchar NOT NULL);
CREATE UNIQUE INDEX index_active_storage_blobs_on_key ON active_storage_blobs (key);
CREATE TABLE active_storage_variant_records (id integer PRIMARY KEY AUTOINCREMENT NOT NULL, blob_id bigint NOT NULL, variation_digest varchar NOT NULL);
CREATE UNIQUE INDEX index_active_storage_variant_records_uniqueness ON active_storage_variant_records (blob_id, variation_digest);
