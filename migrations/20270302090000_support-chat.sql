-- Support chat: a CRM user writes to Granite Manager's support team from the
-- Chat panel (the "Support" option); the team answers from the Support app
-- (support.granite-manager.com, superusers with two-step sign-in only).
--
-- support_conversations: one support request of a user. A user has at most
--   one active one (status `open` or `waiting`); writing after it was
--   resolved starts a new one.
--   status:   `open` (the team owes an answer), `waiting` (the team answered,
--             waiting on the client), `resolved`.
--   category: what the request is, set by the team: `feature_request`,
--             `bug`, `how_to`, `billing`, `account_access`, `data_request`,
--             `integration`, `other`; NULL until set.
--   priority: `low`, `normal`, `high`, `urgent`.
--   assignee_id: the superuser handling it; NULL when unassigned.
--   agent_read_at / client_read_at: when the team / the client last read it,
--             against last_client_message_at / last_agent_message_at for the
--             unread marks.
--   updated_at: changes with every write, so the Support app's poll can ask
--             "anything new since?" with one indexed read.
CREATE TABLE support_conversations (
    id                     INT          NOT NULL AUTO_INCREMENT PRIMARY KEY,
    company_id             INT          NOT NULL,
    user_id                INT          NOT NULL,
    status                 VARCHAR(16)  NOT NULL DEFAULT 'open',
    category               VARCHAR(32)  NULL,
    priority               VARCHAR(16)  NOT NULL DEFAULT 'normal',
    assignee_id            INT          NULL,
    subject                VARCHAR(200) NOT NULL,
    last_message_id        BIGINT       NULL,
    last_message_at        DATETIME(3)  NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    last_client_message_at DATETIME(3)  NULL,
    last_agent_message_at  DATETIME(3)  NULL,
    agent_read_at          DATETIME(3)  NULL,
    client_read_at         DATETIME(3)  NULL,
    resolved_at            DATETIME(3)  NULL,
    created_at             DATETIME(3)  NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    updated_at             DATETIME(3)  NOT NULL DEFAULT CURRENT_TIMESTAMP(3)
                                        ON UPDATE CURRENT_TIMESTAMP(3),
    KEY idx_support_conversations_user (user_id, status),
    KEY idx_support_conversations_company (company_id, id),
    KEY idx_support_conversations_status (status, last_message_at),
    KEY idx_support_conversations_updated (updated_at),
    CONSTRAINT fk_support_conversations_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
    CONSTRAINT fk_support_conversations_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_support_conversations_assignee
        FOREIGN KEY (assignee_id) REFERENCES users(id) ON DELETE SET NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- support_messages: one message of a conversation.
--   from_support: written by the team (else by the client).
--   internal:     a note of the team the client never sees.
--   image_urls:   JSON array of pictures uploaded to `bug-reports/`.
--   page_path:    the CRM page the client was on.
--   posthog_session_id / replay_seconds: the client's PostHog session and
--                 how far into its recording they wrote, for the replay link.
--   user_agent / screen: the client's browser and its window size (WxH).
CREATE TABLE support_messages (
    id                 BIGINT       NOT NULL AUTO_INCREMENT PRIMARY KEY,
    conversation_id    INT          NOT NULL,
    author_id          INT          NOT NULL,
    from_support       BOOLEAN      NOT NULL DEFAULT FALSE,
    internal           BOOLEAN      NOT NULL DEFAULT FALSE,
    body               TEXT         NOT NULL,
    image_urls         TEXT         NULL,
    page_path          VARCHAR(500) NULL,
    posthog_session_id VARCHAR(64)  NULL,
    replay_seconds     INT          NULL,
    user_agent         VARCHAR(300) NULL,
    screen             VARCHAR(32)  NULL,
    created_at         DATETIME(3)  NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
    KEY idx_support_messages_conversation (conversation_id, id),
    CONSTRAINT fk_support_messages_conversation
        FOREIGN KEY (conversation_id) REFERENCES support_conversations(id) ON DELETE CASCADE,
    CONSTRAINT fk_support_messages_author
        FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
