-- Event details > Scheduling call: an automatic call that reads an
-- auto-confirm text to the customer and takes a yes or a no (spoken, or 1 / 2
-- on the keypad). One row per call placed.
CREATE TABLE schedule_calls (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    event_id INT NOT NULL,
    template_id INT NULL DEFAULT NULL,
    -- The appointment's start when the call was placed: a yes confirms the job
    -- only while it has not moved.
    start_date DATETIME NOT NULL,
    phone CHAR(10) NOT NULL,
    -- The scheduling text as the employee sent it: the voice reads it, and it
    -- is texted when the call brings no answer.
    body TEXT NOT NULL,
    placed_by INT NULL DEFAULT NULL,
    placed_at DATETIME NOT NULL,
    call_sid VARCHAR(128) NULL DEFAULT NULL,
    -- Times the question was asked.
    tries INT NOT NULL DEFAULT 0,
    -- Times the customer said "repeat" and heard the message again.
    repeats INT NOT NULL DEFAULT 0,
    -- `calling`: ringing. `asking`: picked up, the voice is asking. `yes` /
    -- `no`: the customer's answer. `unclear`: no yes or no after every try.
    -- `unanswered`: nobody picked up, voicemail, busy, failed, or they hung up.
    status ENUM('calling', 'asking', 'yes', 'no', 'unclear', 'unanswered') NOT NULL DEFAULT 'calling',
    -- What the customer said or pressed last.
    heard VARCHAR(255) NULL DEFAULT NULL,
    ended_at DATETIME NULL DEFAULT NULL,
    INDEX idx_schedule_calls_event (event_id),
    CONSTRAINT fk_schedule_calls_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_calls_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_calls_template
        FOREIGN KEY (template_id) REFERENCES schedule_notification_templates(id) ON DELETE SET NULL,
    CONSTRAINT fk_schedule_calls_placed_by
        FOREIGN KEY (placed_by) REFERENCES users(id) ON DELETE SET NULL
);
