use sqlx::MySqlPool;

#[derive(Debug)]
pub struct SalesUser {
    pub id: i32,
    pub telegram_id: Option<i64>,
    pub name: Option<String>,
    pub position_id: i32,
    pub mtd_lead_count: i64,
    pub user_position_id: i32,
}

pub struct UserTgInfo {
    pub telegram_id: Option<i64>,
    pub name: Option<String>,
    pub email: String,
}

pub struct UserNotificationsTgInfo {
    pub notifications_telegram_id: Option<i64>,
    pub telegram_sms_notifications: bool,
    pub telegram_email_notifications: bool,
    pub telegram_activity_notifications: bool,
}

pub async fn get_sales_users(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<SalesUser>, sqlx::Error> {
    sqlx::query_as!(
        SalesUser,
        r#"
        SELECT
            u.id,
            u.telegram_id,
            u.name,
            up.position_id,
            up.id as user_position_id,
            COUNT(c.id) as mtd_lead_count
        FROM users u
        INNER JOIN users_positions up ON u.id = up.user_id
        LEFT JOIN customers c ON u.id = c.sales_rep
            AND c.source = 'leads'
            AND c.assigned_date >= DATE_FORMAT(NOW(), '%Y-%m-01')
            AND c.company_id = u.company_id
            AND c.deleted_at IS NULL
        WHERE u.company_id = ?
        AND u.is_deleted = 0
        AND (up.position_id = 1 OR up.position_id = 2)
        GROUP BY u.id, u.telegram_id, u.name, up.position_id, user_position_id
        "#,
        company_id
    )
    .fetch_all(pool)
    .await
}

/// Wrong link codes a user may send within one hour (counted from the first
/// wrong one) before linking is refused for the rest of that hour.
pub const TELEGRAM_CODE_MAX_FAILURES: u8 = 5;

/// What [`set_user_telegram_token`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum IssueCode {
    /// The code is stored for the chat and lives for 10 minutes.
    Issued,
    /// Too many wrong codes this hour: no code was stored.
    Locked,
}

/// What [`link_telegram_with_code`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum CodeCheck {
    /// The chat asked for no code (or it was used up).
    NoCode,
    /// Too many wrong codes this hour.
    Locked,
    /// The code is older than 10 minutes.
    Expired,
    /// Not the code that was sent.
    Wrong,
    /// The code matched: the chat is now the user's Telegram.
    Linked,
}

