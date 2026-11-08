#!/usr/bin/env bash
# Refuses a deploy whose migrations are not all applied to the target database.
#
#   scripts/check-migrations.sh prod|dev
#
# Every file in migrations/ must be in the database's _sqlx_migrations, applied
# successfully. A Lambda deployed before its migration answers 500 "Unknown
# column" until someone runs it (2026-10-23, 10-26, 10-29, 11-07: ~500 errors).
#
# The database: MIGRATION_CHECK_DATABASE_URL (mysql://user:pass@host:port,
# no database; GitHub Actions uses a read-only user), else the DATABASE_URL of
# the api's secret in AWS Secrets Manager (profile AWS_DEPLOY_PROFILE, default
# `personal`). prod is the `main` database, dev `duplicate`.
#
# The MySQL client: MYSQL_CLIENT if set, else `mysql` on the PATH, else the
# local container's (`docker exec -i granite-local-mysql mysql`).
set -euo pipefail

env_name="${1:-}"
case "$env_name" in
  prod) database=main; secret=granite/granite-api ;;
  dev) database=duplicate; secret=granite/granite-api-dev ;;
  *) echo "usage: $0 prod|dev" >&2; exit 2 ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"

url="${MIGRATION_CHECK_DATABASE_URL:-}"
if [ -z "$url" ] && [ -n "${GITHUB_ACTIONS:-}" ]; then
  echo "::error::Set the MIGRATION_CHECK_DATABASE_URL repository secret (read-only migration_check user)." >&2
  exit 1
fi
if [ -z "$url" ]; then
  url=$(aws secretsmanager get-secret-value --profile "${AWS_DEPLOY_PROFILE:-personal}" \
    --region us-east-2 --secret-id "$secret" --query SecretString --output text \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["DATABASE_URL"])')
fi

# user, password, host, port from the URL (any database or query in it is ignored).
parts=$(python3 -c '
import sys, urllib.parse as u
x = u.urlparse(sys.argv[1])
print(u.unquote(x.username or "")); print(u.unquote(x.password or "")); print(x.hostname or ""); print(x.port or 3306)
' "$url")
db_user=$(sed -n 1p <<<"$parts")
db_password=$(sed -n 2p <<<"$parts")
db_host=$(sed -n 3p <<<"$parts")
db_port=$(sed -n 4p <<<"$parts")

if [ -n "${MYSQL_CLIENT:-}" ]; then
  read -r -a client <<<"$MYSQL_CLIENT"
elif command -v mysql >/dev/null 2>&1; then
  client=(mysql)
else
  client=(docker exec -i -e MYSQL_PWD granite-local-mysql mysql)
fi

applied=$(MYSQL_PWD="$db_password" "${client[@]}" --ssl-mode=REQUIRED --batch --skip-column-names \
  -h "$db_host" -P "$db_port" -u "$db_user" "$database" \
  -e "SELECT version FROM _sqlx_migrations WHERE success = 1" 2> >(grep -v 'Using a password' >&2))

missing=()
for file in "$root"/migrations/*.sql; do
  version=$(basename "$file" | cut -d_ -f1)
  if ! grep -qx "$version" <<<"$applied"; then
    missing+=("$(basename "$file")")
  fi
done

if [ "${#missing[@]}" -gt 0 ]; then
  echo "Refusing to deploy to $env_name: the $database database lacks these migrations:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  echo "Apply them first (sqlx migrate run --source migrations), then deploy." >&2
  exit 1
fi
echo "Migrations: all $(ls "$root"/migrations/*.sql | wc -l | tr -d ' ') applied to $database ($env_name)."
