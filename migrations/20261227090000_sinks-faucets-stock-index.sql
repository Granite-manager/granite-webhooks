-- Sinks and faucets in stock per type (quote pricing, inventory lists):
-- without this MySQL read every unattached sink once per type (1 s+ per
-- quote load), which slowed every other page sharing the database.
CREATE INDEX idx_sinks_type_free ON sinks (sink_type_id, is_deleted, slab_id);
CREATE INDEX idx_faucets_type_free ON faucets (faucet_type_id, is_deleted, slab_id);