/// Link the chat to the user that was sent `code` for it, if the code is the
/// one sent, still live, and the user is not locked out by wrong codes.
pub async fn link_telegram_with_code(
    pool: &MySqlPool,
    telegram_id: i64,
    code: i32,
) -> Result<CodeCheck, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let pending = sqlx::query!(
        r#"
        SELECT telegram_conf_code,
               COALESCE(telegram_conf_expires_at > NOW(), FALSE) AS "live: bool",
               COALESCE(telegram_conf_failures >= ?
                        AND telegram_conf_failed_at > NOW() - INTERVAL 1 HOUR, FALSE) AS "locked: bool"
        FROM users
        WHERE temp_telegram_id = ?
        ORDER BY id
        LIMIT 1
        FOR UPDATE
        "#,
        TELEGRAM_CODE_MAX_FAILURES,
        telegram_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(pending) = pending else {
        return Ok(CodeCheck::NoCode);
    };
    let Some(sent) = pending.telegram_conf_code else {
        return Ok(CodeCheck::NoCode);
    };
    if pending.locked {
        return Ok(CodeCheck::Locked);
    }
    if !pending.live {
        return Ok(CodeCheck::Expired);
    }
    if sent != code {
        // MySQL assigns left to right: `failures` reads the old `failed_at`.
        sqlx::query!(
            r#"
            UPDATE users SET
              telegram_conf_failures = IF(telegram_conf_failed_at IS NULL
                                          OR telegram_conf_failed_at <= NOW() - INTERVAL 1 HOUR,
                                          1, LEAST(telegram_conf_failures + 1, 255)),
              telegram_conf_failed_at = IF(telegram_conf_failed_at IS NULL
                                           OR telegram_conf_failed_at <= NOW() - INTERVAL 1 HOUR,
                                           NOW(), telegram_conf_failed_at)
            WHERE temp_telegram_id = ?
            "#,
            telegram_id,
        )
        .execute(&mut *tx)
        .await?;
        let used_up = sqlx::query!(
            r#"
            UPDATE users SET telegram_conf_code = NULL, telegram_conf_expires_at = NULL
            WHERE temp_telegram_id = ? AND telegram_conf_failures >= ?
            "#,
            telegram_id,
            TELEGRAM_CODE_MAX_FAILURES,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        return Ok(if used_up {
            CodeCheck::Locked
        } else {
            CodeCheck::Wrong
        });
    }
    sqlx::query!(
        r#"
        UPDATE users SET
          telegram_id = ?,
          temp_telegram_id = NULL,
          telegram_conf_code = NULL,
          telegram_conf_expires_at = NULL,
          telegram_conf_failures = 0,
          telegram_conf_failed_at = NULL
        WHERE temp_telegram_id = ? AND telegram_conf_code = ?
        "#,
        telegram_id,
        telegram_id,
        code,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CodeCheck::Linked)
}

pub async fn user_has_telegram_id(pool: &MySqlPool, telegram_id: i64) -> Result<bool, sqlx::Error> {
    let user = sqlx::query_scalar!(
        r#"SELECT id FROM users WHERE telegram_id = ? "#,
        telegram_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(user.is_some())
}

/// Store a 10-minute link code for `email`'s user and this chat.
///
/// The code is sent to that email. A chat has one pending code at a time:
/// asking for another email's code drops the chat's earlier one. Refused while the user is
/// locked out by wrong codes; a new code does not reset that count.
pub async fn set_user_telegram_token(
    pool: &MySqlPool,
    telegram_id: i64,
    token: i32,
    email: &str,
) -> Result<IssueCode, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) FROM users
        WHERE email = ?
          AND telegram_conf_failures >= ?
          AND telegram_conf_failed_at > NOW() - INTERVAL 1 HOUR
        "#,
        email,
        TELEGRAM_CODE_MAX_FAILURES,
    )
    .fetch_one(&mut *tx)
    .await?
        > 0;
    if locked {
        return Ok(IssueCode::Locked);
    }
    sqlx::query!(
        r#"
        UPDATE users
        SET temp_telegram_id = NULL, telegram_conf_code = NULL, telegram_conf_expires_at = NULL
        WHERE temp_telegram_id = ? AND email <> ?
        "#,
        telegram_id,
        email,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"
        UPDATE users
        SET telegram_conf_code = ?,
            temp_telegram_id = ?,
            telegram_conf_expires_at = NOW() + INTERVAL 10 MINUTE
        WHERE email = ?
        "#,
        token,
        telegram_id,
        email
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(IssueCode::Issued)
}

pub async fn get_user_tg_info(
    pool: &MySqlPool,
    user_id: i32,
) -> Result<Option<UserTgInfo>, sqlx::Error> {
    sqlx::query_as!(
        UserTgInfo,
        r#"SELECT telegram_id, email, name FROM users WHERE id = ?"#,
        user_id
    )
    .fetch_optional(pool)
    .await
}

