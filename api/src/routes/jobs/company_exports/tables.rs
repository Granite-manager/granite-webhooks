//! What a company's export holds: every table of the database is listed here,
//! exported (with the rows that belong to the company) or skipped with the
//! reason. A table missing from [`TABLES`] fails the tests, so a new table is
//! classified when it is added.
//!
//! Rows belong to the company by its `company_id`, or through a column naming
//! a row of another exported table (a deal through its customer, a quote's
//! emails through the quote). Columns holding passwords, keys and tokens are
//! left out ([`is_secret_column`] and [`SECRET_COLUMNS`]).

/// How the rows of an exported table are picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `company_id` is the company.
    Company,
    /// The `company` row of the company.
    CompanyRow,
    /// Every row: shared lists other tables point at (colors, positions).
    All,
    /// `column` names a row (`parent_column`) of the exported table `parent`.
    Parent(&'static str, &'static str, &'static str),
    /// A condition; each `?` is the company's id.
    Where(&'static str),
}

/// Whether a table is exported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Export(Scope),
    /// Left out, and why.
    Skip(&'static str),
}

/// One table of the database.
#[derive(Debug, Clone, Copy)]
pub struct Table {
    pub name: &'static str,
    pub rule: Rule,
}

const fn export(name: &'static str, scope: Scope) -> Table {
    Table {
        name,
        rule: Rule::Export(scope),
    }
}

const fn skip(name: &'static str, reason: &'static str) -> Table {
    Table {
        name,
        rule: Rule::Skip(reason),
    }
}

use Scope::{All, Company, CompanyRow, Parent, Where};

/// Every table of the database.
pub const TABLES: &[Table] = &[
    // Tables with a `company_id`.
    skip("access_tokens", "API access tokens"),
    export("ai_followup_edit_pairs", Company),
    export("ai_followup_offers", Company),
    export("ai_followup_style_preferences", Company),
    export("ai_promise_scans", Company),
    export("ai_promises", Company),
    export("ai_usage_log", Company),
    export("appointment_reminders", Company),
    export("appointment_rep_reminders", Company),
    export("billing_notifications", Company),
    export("bug_report_images", Company),
    export("bug_reports", Company),
    export("calendar_hidden_users", Company),
    export("calendars", Company),
    export("call_coaching_analyses", Company),
    export("chat_log", Company),
    export("chats", Company),
    skip("check_in_devices", "paired check-in devices and their keys"),
    export("checklists", Company),
    export("cloudtalk_contacts", Company),
    export("cloudtalk_sms", Company),
    export("cloudtalk_sms_templates", Company),
    export("cloudtalk_sms_thread_reads", Company),
    export("commission_base_pay", Company),
    export("commission_rules", Company),
    export("commission_settings", Company),
    export("company_billing", Company),
    export("company_billing_invoices", Company),
    export("company_domain_trust", Company),
    skip("company_export_files", "this export's own bookkeeping"),
    skip("company_export_runs", "this export's own bookkeeping"),
    skip("company_exports", "this export's own bookkeeping"),
    export("company_position_sections", Company),
    export("company_setup_checklist", Company),
    export("crew_positions", Company),
    export("customer_phone_index", Company),
    export("customer_phone_index_builds", Company),
    export("customer_sources", Company),
    export("customer_surveys", Company),
    skip("customer_view_codes", "customer sign-in codes"),
    export("customers", Company),
    export("deal_activities", Company),
    export("deal_activity_suggestion_hides", Company),
    export("deal_note_comments", Company),
    export("deal_notes", Company),
    export("deal_stage_activity_rules", Company),
    export("delayed_emails", Company),
    export("delayed_sms", Company),
    skip("device_sessions", "phone app sign-in sessions"),
    export("documents", Company),
    export("email_templates", Company),
    export("emails", Company),
    export("event_attachments", Company),
    export("event_comments", Company),
    export("event_history", Company),
    export("event_images", Company),
    export("event_issues", Company),
    export("event_people", Company),
    export("event_qbo_invoices", Company),
    export("faucet_type", Company),
    export("followup_digest_sends", Company),
    export("groups_list", Company),
    export("images", Company),
    export("images_folders", Company),
    export("instructions", Company),
    export("invoices", Company),
    export("maintenance_completions", Company),
    export("maintenance_conditions", Company),
    export("maintenance_due_digest_sends", Company),
    export("maintenance_items", Company),
    export("maintenance_sub_items", Company),
    export("marketing_refferal_sources", Company),
    export("message_feedback", Company),
    export("native_signature_audit", Company),
    export("native_signature_deliveries", Company),
    export("native_signature_delivery_items", Company),
    export("native_signature_documents", Company),
    export("native_signature_finalizations", Company),
    export("native_signature_jobs", Company),
    skip("native_signature_sessions", "e-signature signing sessions"),
    export("onboarding_analytics_events", Company),
    export("onboarding_feedback", Company),
    export("online_quote_options", Company),
    export("phone_audit_log", Company),
    export("phone_blocked_calls", Company),
    export("phone_call_block_list", Company),
    export("phone_call_blocking", Company),
    export("phone_call_events", Company),
    export("phone_call_points", Company),
    export("phone_call_rules", Company),
    export("phone_calls", Company),
    export("phone_contact_groups", Company),
    skip(
        "phone_devices",
        "phone app devices, push and SIP credentials",
    ),
    export("phone_port_events", Company),
    export("phone_port_loa_invites", Company),
    export("phone_port_orders", Company),
    export("phone_user_settings", Company),
    export("price_lists", Company),
    export("project_activities", Company),
    export("project_files", Company),
    export("project_notes", Company),
    export("project_part_types", Company),
    export("project_parts", Company),
    export("project_process_steps", Company),
    export("project_raptor_jobs", Company),
    export("project_stage_auto", Company),
    export("project_stage_events", Company),
    export("project_stage_history", Company),
    export("project_stage_notify", Company),
    export("project_stage_notify_rep", Company),
    export("projects", Company),
    export("provider_call_syncs", Company),
    export("provider_calls", Company),
    export("qbo_invoice_draft_outcomes", Company),
    export("qbo_invoice_draft_words", Company),
    export("qbo_invoice_instructions", Company),
    export("qbo_invoice_scans", Company),
    export("qbo_invoice_splits", Company),
    export("qbo_invoice_style", Company),
    export("questions", Company),
    export("quote_commissions", Company),
    export("quote_email_templates", Company),
    export("quote_extras", Company),
    export("quote_forms", Company),
    export("quote_invoice_sync", Company),
    export("quote_payment_terms", Company),
    export("quote_sale_event_templates", Company),
    export("quote_settings", Company),
    export("quote_tax_rates", Company),
    export("quotes", Company),
    export("referral_source_aliases", Company),
    export("ringcentral_contacts", Company),
    export("ringcentral_sms", Company),
    export("ringcentral_sms_templates", Company),
    export("ringcentral_sms_thread_reads", Company),
    skip("ringcentral_user_logins", "RingCentral sign-in tokens"),
    export("sale_project_notes", Company),
    export("sales", Company),
    export("sales_orders", Company),
    export("sales_questionnaires", Company),
    export("schedule_notification_flows", Company),
    export("schedule_notification_lists", Company),
    export("schedule_notification_rules", Company),
    export("schedule_notification_templates", Company),
    export("schedule_text_answers", Company),
    export("schedule_time_rules", Company),
    export("scheduled_emails", Company),
    export("scheduled_sms", Company),
    export("sent_templates", Company),
    export("signature_requests", Company),
    export("sink_type", Company),
    export("sink_types", Company),
    export("sms_company_phones", Company),
    export("sms_flow_enrollments", Company),
    export("sms_flows", Company),
    export("sms_opt_outs", Company),
    export("statistics_widget_layouts", Company),
    export("stone_finishes", Company),
    export("stone_materials", Company),
    export("stone_types", Company),
    export("stones", Company),
    export("suppliers", Company),
    export("supports", Company),
    export("telegram_lead_messages", Company),
    export("telnyx_company_config", Company),
    export("telnyx_numbers", Company),
    export("telnyx_sms", Company),
    export("telnyx_sms_thread_reads", Company),
    export("tendlc_numbers", Company),
    export("tendlc_registrations", Company),
    export("tendlc_webhook_events", Company),
    export("user_dashboard_widgets", Company),
    export("user_feature_task_sightings", Company),
    export("user_feature_tasks", Company),
    export("user_feature_usage", Company),
    export("user_onboarding", Company),
    export("user_onboarding_modules", Company),
    export("user_points_ledger", Company),
    export("user_schedule_colors", Company),
    export("user_sms_identities", Company),
    export("users", Company),
    export("users_onboarding_legacy_backup", Company),
    export("users_positions", Company),
    export("widget_analytics_events", Company),
    // Tables without one.
    skip("_sqlx_migrations", "database bookkeeping"),
    skip("_sqlx_test_databases", "database bookkeeping"),
    skip(
        "activity_points_sources",
        "the app's own settings, the same for every company",
    ),
    skip(
        "ai_model_pricing",
        "the app's own settings, the same for every company",
    ),
    export("ai_promise_digests", Parent("user_id", "users", "id")),
    export("answer_attempts", Parent("employee_id", "users", "id")),
    export("answer_choices", Parent("question_id", "questions", "id")),
    export("chat_history", Parent("user_id", "users", "id")),
    export("chat_messages", Parent("chat_id", "chats", "id")),
    export(
        "cloudtalk_sms_attachments",
        Parent("cloudtalk_sms_id", "cloudtalk_sms", "id"),
    ),
    export("colors", All),
    export("company", CompanyRow),
    export(
        "contact_sync_attempts",
        Parent("customer_id", "customers", "id"),
    ),
    skip("crm_manual_overrides", "the app's own manual"),
    export("customers_emails", Parent("customer_id", "customers", "id")),
    export(
        "customers_history",
        Parent("customer_id", "customers", "id"),
    ),
    export("deal_stage_history", Parent("deal_id", "deals", "id")),
    export("deals", Parent("customer_id", "customers", "id")),
    export("deals_documents", Parent("deal_id", "deals", "id")),
    export("deals_images", Parent("deal_id", "deals", "id")),
    export("deals_list", Parent("group_id", "groups_list", "id")),
    export("email_attachments", Parent("email_id", "emails", "id")),
    export("email_participants", Parent("email_id", "emails", "id")),
    skip("email_processing_claims", "short-lived processing locks"),
    export("email_reads", Parent("message_id", "emails", "message_id")),
    export(
        "email_template_attachments",
        Parent("template_id", "email_templates", "id"),
    ),
    export(
        "event_issue_images",
        Parent("issue_id", "event_issues", "id"),
    ),
    export(
        "event_notification_picks",
        Parent("event_id", "events", "id"),
    ),
    export(
        "event_notification_skips",
        Parent("event_id", "events", "id"),
    ),
    // On the company's calendars; without a calendar, made by its users.
    export(
        "events",
        Where(
            "calendar_id IN (SELECT id FROM calendars WHERE company_id = ?) \
             OR (calendar_id IS NULL AND created_user_id IN (SELECT id FROM users WHERE company_id = ?))",
        ),
    ),
    export("faucets", Parent("faucet_type_id", "faucet_type", "id")),
    export("follow_ups", Parent("telnyx_call_id", "telnyx_calls", "id")),
    skip(
        "global_feature_flags",
        "the app's own settings, the same for every company",
    ),
    export(
        "installed_faucets",
        Parent("faucet_id", "faucet_type", "id"),
    ),
    export("installed_sinks", Parent("sink_id", "sink_type", "id")),
    export("installed_stones", Parent("stone_id", "stones", "id")),
    export(
        "instruction_access",
        Parent("instruction_id", "instructions", "id"),
    ),
    skip("login_attempts", "sign-in attempts"),
    export("notification_email_hides", Parent("user_id", "users", "id")),
    export("notifications", Parent("user_id", "users", "id")),
    skip("password_reset_tokens", "password reset links"),
    export("phone_call_legs", Parent("call_id", "phone_calls", "id")),
    export("phone_call_views", Parent("call_id", "phone_calls", "id")),
    export(
        "phone_contact_group_hidden",
        Parent("group_id", "phone_contact_groups", "id"),
    ),
    export(
        "phone_contact_group_members",
        Parent("group_id", "phone_contact_groups", "id"),
    ),
    skip("phone_job_marks", "the app's own job bookkeeping"),
    skip("phone_notice_acks", "phone app devices"),
    export("positions", All),
    skip("posthog_autofix_notices", "the app's own error tracking"),
    skip("posthog_autofix_runs", "the app's own error tracking"),
    export(
        "price_list_revisions",
        Parent("price_list_id", "price_lists", "id"),
    ),
    export(
        "project_process_step_auto_off",
        Parent("project_id", "projects", "id"),
    ),
    export(
        "project_process_step_events",
        Parent("step_id", "project_process_steps", "id"),
    ),
    export(
        "project_process_step_notify",
        Parent("step_id", "project_process_steps", "id"),
    ),
    export(
        "project_process_steps_done",
        Parent("project_id", "projects", "id"),
    ),
    export("quote_autosaves", Parent("quote_id", "quotes", "id")),
    export(
        "quote_email_views",
        Parent("quote_email_id", "quote_emails", "id"),
    ),
    export("quote_emails", Parent("quote_id", "quotes", "id")),
    export("quote_prints", Parent("quote_id", "quotes", "id")),
    export("quote_revisions", Parent("quote_id", "quotes", "id")),
    export(
        "ringcentral_sms_attachments",
        Parent("ringcentral_sms_id", "ringcentral_sms", "id"),
    ),
    export(
        "ringcentral_user_contacts",
        Parent("user_id", "users", "id"),
    ),
    export(
        "sales_order_items",
        Parent("order_id", "sales_orders", "id"),
    ),
    export(
        "sales_order_slabs",
        Parent("item_id", "sales_order_items", "id"),
    ),
    export(
        "sales_questionnaire_questions",
        Parent("questionnaire_id", "sales_questionnaires", "id"),
    ),
    export(
        "sales_questionnaire_responses",
        Parent("question_id", "sales_questionnaire_questions", "id"),
    ),
    export(
        "schedule_notification_sends",
        Parent("rule_id", "schedule_notification_rules", "id"),
    ),
    export(
        "schedule_notification_template_calendars",
        Parent("template_id", "schedule_notification_templates", "id"),
    ),
    skip("sessions", "website sign-in sessions"),
    export("sinks", Parent("sink_type_id", "sink_type", "id")),
    export("slab_inventory", Parent("stone_id", "stones", "id")),
    export("sms_flow_steps", Parent("flow_id", "sms_flows", "id")),
    export(
        "statistics_widget_layout_items",
        Parent("layout_id", "statistics_widget_layouts", "id"),
    ),
    export("stone_colors", Parent("stone_id", "stones", "id")),
    export("stone_image_links", Parent("stone_id", "stones", "id")),
    export("stone_slab_links", Parent("stone_id", "stones", "id")),
    export("stripe_payments", Parent("sale_id", "sales", "id")),
    skip("stripe_webhook_events", "the app's own Stripe bookkeeping"),
    export(
        "supplier_file_chunks",
        Parent("supplier_file_id", "supplier_files", "id"),
    ),
    export("supplier_files", Parent("supplier_id", "suppliers", "id")),
    // AI line calls to or from the company's numbers.
    export(
        "telnyx_calls",
        Where(
            "to_number IN (SELECT e164 FROM telnyx_numbers WHERE company_id = ?) \
             OR from_number IN (SELECT e164 FROM telnyx_numbers WHERE company_id = ?)",
        ),
    ),
    export(
        "telnyx_number_users",
        Parent("number_id", "telnyx_numbers", "id"),
    ),
    export(
        "telnyx_sms_attachments",
        Parent("telnyx_sms_id", "telnyx_sms", "id"),
    ),
    export("todolist", Parent("user_id", "users", "id")),
    export("user_template_order", Parent("user_id", "users", "id")),
    export("user_template_order_mode", Parent("user_id", "users", "id")),
    export("user_template_usage", Parent("user_id", "users", "id")),
];

/// Tables of the production database that no migration creates (made by
/// hand or by the old app); the test database does not have them.
pub const PRODUCTION_ONLY_TABLES: &[&str] = &[
    "_sqlx_test_databases",
    "onboarding_analytics_events",
    "users_onboarding_legacy_backup",
];

/// Columns left out of exported tables: passwords, provider keys and sign-in
/// codes. [`is_secret_column`] leaves out more by name.
pub const SECRET_COLUMNS: &[(&str, &str)] = &[
    ("company", "qbo_client_id"),
    ("company", "qbo_client_secret"),
    ("company", "qbo_access_token"),
    ("company", "qbo_refresh_token"),
    ("company", "qbo_access_expires"),
    ("company", "qbo_refresh_expires"),
    ("company", "cloudtalk_access_key"),
    ("company", "cloudtalk_access_secret"),
    ("company", "ringcentral_client_id"),
    ("company", "ringcentral_client_secret"),
    ("company", "ringcentral_jwt"),
    ("company", "ringcentral_access_token"),
    ("company", "ringcentral_refresh_token"),
    ("company", "ringcentral_access_expires"),
    ("company", "ringcentral_refresh_expires"),
    ("company", "ringcentral_subscription_token_hash"),
    ("company", "signwell_api_key"),
    ("company", "raptor_password"),
    ("users", "password"),
    ("users", "telegram_conf_code"),
    ("users", "telegram_conf_expires_at"),
    ("users", "telegram_conf_failures"),
    ("users", "telegram_conf_failed_at"),
    ("users", "temp_telegram_id"),
    ("quotes", "portal_token"),
    ("quote_emails", "token"),
    ("phone_user_settings", "answer_pin_hash"),
    ("phone_port_loa_invites", "token_hash"),
    ("native_signature_documents", "token_hash"),
    ("telnyx_company_config", "webhook_token"),
    ("telnyx_company_config", "credential_connection_id"),
];

/// Whether a column's name says it holds a secret: a password, secret, key,
/// token or hash. Applied to every exported table, so a new secret column is
/// left out even before it is listed in [`SECRET_COLUMNS`].
pub fn is_secret_column(column: &str) -> bool {
    let column = column.to_ascii_lowercase();
    let words: Vec<&str> = column.split('_').collect();
    ["password", "secret", "jwt", "credential", "credentials"]
        .iter()
        .any(|word| column.contains(word))
        || words
            .iter()
            .any(|word| matches!(*word, "token" | "hash" | "pin" | "otp" | "passcode"))
        || column.contains("api_key")
        || column.contains("access_key")
        || column.contains("private_key")
}

/// Whether `table`.`column` is left out of the export.
pub fn column_is_left_out(table: &str, column: &str) -> bool {
    is_secret_column(column)
        || SECRET_COLUMNS
            .iter()
            .any(|(t, c)| *t == table && c.eq_ignore_ascii_case(column))
}

/// The table's entry in [`TABLES`].
pub fn table(name: &str) -> Option<&'static Table> {
    TABLES.iter().find(|table| table.name == name)
}

