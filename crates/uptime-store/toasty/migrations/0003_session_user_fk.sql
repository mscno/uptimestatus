-- Hand-written: sessions belong to an admin; removing the admin ends them.
ALTER TABLE "sessions"
    ADD CONSTRAINT "sessions_user_fk"
    FOREIGN KEY ("user_id") REFERENCES "admin_users" ("id") ON DELETE CASCADE;
