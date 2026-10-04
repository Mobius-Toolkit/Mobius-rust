-- The step of the work that a session does now and that the Owner sees, for example `runs .mobius/check`. `NULL` when the session shows no step.
ALTER TABLE sessions ADD COLUMN phase TEXT;