/// The condition picking the table's rows of the company, and how many times
/// the company's id is bound in it. `None` for a skipped or unknown table.
pub fn condition(name: &str) -> Option<(String, usize)> {
    condition_at(name, 0)
}

fn condition_at(name: &str, depth: usize) -> Option<(String, usize)> {
    // Parents never loop (a test walks every chain); this only stops a typo.
    if depth > 8 {
        return None;
    }
    let Rule::Export(scope) = table(name)?.rule else {
        return None;
    };
    Some(match scope {
        Company => ("`company_id` = ?".to_string(), 1),
        CompanyRow => ("`id` = ?".to_string(), 1),
        All => ("1 = 1".to_string(), 0),
        Where(sql) => (format!("({sql})"), sql.matches('?').count()),
        Parent(column, parent, parent_column) => {
            let (inner, binds) = condition_at(parent, depth + 1)?;
            (
                format!("`{column}` IN (SELECT `{parent_column}` FROM `{parent}` WHERE {inner})"),
                binds,
            )
        }
    })
}

/// A column holding a stored file, put in the zip with the row's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileColumn {
    /// A URL; only files of the app's public bucket are copied (other links
    /// are listed in `files.csv`).
    Url(&'static str, &'static str),
    /// A key in a private bucket, which must start with the given prefix
    /// (`{company}` is the company's id): keys outside the company's folder
    /// are never read.
    Key(&'static str, &'static str, Bucket, &'static str),
}

