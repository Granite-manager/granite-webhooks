-- A company's daily export can go to its own Google Cloud Storage bucket
-- instead of an emailed link (GMQ TOPS, 2026-11-20). The API uploads as the
-- service account in GCS_SERVICE_ACCOUNT_JSON, which the company grants
-- Storage Object Creator on that bucket.

-- The bucket name (no gs://); NULL: the zip link is emailed as before.
ALTER TABLE company_exports ADD COLUMN gcs_bucket VARCHAR(222) NULL AFTER alert_recipients;

-- Where the run's zip was uploaded (gs://bucket/name); NULL: emailed.
ALTER TABLE company_export_runs ADD COLUMN gcs_uri VARCHAR(1024) NULL AFTER object_key;
