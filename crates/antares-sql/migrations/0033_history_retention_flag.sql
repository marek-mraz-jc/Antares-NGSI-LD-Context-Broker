-- Lets an Entity update be HOT (T-6607, review finding R2-004). PostgreSQL
-- writes a HOT update (no new index entry, the new version on the same page)
-- only when no column an index reads changed, and the predicate of 0010's
-- partial index read `entity`, the column every write sets. Since 0019
-- dropped the whole-document GIN that predicate was the last index reading
-- `entity`, so every update rewrote every index of the table and left the
-- 15 % page headroom of 0001's fillfactor unused (storage.md, T-6277: the
-- rows of one type scattered over the heap).
--
-- The flag holds `entity ? historyRetention`, kept by a BEFORE trigger, so no
-- write path can forget it; a value write leaves it unchanged and stays HOT.
-- A constant default adds the column without a table rewrite (PostgreSQL 11),
-- and the backfill updates only the few Entities that carry the key. A
-- trigger, not a STORED generated column: adding one rewrites the whole
-- table under ACCESS EXCLUSIVE.
ALTER TABLE entities ADD COLUMN history_retention boolean NOT NULL DEFAULT false;

CREATE FUNCTION note_history_retention() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.history_retention := NEW.entity ? 'https://uri.etsi.org/ngsi-ld/default-context/historyRetention';
  RETURN NEW;
END $$;

-- The trigger comes first (0012): CREATE TRIGGER holds writes to `entities`
-- until this migration commits, so no Entity written meanwhile slips between
-- the backfill and the trigger.
CREATE TRIGGER history_retention_on_write BEFORE INSERT OR UPDATE OF entity ON entities
  FOR EACH ROW EXECUTE FUNCTION note_history_retention();

-- The broker migrates as a NOSUPERUSER owner under FORCE RLS: entities'
-- WITH CHECK and the 0025 statement trigger's entity_attrs insert both want
-- antares.tenant, and the service escape alone reads rows but writes none
-- (T-6610 in 0032). So each Tenant's rows are flagged under that Tenant's
-- own scope, transaction local like set_tenant.
SELECT set_config('antares.service', 'on', true);
DO $$
DECLARE t text;
BEGIN
  FOR t IN SELECT DISTINCT tenant_id FROM entities
            WHERE entity ? 'https://uri.etsi.org/ngsi-ld/default-context/historyRetention' LOOP
    PERFORM set_config('antares.tenant', t, true);
    UPDATE entities SET history_retention = true
     WHERE tenant_id = t
       AND entity ? 'https://uri.etsi.org/ngsi-ld/default-context/historyRetention';
  END LOOP;
  PERFORM set_config('antares.tenant', '', true);
END $$;
SELECT set_config('antares.service', '', true);

-- The retention sweep's set (store/pg/maintenance.rs `history_overrides`),
-- now keyed on the flag.
CREATE INDEX i_entities_history_retention_flag ON entities (tenant_id) WHERE history_retention;
DROP INDEX i_entities_history_retention;
