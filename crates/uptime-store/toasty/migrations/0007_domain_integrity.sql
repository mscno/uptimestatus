-- Hand-written: a page owns its custom domains.
ALTER TABLE "custom_domains"
    ADD CONSTRAINT "custom_domains_page_fk"
    FOREIGN KEY ("page_id") REFERENCES "status_pages" ("id") ON DELETE CASCADE;
