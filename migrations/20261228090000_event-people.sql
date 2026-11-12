-- People added to an event (the edit form's People row, on the web and in
-- the phone app): the company's users who take part in it, besides the
-- installer or template maker in `events.assigned_user_id`. The event shows
-- on each one's own schedule (`GET /v1/me/schedule`).
CREATE TABLE event_people (
    event_id INT NOT NULL,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (event_id, user_id),
    KEY idx_event_people_user (user_id, company_id),
    CONSTRAINT fk_event_people_event FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_event_people_user FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
