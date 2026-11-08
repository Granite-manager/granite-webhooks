"""Archive Linear issues that have been closed for longer than a set number of days.

Linear's own team setting can only auto-archive after 1, 3, 6, 9 or 12 months;
the free plan caps a workspace at 250 non-archived issues, so this archives
every issue in the workspace that has sat in a Done, Canceled or Duplicate
state for more than ARCHIVE_AFTER_DAYS (default 7) days.

Usage: LINEAR_API_KEY=... python3 scripts/linear_auto_archive.py [--dry-run]
Run daily by .github/workflows/linear-auto-archive.yml.
"""

import datetime
import json
import os
import sys
import urllib.request

API = "https://api.linear.app/graphql"
CLOSED_TYPES = ["completed", "canceled", "duplicate"]

QUERY = """
query($after: String, $filter: IssueFilter!) {
  issues(first: 100, after: $after, filter: $filter) {
    nodes { id identifier title completedAt canceledAt }
    pageInfo { hasNextPage endCursor }
  }
}
"""

ARCHIVE = """
mutation($id: String!) { issueArchive(id: $id) { success } }
"""


def gql(key, query, variables):
    req = urllib.request.Request(
        API,
        data=json.dumps({"query": query, "variables": variables}).encode(),
        headers={"Authorization": key, "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        body = json.load(resp)
    if body.get("errors"):
        raise RuntimeError(f"Linear API error: {body['errors']}")
    return body["data"]


def main():
    key = os.environ["LINEAR_API_KEY"]
    dry_run = "--dry-run" in sys.argv
    days = int(os.environ.get("ARCHIVE_AFTER_DAYS") or 7)
    cutoff = (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=days)).isoformat()
    issue_filter = {
        "state": {"type": {"in": CLOSED_TYPES}},
        "or": [{"completedAt": {"lt": cutoff}}, {"canceledAt": {"lt": cutoff}}],
    }

    issues, after = [], None
    while True:
        page = gql(key, QUERY, {"after": after, "filter": issue_filter})["issues"]
        issues += page["nodes"]
        if not page["pageInfo"]["hasNextPage"]:
            break
        after = page["pageInfo"]["endCursor"]

    print(f"{len(issues)} issue(s) closed before {cutoff}")
    for issue in issues:
        print(f"{'would archive' if dry_run else 'archiving'} {issue['identifier']}: {issue['title'][:80]}")
        if not dry_run and not gql(key, ARCHIVE, {"id": issue["id"]})["issueArchive"]["success"]:
            raise RuntimeError(f"Failed to archive {issue['identifier']}")


if __name__ == "__main__":
    main()