/// The bucket a [`FileColumn::Key`] is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// The app's bucket (`STORAGE_BUCKET`).
    Main,
    /// The private signatures bucket (`SIGNATURE_STORAGE_BUCKET`).
    Signatures,
    /// The call recordings bucket (`PHONE_RECORDINGS_BUCKET`).
    Recordings,
}

/// Every column holding a stored file.
pub const FILE_COLUMNS: &[FileColumn] = &[
    FileColumn::Url("bug_report_images", "url"),
    FileColumn::Url("company", "logo_url"),
    FileColumn::Url("customers", "attached_file"),
    FileColumn::Url("deals_documents", "image_url"),
    FileColumn::Url("deals_images", "image_url"),
    FileColumn::Url("documents", "src"),
    FileColumn::Url("documents", "url"),
    FileColumn::Url("email_attachments", "url"),
    FileColumn::Url("email_template_attachments", "url"),
    FileColumn::Url("event_attachments", "url"),
    FileColumn::Url("event_images", "url"),
    FileColumn::Url("event_issue_images", "url"),
    FileColumn::Url("faucet_type", "url"),
    FileColumn::Url("images", "url"),
    FileColumn::Url("images_folders", "url"),
    FileColumn::Url("installed_faucets", "url"),
    FileColumn::Url("installed_sinks", "url"),
    FileColumn::Url("installed_stones", "url"),
    FileColumn::Url("phone_user_settings", "hold_music_url"),
    FileColumn::Url("project_files", "url"),
    FileColumn::Url("signature_requests", "file_url"),
    FileColumn::Url("sink_type", "url"),
    FileColumn::Url("slab_inventory", "url"),
    FileColumn::Url("stone_materials", "url"),
    FileColumn::Url("stones", "url"),
    FileColumn::Url("supplier_files", "url"),
    FileColumn::Url("supports", "url"),
    FileColumn::Url("users", "photo_url"),
    FileColumn::Key(
        "cloudtalk_sms_attachments",
        "s3_key",
        Bucket::Main,
        "sms-attachments/{company}/",
    ),
    FileColumn::Key(
        "ringcentral_sms_attachments",
        "s3_key",
        Bucket::Main,
        "sms-attachments/{company}/",
    ),
    FileColumn::Key(
        "telnyx_sms_attachments",
        "s3_key",
        Bucket::Main,
        "sms-attachments/{company}/",
    ),
    FileColumn::Key(
        "phone_calls",
        "recording_key",
        Bucket::Recordings,
        "phone/{company}/",
    ),
    FileColumn::Key(
        "native_signature_documents",
        "prepared_object_key",
        Bucket::Signatures,
        "{company}/signatures/",
    ),
    FileColumn::Key(
        "native_signature_documents",
        "completed_object_key",
        Bucket::Signatures,
        "{company}/signatures/",
    ),
    FileColumn::Key(
        "native_signature_finalizations",
        "completed_object_key",
        Bucket::Signatures,
        "{company}/signatures/",
    ),
    FileColumn::Key(
        "native_signature_finalizations",
        "evidence_object_key",
        Bucket::Signatures,
        "{company}/signatures/",
    ),
];

