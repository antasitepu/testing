-- Outbox readers page with `cursor > last_seen`. A sequence value is taken when a
-- row is inserted, not when its transaction commits, so a transaction holding a
-- lower cursor could commit after a higher one and be skipped by readers.
-- Assign the cursor under a transaction-scoped advisory lock so cursor order
-- always matches commit order.
ALTER TABLE monitoring_events ALTER COLUMN cursor DROP DEFAULT;

CREATE FUNCTION assign_monitoring_event_cursor() RETURNS trigger AS $$
BEGIN
    -- Arbitrary application-wide key reserved for monitoring_events cursors.
    PERFORM pg_advisory_xact_lock(720600150001);
    NEW.cursor := nextval('monitoring_event_cursor_seq');
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER monitoring_events_assign_cursor
    BEFORE INSERT ON monitoring_events
    FOR EACH ROW EXECUTE FUNCTION assign_monitoring_event_cursor();