pub async fn get_user_notifications_tg_info(
    pool: &MySqlPool,
    user_id: i32,
) -> Result<Option<UserNotificationsTgInfo>, sqlx::Error> {
    sqlx::query_as!(
        UserNotificationsTgInfo,
        r#"
        SELECT
            notifications_telegram_id,
            telegram_sms_notifications as "telegram_sms_notifications!: bool",
            telegram_email_notifications as "telegram_email_notifications!: bool",
            telegram_activity_notifications as "telegram_activity_notifications!: bool"
        FROM users
        WHERE id = ?
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await
}

pub async fn get_company_id_by_cloudtalk_agent(
    pool: &MySqlPool,
    agent_id: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar::<_, i32>(
        r#"
        SELECT company_id
        FROM users
        WHERE is_deleted = 0
          AND company_id IS NOT NULL
          AND cloudtalk_agent_id = ?
        LIMIT 1
        "#,
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await
}

pub async fn get_company_id_by_cloudtalk_phone(
    pool: &MySqlPool,
    phone_last10: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar::<_, i32>(
        r#"
        SELECT company_id
        FROM users
        WHERE is_deleted = 0
          AND company_id IS NOT NULL
          AND RIGHT(REGEXP_REPLACE(COALESCE(cloudtalk_phone_number, ''), '[^0-9]', ''), 10) = ?
        LIMIT 1
        "#,
    )
    .bind(phone_last10)
    .fetch_optional(pool)
    .await
}

pub async fn get_user_id_by_cloudtalk_agent(
    pool: &MySqlPool,
    company_id: i32,
    agent_id: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT id
        FROM users
        WHERE company_id = ?
          AND cloudtalk_agent_id = ?
          AND is_deleted = 0
        LIMIT 1
        "#,
        company_id,
        agent_id
    )
    .fetch_optional(pool)
    .await
}

pub async fn get_user_id_by_ringcentral_agent(
    pool: &MySqlPool,
    company_id: i32,
    extension_id: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar::<_, i32>(
        r#"
        SELECT id
        FROM users
        WHERE company_id = ?
          AND ringcentral_extension_id = ?
          AND is_deleted = 0
        LIMIT 1
        "#,
    )
    .bind(company_id)
    .bind(extension_id)
    .fetch_optional(pool)
    .await
}

pub async fn email_exists(pool: &MySqlPool, email: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM users
            WHERE email = ?
        ) AS "email_exists!: bool"
        "#,
        email
    )
    .fetch_one(pool)
    .await
}

pub async fn get_id_by_email(pool: &MySqlPool, email: &str) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(r#"SELECT id FROM users WHERE email = ?"#, email)
        .fetch_optional(pool)
        .await
}

/// Company that owns a user. Inbound mail derives `emails.company_id` from the
/// resolved receiver, which is the only party we can attribute with certainty.
pub async fn get_company_id_by_user_id(
    pool: &MySqlPool,
    user_id: i32,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(r#"SELECT company_id FROM users WHERE id = ?"#, user_id)
        .fetch_optional(pool)
        .await
}

/// Case-insensitive user lookup used when resolving recipient addresses.
/// `get_id_by_email` compares exactly; header addresses arrive in any case.
pub async fn get_id_by_email_normalized(
    pool: &MySqlPool,
    email: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT id FROM users WHERE LOWER(TRIM(email)) = ? AND is_deleted = 0 LIMIT 1"#,
        email
    )
    .fetch_optional(pool)
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceivingEmail {
    To(i32),
    Forward(i32),
}

impl ReceivingEmail {
    pub const fn inner(self) -> i32 {
        match self {
            Self::To(value) | Self::Forward(value) => value,
        }
    }
}

pub async fn get_id_by_email_with_forward(
    pool: &MySqlPool,
    email: &str,
    forward: Option<&str>,
) -> Result<Option<ReceivingEmail>, sqlx::Error> {
    if let Some(user_id) = get_id_by_email(pool, email).await? {
        return Ok(Some(ReceivingEmail::To(user_id)));
    }
    let Some(inner_forward) = forward else {
        return Ok(None);
    };
    if let Some(user_id) = get_id_by_email(pool, inner_forward).await? {
        return Ok(Some(ReceivingEmail::Forward(user_id)));
    }
    Ok(None)
}
