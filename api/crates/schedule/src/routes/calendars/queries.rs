use super::schemas::Calendar;
use sqlx::MySqlPool;

/// Built-in calendars every company has: slug, name, color.
pub const SYSTEM_CALENDARS: [(&str, &str, &str); 3] = [
    ("templates", "Templates", "green"),
    ("installations", "Installations", "red"),
    ("estimates", "Estimates", "blue"),
];

/// The colors a calendar can have, in the order a calendar that needs one
/// gets the first free one: every color the schedule draws but black.
/// Templates is always green and Installations always red.
pub const CALENDAR_COLORS: [&str; 14] = [
    "green", "red", "blue", "purple", "orange", "teal", "yellow", "pink", "cyan", "indigo", "lime",
    "slate", "brown", "coral",
];

/// The color a built-in calendar always has (Templates green, Installations
/// red); `None` for the others.
pub fn fixed_color(slug: Option<&str>) -> Option<&'static str> {
    match slug {
        Some("templates") => Some("green"),
        Some("installations") => Some("red"),
        _ => None,
    }
}

/// Each calendar's color so that no two share one (owner, 2026-11-13):
/// Templates green and Installations red, then in schedule order each other
/// calendar keeps its color while it is a known one nobody before took, else
/// takes the first free one. With more calendars than colors the last ones
/// repeat colors in turn. `calendars` is (id, slug, color).
pub fn distinct_colors(calendars: &[(i32, Option<String>, String)]) -> Vec<(i32, String)> {
    let mut taken: Vec<String> = calendars
        .iter()
        .filter_map(|(_, slug, _)| fixed_color(slug.as_deref()).map(str::to_string))
        .collect();
    let mut out = Vec::with_capacity(calendars.len());
    let mut spare = 0;
    for (id, slug, color) in calendars {
        if let Some(fixed) = fixed_color(slug.as_deref()) {
            out.push((*id, fixed.to_string()));
            continue;
        }
        let known = CALENDAR_COLORS.contains(&color.as_str());
        let chosen = if known && !taken.contains(color) {
            color.clone()
        } else if let Some(free) = CALENDAR_COLORS
            .iter()
            .find(|c| !taken.iter().any(|t| t == *c))
        {
            (*free).to_string()
        } else {
            let again = CALENDAR_COLORS[2 + spare % (CALENDAR_COLORS.len() - 2)];
            spare += 1;
            again.to_string()
        };
        taken.push(chosen.clone());
        out.push((*id, chosen));
    }
    out
}

