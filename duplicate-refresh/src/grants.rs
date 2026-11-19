//! The run refuses to start unless the MySQL user it signed in as can only
//! read the source database: whatever the code does, the server itself then
//! rejects any write to `main`. Running it with the `admin` user (or any user
//! with a global privilege, a role, or more than `SELECT`/`SHOW VIEW` on the
//! source) fails here, before any statement touches a database.

/// Privileges a grant that reaches the source database may hold.
const SOURCE_PRIVILEGES: [&str; 2] = ["SELECT", "SHOW VIEW"];

/// Checks the lines of `SHOW GRANTS` (for the current user). `Ok` only when
/// every line is understood and none lets the user change `source`.
pub fn check(lines: &[String], source: &str) -> Result<(), String> {
    if lines.is_empty() {
        return Err("SHOW GRANTS returned nothing".to_string());
    }
    for line in lines {
        check_line(line, source).map_err(|why| format!("refusing to run: {why}: {line}"))?;
    }
    Ok(())
}

fn check_line(line: &str, source: &str) -> Result<(), &'static str> {
    let rest = line.strip_prefix("GRANT ").ok_or("not a GRANT")?;
    if line.contains(" WITH GRANT OPTION") {
        return Err("grant option");
    }
    // A role (`GRANT role TO user`) has no ` ON `; its privileges are unknown.
    let (privileges, rest) = rest.split_once(" ON ").ok_or("role or unknown grant")?;
    let (object, _) = rest.split_once(" TO ").ok_or("no TO")?;
    let privileges: Vec<&str> = privileges.split(',').map(str::trim).collect();
    if object == "*.*" {
        return if privileges == ["USAGE"] {
            Ok(())
        } else {
            Err("global privilege")
        };
    }
    let database = database_pattern(object).ok_or("unknown object")?;
    if like(&database, source) {
        if privileges.iter().all(|p| SOURCE_PRIVILEGES.contains(p)) {
            Ok(())
        } else {
            Err("more than read access to the source database")
        }
    } else {
        Ok(())
    }
}

/// The database of `` `db`.* `` or `` `db`.`table` ``, unquoted.
fn database_pattern(object: &str) -> Option<String> {
    let mut chars = object.strip_prefix('`')?.chars().peekable();
    let mut name = String::new();
    while let Some(c) = chars.next() {
        if c == '`' {
            if chars.peek() == Some(&'`') {
                chars.next();
                name.push('`');
            } else {
                let rest: String = chars.collect();
                return rest.starts_with('.').then_some(name);
            }
        } else {
            name.push(c);
        }
    }
    None
}

/// Whether a grant's database pattern (`%` and `_` are wildcards, `\`
/// escapes) matches `name`. A grant on `` `%`.* `` reaches every database.
fn like(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('%', rest)) => (0..=n.len()).any(|skip| go(rest, &n[skip..])),
            Some(('_', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some(('\\', rest)) if !rest.is_empty() => {
                n.first() == Some(&rest[0]) && go(&rest[1..], &n[1..])
            }
            Some((c, rest)) => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(ToString::to_string).collect()
    }

    const OK: [&str; 5] = [
        "GRANT USAGE ON *.* TO `duplicate_refresh`@`%`",
        "GRANT SELECT, SHOW VIEW ON `main`.* TO `duplicate_refresh`@`%`",
        "GRANT ALL PRIVILEGES ON `duplicate`.* TO `duplicate_refresh`@`%`",
        "GRANT ALL PRIVILEGES ON `duplicate\\_next`.* TO `duplicate_refresh`@`%`",
        "GRANT ALL PRIVILEGES ON `duplicate_old`.* TO `duplicate_refresh`@`%`",
    ];

    #[test]
    fn the_refresh_user_passes() {
        assert_eq!(check(&lines(&OK), "main"), Ok(()));
    }

    #[test]
    fn write_access_to_the_source_is_refused() {
        for bad in [
            "GRANT SELECT, INSERT ON `main`.* TO `u`@`%`",
            "GRANT ALL PRIVILEGES ON `main`.* TO `u`@`%`",
            "GRANT SELECT, DELETE ON `main`.`deals` TO `u`@`%`",
            "GRANT DROP ON `ma%`.* TO `u`@`%`",
            "GRANT ALL PRIVILEGES ON `%`.* TO `u`@`%`",
            "GRANT ALTER ON `m_in`.* TO `u`@`%`",
        ] {
            let mut all = lines(&OK);
            all.push(bad.to_string());
            assert!(check(&all, "main").is_err(), "{bad}");
        }
    }

    #[test]
    fn admin_like_users_are_refused() {
        for bad in [
            "GRANT SELECT, INSERT, UPDATE, DELETE, CREATE, DROP ON *.* TO `admin`@`%` WITH GRANT OPTION",
            "GRANT SELECT ON *.* TO `u`@`%`",
            "GRANT BACKUP_ADMIN,SYSTEM_VARIABLES_ADMIN ON *.* TO `u`@`%`",
            "GRANT `rds_superuser_role`@`%` TO `admin`@`%`",
            "GRANT SELECT ON `main`.* TO `u`@`%` WITH GRANT OPTION",
            "GRANT PROXY ON ``@`` TO `u`@`%`",
        ] {
            let mut all = lines(&OK);
            all.push(bad.to_string());
            assert!(check(&all, "main").is_err(), "{bad}");
        }
        assert!(check(&[], "main").is_err());
    }

    #[test]
    fn like_follows_mysql_wildcards() {
        assert!(like("main", "main"));
        assert!(like("m%", "main"));
        assert!(like("m_in", "main"));
        assert!(!like("m\\_in", "main"));
        assert!(like("duplicate\\_next", "duplicate_next"));
        assert!(!like("duplicate", "main"));
        assert!(!like("mai", "main"));
    }

    #[test]
    fn database_pattern_unquotes() {
        assert_eq!(database_pattern("`main`.*").as_deref(), Some("main"));
        assert_eq!(database_pattern("`a``b`.`t`").as_deref(), Some("a`b"));
        assert_eq!(database_pattern("``@``"), None);
    }
}
