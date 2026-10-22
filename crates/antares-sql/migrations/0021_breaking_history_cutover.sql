-- Integer history keys, slice 2 (T-5243; docs/src/storage.md "Integer
-- history keys"). Sets the cut-over 0020 left NULL to the Monday after this
-- migration runs: from it on the store writes a row observed at or after it
-- to attr_rows, every earlier one (a late write included) to attr_instances.
-- The week boundary is the partition job's own (date_trunc('week', now())),
-- so the first keyed week is a whole partition of attr_rows.
-- Breaking: a release before 0020 reads only attr_instances and would miss
-- every row written to attr_rows. Set once: a rerun or a later boot keeps it.
UPDATE history_cutover
   SET at = date_trunc('week', now()) + interval '1 week'
 WHERE at IS NULL;
