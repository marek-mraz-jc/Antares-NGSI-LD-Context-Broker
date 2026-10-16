-- The instances that carry a datasetId, a small share of attr_instances: lastN
-- reads an Attribute through its newest rows only when none of them is in a
-- dataset (store/pg/temporal.rs `newest_rows`, T-4246), and this answers that
-- question without reading every row of the Attribute.
CREATE INDEX i_attr_instances_dataset
  ON attr_instances (tenant_id, entity_id, attr_id, observed_at DESC)
  WHERE dataset_id IS NOT NULL;
