# duplicate-refresh

Copies the production database (`main`) over the dev database (`duplicate`)
every night at **2:00 AM America/New_York**. Both are databases on the one RDS
instance (`database-1-copy`), so the copy never leaves the server.

## How a run works

1. Signs in as the MySQL user `duplicate_refresh` and checks its grants
   (`SHOW GRANTS`). It refuses to run unless the user can only read `main`
   (`SELECT`, `SHOW VIEW`) and has no global privilege, role or grant option.
   With `admin` it refuses.
2. Takes the named lock `duplicate-refresh` (a second run gives up).
3. Builds `duplicate_next` from scratch: for each table of `main`, its
   `SHOW CREATE TABLE` and `INSERT INTO duplicate_next.t (...) SELECT ... FROM main.t`
   (generated columns are left out; they are computed).
4. Re-encrypts the two-step sign-in secrets in `duplicate_next.user_mfa`
   from production's key to dev's (both read from the API secrets
   `granite/granite-api` and `granite/granite-api-dev`), so an authenticator
   set up on production gives working codes on dev too. Dev never gets
   production's `AES_KEY`, which also opens the copied Gmail, QuickBooks and
   RingCentral tokens: those stay unusable on dev.
5. Swaps in one atomic `RENAME TABLE`: `duplicate`'s tables move to
   `duplicate_old`, `duplicate_next`'s to `duplicate`. Then drops
   `duplicate_old` and `duplicate_next`. The `duplicate` database itself is
   never dropped, so the grants on it stay.

## Why it cannot harm `main`

- The user has only `SELECT` and `SHOW VIEW` on `main`: MySQL rejects any
  write to it, whatever the code does, and the run checks that first.
- The session runs at READ COMMITTED, so each `INSERT ... SELECT` reads `main`
  as a consistent, non-locking snapshot: no row locks, writers never wait.
  (At REPEATABLE READ it would take shared locks on every row it reads.)
- `lock_wait_timeout` is 60 s: if a migration holds a `main` table, the run
  fails rather than queue behind it.
- At 2 AM the copy (~1 GB, measured at ~18 MB/s on the instance) takes about
  a minute or two.

If a run fails before the swap, `duplicate` keeps its old tables and the next
night starts over. Each table is its own snapshot, seconds apart, so a row
written mid-run may be in one table and not another (foreign keys are not
checked while copying). Routines, triggers and events are not copied (`main`
has none); a view in `main` stops the run.

## AWS (personal account, us-east-2)

| What | Name |
| --- | --- |
| Lambda | `duplicate-refresh` (x86_64, 128 MB, timeout 900 s, reserved concurrency 1, async retries 0) |
| Secret | `granite/duplicate-refresh` (`REFRESH_DATABASE_URL`), loaded through `SECRETS_ID` |
| Schedule | EventBridge Scheduler `duplicate-refresh`, `cron(0 2 * * ? *)`, America/New_York, no retries |
| Scheduler role | `duplicate-refresh-scheduler` (invoke this Lambda only) |
| MySQL user | `duplicate_refresh`, `REQUIRE SSL` |

The MySQL user's grants:

```sql
GRANT SELECT, SHOW VIEW ON `main`.* TO 'duplicate_refresh'@'%';
GRANT ALL PRIVILEGES ON `duplicate`.* TO 'duplicate_refresh'@'%';
GRANT ALL PRIVILEGES ON `duplicate\_next`.* TO 'duplicate_refresh'@'%';
GRANT ALL PRIVILEGES ON `duplicate\_old`.* TO 'duplicate_refresh'@'%';
```

## Running it

```sh
make deploy duplicate-refresh prod          # code only; settings are kept
# Checks only (grants, tables, staging database), copies nothing:
aws --profile personal --region us-east-2 lambda invoke --function-name duplicate-refresh \
  --cli-binary-format raw-in-base64-out --payload '{"check_only":true}' /dev/stdout
```

A run started by hand with `{}` copies right away; prefer the night. Logs are
in CloudWatch, `/aws/lambda/duplicate-refresh` (one line per table).