/// Saves distinct colors on the company's live calendars (see
/// [`distinct_colors`]); only the calendars whose color changes are written.
pub async fn ensure_distinct_colors(pool: &MySqlPool, company_id: i32) -> Result<(), sqlx::Error> {
    let calendars: Vec<(i32, Option<String>, String)> = sqlx::query_as(
        "SELECT c.id, c.slug, COALESCE(c.color, '') FROM calendars c
          WHERE c.company_id = ? AND c.deleted_at IS NULL
          ORDER BY CASE c.slug WHEN 'templates' THEN 1 WHEN 'installations' THEN 2
                               WHEN 'estimates' THEN 3 ELSE 4 END, c.created_at, c.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    for (id, color) in distinct_colors(&calendars) {
        let before = calendars
            .iter()
            .find(|(c, _, _)| *c == id)
            .map(|(_, _, c)| c);
        if before != Some(&color) {
            sqlx::query("UPDATE calendars SET color = ? WHERE id = ?")
                .bind(&color)
                .bind(id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// Creates missing built-in calendars and restores their names (colors are
/// the company's choice). Port of `ensureSystemCalendars`.
pub async fn ensure_system_calendars(
    pool: &MySqlPool,
    company_id: i32,
    created_by: i32,
) -> Result<(), sqlx::Error> {
    let existing: Vec<(String, String)> = sqlx::query_as(
        "SELECT slug, name FROM calendars
         WHERE company_id = ? AND slug IN ('templates', 'installations', 'estimates') AND deleted_at IS NULL",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    for (slug, name, color) in SYSTEM_CALENDARS {
        let rows: Vec<&String> = existing
            .iter()
            .filter(|(s, _)| s == slug)
            .map(|(_, n)| n)
            .collect();
        if rows.is_empty() {
            sqlx::query(
                "INSERT INTO calendars (company_id, name, color, slug, created_by)
                 SELECT ?, ?, ?, ?, ? WHERE NOT EXISTS (
                   SELECT 1 FROM calendars WHERE company_id = ? AND slug = ? AND deleted_at IS NULL)",
            )
            .bind(company_id)
            .bind(name)
            .bind(color)
            .bind(slug)
            .bind(created_by)
            .bind(company_id)
            .bind(slug)
            .execute(pool)
            .await?;
        } else if rows.iter().any(|n| n.as_str() != name) {
            sqlx::query(
                "UPDATE calendars SET name = ?, updated_at = CURRENT_TIMESTAMP
                 WHERE company_id = ? AND slug = ? AND deleted_at IS NULL",
            )
            .bind(name)
            .bind(company_id)
            .bind(slug)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Live calendars not hidden for the company nor from `user_id`: built-in
/// ones first (templates, installations, estimates), then by name.
pub async fn visible_calendars(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> Result<Vec<Calendar>, sqlx::Error> {
    sqlx::query_as(
        "SELECT c.id, c.name, c.color, c.slug,
                COALESCE(c.slug IN ('templates', 'installations', 'estimates'), 0) AS is_system,
                FALSE AS is_hidden
         FROM calendars c
         WHERE c.company_id = ? AND c.deleted_at IS NULL AND c.is_hidden = 0
           AND NOT EXISTS (SELECT 1 FROM calendar_hidden_users h
                           WHERE h.calendar_id = c.id AND h.company_id = c.company_id AND h.user_id = ?)
         ORDER BY CASE c.slug WHEN 'templates' THEN 1 WHEN 'installations' THEN 2
                              WHEN 'estimates' THEN 3 ELSE 4 END, c.name, c.id",
    )
    .bind(company_id)
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// Every live calendar of the company, hidden ones too, in schedule order.
pub async fn all_calendars(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<Calendar>, sqlx::Error> {
    sqlx::query_as(
        "SELECT c.id, c.name, c.color, c.slug,
                COALESCE(c.slug IN ('templates', 'installations', 'estimates'), 0) AS is_system,
                COALESCE(c.is_hidden, 0) <> 0 AS is_hidden
         FROM calendars c
         WHERE c.company_id = ? AND c.deleted_at IS NULL
         ORDER BY CASE c.slug WHEN 'templates' THEN 1 WHEN 'installations' THEN 2
                              WHEN 'estimates' THEN 3 ELSE 4 END, c.name, c.id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::distinct_colors;

    fn cal(id: i32, slug: Option<&str>, color: &str) -> (i32, Option<String>, String) {
        (id, slug.map(str::to_string), color.to_string())
    }

    #[test]
    fn templates_green_installations_red_and_no_two_alike() {
        let colors = distinct_colors(&[
            cal(1, Some("templates"), "blue"),
            cal(2, Some("installations"), "blue"),
            cal(3, Some("estimates"), "blue"),
            cal(4, None, "teal"),
            cal(5, None, "blue"),
            cal(6, None, "green"),
            cal(7, None, "#123456"),
        ]);
        let names: Vec<&str> = colors.iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(
            names,
            ["green", "red", "blue", "teal", "purple", "orange", "yellow"]
        );
    }

    #[test]
    fn a_calendar_keeps_its_own_free_color() {
        let colors = distinct_colors(&[cal(4, None, "pink"), cal(5, None, "cyan")]);
        assert_eq!(colors, [(4, "pink".to_string()), (5, "cyan".to_string())]);
    }
}
