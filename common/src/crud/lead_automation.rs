//! Whose automated emails and texts go out for a sales rep's deals when a
//! deal lands on a list (Employee CRM › Automations).
//!
//! Each user picks in `users.lead_automation`:
//! - `company` (the default): the company's automated templates
//!   (Admin › Automations), those naming no sales reps or naming this one;
//! - `own`: only the user's own automated templates.

/// The SQL of [`OWN_TEMPLATE`], for `concat!`.
macro_rules! own_template {
    () => {
        "t.user_id <=> ?"
    };
}

/// The SQL of [`COMPANY_TEMPLATE_FOR_REP`], for `concat!`.
macro_rules! company_template_for_rep {
    () => {
        "t.user_id IS NULL AND (t.sales_rep_ids IS NULL OR JSON_LENGTH(t.sales_rep_ids) = 0 \
            OR JSON_CONTAINS(t.sales_rep_ids, CAST(? AS JSON)))"
    };
}

/// SQL condition on a template aliased `t` (`email_templates` or
/// `cloudtalk_sms_templates`): it is the own template of the user bound to
/// its one `?`.
pub const OWN_TEMPLATE: &str = own_template!();

/// SQL condition on a template aliased `t`: a company template that goes to
/// the deals of the user bound to its one `?` while they use the company's
/// automations (it names no sales reps, or names them).
pub const COMPANY_TEMPLATE_FOR_REP: &str = company_template_for_rep!();

/// SQL condition on a drip template aliased `t` (`email_templates` or
/// `cloudtalk_sms_templates`): it goes out for deals of the user bound to
/// each of its three `?`, in order.
pub const DRIP_TEMPLATE_FOR_REP: &str = concat!(
    "IF((SELECT u.lead_automation FROM users u WHERE u.id = ?) <=> 'own', \
       ",
    own_template!(),
    ", \
       ",
    company_template_for_rep!(),
    ")"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scheduler_condition_is_unchanged() {
        // The condition as it was before it was split into its two sides.
        let before = "IF((SELECT u.lead_automation FROM users u WHERE u.id = ?) <=> 'own', \
       t.user_id <=> ?, \
       t.user_id IS NULL AND (t.sales_rep_ids IS NULL OR JSON_LENGTH(t.sales_rep_ids) = 0 \
            OR JSON_CONTAINS(t.sales_rep_ids, CAST(? AS JSON))))";
        assert_eq!(DRIP_TEMPLATE_FOR_REP, before);
        assert_eq!(OWN_TEMPLATE.matches('?').count(), 1);
        assert_eq!(COMPANY_TEMPLATE_FOR_REP.matches('?').count(), 1);
    }
}
