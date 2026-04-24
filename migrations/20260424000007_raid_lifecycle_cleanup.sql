-- Raid lifecycle is a state queue, not a history store.
--
-- `headcounts` and `runs` were originally designed with a `status` column
-- and ON DELETE RESTRICT references, with the intent of preserving
-- post-raid rows as audit data. But no feature ever reads those rows:
--   * `list_active` filters on status='active' and has no callers.
--   * `load_active` loads historical rows only to bounce them.
--   * `runs.headcount_id` is written on every start but never read.
-- The R4 rework further hollowed out the audit case by moving live
-- signup state off the DB and onto Discord reactions.
--
-- So historical rows are pure garbage, and they cause real problems:
-- they pin `dungeon_template_id` via RESTRICT FKs, so the seeder can't
-- prune renamed global templates (e.g. after the apostrophe-parsing fix
-- the scraper went from `oryx_s_sanctuary` to `oryxs_sanctuary`, and
-- every past raid for the stale name blocks cleanup forever).
--
-- This migration aligns the schema with how the code actually uses it:
-- a row exists iff the raid is live. Terminal transitions delete; no
-- status column needed.

-- Purge everything non-active before dropping the column that defines it.
-- run_participants / headcount_reactions are already gone (R4), so no
-- cascade fallout downstream.
DELETE FROM runs       WHERE status <> 'active';
DELETE FROM headcounts WHERE status <> 'active';

-- headcounts: drop status, its partial index, its CHECK constraint, and
-- the updated_at scaffolding (only ever bumped by set_status calls).
DROP INDEX   idx_headcounts_status;
ALTER TABLE  headcounts DROP CONSTRAINT headcounts_status_check;
ALTER TABLE  headcounts DROP COLUMN status;
DROP TRIGGER trg_headcounts_touch ON headcounts;
ALTER TABLE  headcounts DROP COLUMN updated_at;

-- runs: drop status, ended_at, and the unread headcount_id back-link.
DROP INDEX  idx_runs_status;
ALTER TABLE runs DROP CONSTRAINT runs_status_check;
ALTER TABLE runs DROP COLUMN status;
ALTER TABLE runs DROP COLUMN ended_at;
ALTER TABLE runs DROP COLUMN headcount_id;

-- Prefill support: `/headcount` can stash a starting location/party that
-- carries into the run created on Start. Headcount is now the sole
-- state-carrier between command invocation and run creation, so the
-- fields live here rather than on a separate structure.
ALTER TABLE headcounts
    ADD COLUMN location TEXT,
    ADD COLUMN party    TEXT;