/// Columns whose name looks like a file's but that hold none of the
/// company's stored files (links to providers' pages, ids).
pub const NOT_FILE_COLUMNS: &[(&str, &str)] = &[
    ("cloudtalk_sms", "media_urls"),
    ("cloudtalk_sms_attachments", "filename"),
    // The same file as `s3_key`.
    ("cloudtalk_sms_attachments", "s3_url"),
    ("cloudtalk_sms_templates", "attachments"),
    ("company", "ringcentral_server_url"),
    ("company_billing_invoices", "hosted_invoice_url"),
    ("email_attachments", "filename"),
    ("email_template_attachments", "filename"),
    ("native_signature_delivery_items", "attachment_index"),
    ("onboarding_analytics_events", "path"),
    ("ringcentral_sms_attachments", "filename"),
    ("ringcentral_sms_attachments", "s3_url"),
    ("signature_requests", "email_attachment_id"),
    ("signature_requests", "filename"),
    ("supplier_file_chunks", "supplier_file_id"),
    ("supplier_files", "file_text"),
    ("telnyx_calls", "recording_url"),
    ("telnyx_company_config", "fallback_texml_url"),
    ("telnyx_company_config", "recording_notice_url"),
    ("telnyx_company_config", "messaging_profile_id"),
    ("telnyx_company_config", "outbound_voice_profile_id"),
    ("telnyx_sms_attachments", "filename"),
    ("telnyx_sms_attachments", "s3_url"),
];

