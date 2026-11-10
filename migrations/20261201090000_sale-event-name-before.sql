-- Admin > Quotes > Automation > Event templates: an event's title can have
-- text before the variable too ("Rough-in - Ostrom Kitchen - Plumbing"), and
-- the built-in template and installation events can be named the same way.
ALTER TABLE quote_sale_event_templates
  ADD COLUMN title_before VARCHAR(200) NOT NULL DEFAULT '' AFTER title_variable;

-- {"title_variable","title_before","title_text"}; NULL: the quote's name alone.
ALTER TABLE company
  ADD COLUMN sale_template_title JSON NULL,
  ADD COLUMN sale_installation_title JSON NULL;
