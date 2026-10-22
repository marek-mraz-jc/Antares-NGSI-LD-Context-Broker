-- The attribute catalogue (T-4934): every (type, member name) pair a
-- Tenant's Entities ever carried, one row each, kept by the writes. `/types`
-- with details and `/attributes` read the pairs here and probe a few
-- Entities per type through the caller's view, instead of folding every
-- Entity of the Tenant (13-19 s for `/attributes` on dev).
--
-- An insert lists every pair; an update lists only the pairs it adds (a new
-- member, or a new type), so a write that keeps the Entity's shape inserts
-- nothing. `ON CONFLICT DO NOTHING` takes no lock on a listed pair, so
-- writers of one type never queue on its rows. A removed member or a deleted
-- Entity leaves its pairs; the read checks each against a live Entity.
-- The member names are the document's keys, the Entity frame (id, type,
-- createdAt...) included: the reader drops those, as the fold does.
-- Numbered 0023: 0020-0022 are claimed by T-5200 and T-5349.
CREATE TABLE entity_attrs (
  tenant_id text NOT NULL,
  type      text NOT NULL,
  attr      text NOT NULL,
  PRIMARY KEY (tenant_id, type, attr)
);

CREATE FUNCTION note_entity_attrs() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    INSERT INTO entity_attrs (tenant_id, type, attr)
      SELECT NEW.tenant_id, t, k FROM unnest(NEW.types) AS t, jsonb_object_keys(NEW.entity) AS k
      ON CONFLICT DO NOTHING;
  ELSE
    INSERT INTO entity_attrs (tenant_id, type, attr)
      SELECT NEW.tenant_id, t, k FROM unnest(NEW.types) AS t, jsonb_object_keys(NEW.entity) AS k
       WHERE NOT (t = ANY (OLD.types) AND OLD.entity ? k)
      ON CONFLICT DO NOTHING;
  END IF;
  RETURN NULL;
END $$;

CREATE TRIGGER entity_attrs_on_insert AFTER INSERT ON entities
  FOR EACH ROW EXECUTE FUNCTION note_entity_attrs();
CREATE TRIGGER entity_attrs_on_update AFTER UPDATE OF entity, types ON entities
  FOR EACH ROW WHEN (OLD.entity IS DISTINCT FROM NEW.entity OR OLD.types IS DISTINCT FROM NEW.types)
  EXECUTE FUNCTION note_entity_attrs();

-- Triggers first, as in 0012: no Entity written meanwhile slips between the
-- backfill and the trigger.
SELECT set_config('antares.service', 'on', true);
INSERT INTO entity_attrs (tenant_id, type, attr)
  SELECT DISTINCT tenant_id, t, k FROM entities, unnest(types) AS t, jsonb_object_keys(entity) AS k
  ON CONFLICT DO NOTHING;
SELECT set_config('antares.service', '', true);

ALTER TABLE entity_attrs ENABLE ROW LEVEL SECURITY;
ALTER TABLE entity_attrs FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON entity_attrs
  USING (tenant_id = current_setting('antares.tenant', true))
  WITH CHECK (tenant_id = current_setting('antares.tenant', true));