/// Whether a column's name looks like it holds a file (checked by the tests
/// against [`FILE_COLUMNS`] and [`NOT_FILE_COLUMNS`]).
pub fn looks_like_file_column(column: &str) -> bool {
    let column = column.to_ascii_lowercase();
    [
        "url",
        "src",
        "s3_key",
        "object_key",
        "recording_key",
        "file",
        "path",
        "attachment",
    ]
    .iter()
    .any(|word| column.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names_are_left_out() {
        for column in [
            "password",
            "token",
            "token_hash",
            "access_token",
            "refresh_token",
            "portal_token",
            "client_secret",
            "ringcentral_jwt",
            "signwell_api_key",
            "cloudtalk_access_key",
            "answer_pin_hash",
            "code_hash",
            "telnyx_credential_id",
        ] {
            assert!(is_secret_column(column), "{column}");
        }
        for column in [
            "input_tokens",
            "name",
            "email",
            "pinned",
            "is_pinned",
            "pined_bar",
        ] {
            assert!(!is_secret_column(column), "{column}");
        }
        assert!(column_is_left_out("users", "password"));
        assert!(column_is_left_out("company", "qbo_client_id"));
        assert!(!column_is_left_out("users", "email"));
    }

    #[test]
    fn conditions_follow_parents_to_the_company() {
        assert_eq!(condition("customers"), Some(("`company_id` = ?".into(), 1)));
        assert_eq!(condition("company"), Some(("`id` = ?".into(), 1)));
        assert_eq!(condition("colors"), Some(("1 = 1".into(), 0)));
        assert_eq!(
            condition("deal_stage_history"),
            Some((
                "`deal_id` IN (SELECT `id` FROM `deals` WHERE `customer_id` IN \
                 (SELECT `id` FROM `customers` WHERE `company_id` = ?))"
                    .into(),
                1
            ))
        );
        assert_eq!(condition("events").map(|(_, binds)| binds), Some(2));
        assert_eq!(condition("sessions"), None);
        assert_eq!(condition("no_such_table"), None);
    }

    #[test]
    fn every_table_is_listed_once_and_every_parent_is_exported() {
        let mut names: Vec<&str> = TABLES.iter().map(|table| table.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "a table is listed twice");
        for table in TABLES {
            if let Rule::Export(_) = table.rule {
                assert!(condition(table.name).is_some(), "{}", table.name);
            }
        }
    }

    #[test]
    fn file_columns_are_in_exported_tables() {
        for file in FILE_COLUMNS {
            let (FileColumn::Url(table, _) | FileColumn::Key(table, ..)) = file;
            assert!(condition(table).is_some(), "{table}");
        }
    }
}
