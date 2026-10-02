-- Hand-written: a page owns its sections, a section owns its components, and
-- deleting a monitor removes it from every page.
ALTER TABLE "page_sections"
    ADD CONSTRAINT "page_sections_page_fk"
    FOREIGN KEY ("page_id") REFERENCES "status_pages" ("id") ON DELETE CASCADE;

ALTER TABLE "page_components"
    ADD CONSTRAINT "page_components_section_fk"
    FOREIGN KEY ("section_id") REFERENCES "page_sections" ("id") ON DELETE CASCADE;

ALTER TABLE "page_components"
    ADD CONSTRAINT "page_components_monitor_fk"
    FOREIGN KEY ("monitor_id") REFERENCES "monitors" ("id") ON DELETE CASCADE;
