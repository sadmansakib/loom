//! Mock GitHub MCP server for the D3 deferral probe (docs/daily-driver.md).
//!
//! Standalone, std-only, no dependencies: compile with
//! `rustc -O mcp-probe-ghmock.rs -o ghmock` and point `.loom/mcp.toml` at
//! the binary. Speaks MCP stdio (newline-delimited JSON-RPC 2.0).
//!
//! Serves 35 GitHub-like tools with realistically sized schemas (~3 KB
//! each, matching the measured ~26K-token upfront cost of the real GitHub
//! MCP server). `get_issue` is functional: it returns a seeded issue that
//! fully describes the fixture bug (the daily-driver `fix` variant,
//! median on even-length slices). Every other tool answers "noop".
//!
//! If the env `LOOM_GHMOCK_DUMP` is set at startup, the exact tool
//! definition list is written to that path as JSON before serving, so the
//! probe can compute upfront token costs from the same bytes the client
//! sees.

use std::env;
use std::io::{self, BufRead, Write};

const OWNER_DESC: &str = "The account or organization that owns the repository. This is the login name as it appears in the repository URL, for example 'octocat' in https://github.com/octocat/Hello-World. The value is case-insensitive on the GitHub API but is echoed back exactly as provided in the response payload, so prefer the canonical casing shown on the repository page when it is known.";
const REPO_DESC: &str = "The name of the repository without the owner prefix, for example 'Hello-World' rather than 'octocat/Hello-World'. Combined with the owner parameter this identifies the repository unambiguously. The name is case-insensitive on the GitHub API; responses return the canonical casing that was assigned when the repository was created.";
const PER_PAGE_DESC: &str = "The number of results per page, between 1 and 100 inclusive. The GitHub API caps page sizes at 100 and defaults to 30; larger pages reduce the number of round trips needed to enumerate large collections but increase individual response latency and payload size. Values outside the accepted range are clamped by the server rather than rejected.";
const PAGE_DESC: &str = "The 1-based index of the page of results to fetch. Page 1 contains the most recent items. Requesting a page beyond the last one returns an empty array rather than an error, so callers that need completeness should iterate until an empty page is observed rather than relying on a total count.";

const ISSUE_TEXT: &str = "Issue #1 [bug]: median() returns the upper-middle element for even-length slices. Expected behavior: median(&[1, 2, 3, 4]) == 2, the average of the two middle values (integer division), while odd-length slices keep the single middle element. Actual behavior: the function returns 3 for that input. The defect is in proberepo src/lib.rs, function median. Fix the function so the existing test suite passes.";

fn s(name: &str, desc: &str) -> String {
    format!("\"{name}\":{{\"type\":\"string\",\"description\":\"{desc}\"}}")
}

fn i(name: &str, desc: &str) -> String {
    format!("\"{name}\":{{\"type\":\"integer\",\"description\":\"{desc}\"}}")
}

fn b(name: &str, desc: &str) -> String {
    format!("\"{name}\":{{\"type\":\"boolean\",\"description\":\"{desc}\"}}")
}

fn tool(name: &str, desc: &str, params: String, required: String) -> String {
    format!(
        "{{\"name\":\"{name}\",\"description\":\"{desc}\",\"inputSchema\":{{\"type\":\"object\",\"properties\":{{{params}}},\"required\":[{required}]}}}}"
    )
}

fn owner_repo() -> String {
    format!("{},{},{}", s("owner", OWNER_DESC), s("repo", REPO_DESC), common_extra())
}

fn common_extra() -> String {
    format!("{},{}",
        s(
            "accept",
            "The media type to request for the response. The GitHub API v3 REST surface nominates 'application/vnd.github+json' as the canonical media type; vendor-specific variants such as 'application/vnd.github.v3+json' are accepted for compatibility and behave identically. Requesting a raw or HTML media type where the endpoint supports it changes the shape of the returned payload entirely, so callers that parse JSON should not vary this value."
        ),
        s(
            "api_version",
            "The GitHub API version to pin for this request, as '2022-11-28'. Pinning the version opts the request into a stable contract: breaking changes to the endpoint's response shape are then announced and staged through a deprecation timeline rather than arriving unannounced. Omitting the version lets the request float on the server's current default version, which is acceptable for exploratory tooling but risky for anything that parses responses structurally."
        )
    )
}

fn tools() -> Vec<String> {
    let mut t = Vec::new();
    let or = owner_repo();

    t.push(tool(
        "get_issue",
        "Gets a single issue from a GitHub repository by its number, including title, body, author, labels, state, and timestamps.",
        format!(
            "{},{}",
            or,
            i("issue_number", "The number of the issue to retrieve. Issue numbers are unique within a repository and are assigned sequentially at creation time; they are not the same as the globally unique issue id. The number may point at a pull request, in which case the API returns the pull request's implicit issue object with a pull_request field present.")
        ),
        "\"owner\",\"repo\",\"issue_number\"".to_string(),
    ));

    t.push(tool(
        "list_issues",
        "Lists issues in a repository, most recently created first, with filtering by state, assignee, label, and milestone.",
        format!(
            "{},{},{},{}",
            or,
            s("state", "Filters issues by state: 'open', 'closed', or 'all'. The default is 'open'. Note that the GitHub API considers pull requests to be issues for the purposes of this listing unless they are excluded; state filtering applies to the issue's current lifecycle position rather than to any merge status of an associated pull request."),
            s("labels", "A comma-separated list of label names. Only issues carrying every listed label are returned. Labels are matched case-insensitively. When this parameter is omitted, no label filtering is applied and issues with zero labels are included alongside labeled ones."),
            s("assignee", "Filters issues by assignee login. Use 'none' for issues with no assignee, or '*' for issues with any assignee. A specific login restricts the listing to issues currently assigned to that user. Assignment history is not reflected; only the current assignee set is consulted."),
        ),
        "\"owner\",\"repo\"".to_string(),
    ));

    t.push(tool(
        "create_issue",
        "Creates a new issue in a repository with a title, body, and optional label and assignee lists.",
        format!(
            "{},{},{},{}",
            or,
            s("title", "The title of the new issue, rendered as a heading on the issue page and used in notification emails and mentions. Titles are plain text; markdown formatting is rendered on the site but is not interpreted by the API. The maximum length is 256 characters and longer titles are rejected with a validation error."),
            s("body", "The body of the new issue in GitHub-flavored markdown. The body supports the full markdown surface including task lists, tables, code fences with syntax highlighting, and user and issue mentions. There is no hard length limit, but very large bodies may be truncated by downstream consumers."),
            s("labels", "A JSON array of label names to apply to the new issue at creation time. Labels that do not already exist in the repository are created automatically if the authenticated user has push access; otherwise the request fails and the issue is not created.")
        ),
        "\"owner\",\"repo\",\"title\"".to_string(),
    ));

    t.push(tool(
        "update_issue",
        "Updates an existing issue: title, body, state, labels, and assignees can all be replaced in one call.",
        format!(
            "{},{},{},{}",
            or,
            i("issue_number", "The number of the issue to update. The issue must exist in the named repository and must not be a locked issue unless the actor has maintain permissions; locked issues reject updates from non-maintainers with a 403 response rather than a 404."),
            s("state", "The new state for the issue: 'open' or 'closed'. Omitting the field leaves the state unchanged. Closing an issue through this parameter does not attach a closing reference to any commit or pull request; issues closed this way show as closed by the actor alone."),
            s("title", "The replacement title for the issue. Passing an empty string clears the title only if the API version in use permits it; the recommended pattern for leaving the title untouched is to omit the field from the request entirely.")
        ),
        "\"owner\",\"repo\",\"issue_number\"".to_string(),
    ));

    t.push(tool(
        "add_issue_comment",
        "Adds a markdown comment to an existing issue or pull request conversation thread.",
        format!(
            "{},{},{}",
            or,
            i("issue_number", "The number of the issue or pull request to comment on. Pull request conversations are issue threads for numbering purposes, so the pull request number is used here directly."),
            s("body", "The comment body in GitHub-flavored markdown. Mentions inside the body trigger notifications for the mentioned users subject to their notification settings and the repository's participation rules. Reactions are added to comments separately after creation.")
        ),
        "\"owner\",\"repo\",\"issue_number\",\"body\"".to_string(),
    ));

    t.push(tool(
        "list_pull_requests",
        "Lists pull requests in a repository with optional filtering by head and base branch and by open or closed state.",
        format!(
            "{},{},{},{}",
            or,
            s("state", "Filters pull requests by state: 'open', 'closed', or 'all'. The default is 'open'. A merged pull request is in the 'closed' state with an associated merge commit; the merged_at timestamp distinguishes merged from merely closed pull requests when 'all' is selected."),
            s("head", "Filters by head branch in 'org:branch' or 'user:branch' form. The filter matches pull requests whose head ref is exactly the named branch of the named account. Branch name matching is exact after the colon; no globbing or prefix matching is performed."),
            s("base", "Filters by base branch name. Only pull requests requesting merges into exactly this branch are returned. This is commonly used to see everything queued for a release or mainline branch at once.")
        ),
        "\"owner\",\"repo\"".to_string(),
    ));

    t.push(tool(
        "get_pull_request",
        "Gets a single pull request by number, including diffs, review state, mergeability, and commit SHAs.",
        format!(
            "{},{}",
            or,
            i("pull_number", "The number of the pull request to retrieve. Pull request numbers share the issue number space within a repository. The returned object includes head and base refs, the diff introduced by the change, the review decision summary, and the mergeability state as last computed by GitHub.")
        ),
        "\"owner\",\"repo\",\"pull_number\"".to_string(),
    ));

    t.push(tool(
        "create_pull_request",
        "Creates a pull request from one branch to another in the same repository or across forks.",
        format!(
            "{},{},{},{},{}",
            or,
            s("title", "The title of the pull request, shown in listings and notification emails. Maximum 256 characters. The title can be left empty only when an issue reference is supplied in the body, in which case the issue title is adopted; this behavior is deprecated and should not be relied upon."),
            s("body", "The description of the change in GitHub-flavored markdown. A good body explains the motivation for the change, summarizes the approach, and calls out testing performed. Template contents from .github/PULL_REQUEST_TEMPLATE.md are not auto-filled by the API; clients that want templates must fetch and pre-populate them."),
            s("head", "The name of the branch containing the changes to be merged, in 'branch' form for same-repository pulls or 'user:branch' form across forks. The branch must already exist and must differ from base, or the request is rejected with a 422 status."),
            s("base", "The name of the branch the changes should be merged into. The branch must exist in the repository. Pull requests cannot target empty repositories or branches with no common ancestor to head.")
        ),
        "\"owner\",\"repo\",\"title\",\"head\",\"base\"".to_string(),
    ));

    t.push(tool(
        "merge_pull_request",
        "Merges a pull request using merge, squash, or rebase strategy, subject to branch protection rules.",
        format!(
            "{},{},{},{}",
            or,
            i("pull_number", "The number of the pull request to merge. The pull request must be mergeable, must not be in a conflicted state, and must satisfy all required status checks and reviews enforced by branch protection before this call can succeed."),
            s("merge_method", "The merge strategy: 'merge' creates a merge commit, 'squash' condenses the commits into one, and 'rebase' replays the commits onto the base without a merge commit. The available strategies are limited by repository configuration; attempting a disabled strategy returns a 405."),
            s("commit_title", "The title for the merge commit. When omitted, a default title incorporating the pull request number and title is generated. Only applies to the 'merge' and 'squash' strategies; rebase merges use the original commit messages verbatim.")
        ),
        "\"owner\",\"repo\",\"pull_number\"".to_string(),
    ));

    t.push(tool(
        "close_pull_request",
        "Closes a pull request without merging, leaving the head branch untouched.",
        format!(
            "{},{}",
            or,
            i("pull_number", "The number of the pull request to close. Closing is reversible by later reopening unless branch protection or repository settings forbid it. The head branch is not deleted by this call even when the repository is configured to delete branches on merge, because no merge occurred.")
        ),
        "\"owner\",\"repo\",\"pull_number\"".to_string(),
    ));

    t.push(tool(
        "list_reviews",
        "Lists all reviews submitted on a pull request with state, body, and reviewer login.",
        format!(
            "{},{},{}",
            or,
            i("pull_number", "The number of the pull request whose reviews should be listed. Reviews appear in submission order. Dismissed reviews remain in the listing with a DISMISSED state, which matters for callers auditing whether a required approval was later revoked."),
            s("state", "Filters reviews by final state: 'APPROVED', 'CHANGES_REQUESTED', 'COMMENTED', or 'DISMISSED'. Omit to list reviews in every state. The filter matches the review's current state after any dismissal, not the state it was submitted with.")
        ),
        "\"owner\",\"repo\",\"pull_number\"".to_string(),
    ));

    t.push(tool(
        "create_review",
        "Submits a review on a pull request: approve, request changes, or comment only.",
        format!(
            "{},{},{},{}",
            or,
            i("pull_number", "The number of the pull request to review. Reviews are submitted against the head SHA at the time of submission; the review is recorded against that commit even if the branch advances afterwards, and stale reviews can be dismissed automatically by new pushes per repository settings."),
            s("event", "The review verdict: 'APPROVE', 'REQUEST_CHANGES', or 'COMMENT'. A body is optional for approvals and comments but strongly recommended for change requests, since requested changes surface in the author's notification with whatever context is supplied here."),
            s("body", "The review summary body in markdown, shown above any line-level comments. Required when event is REQUEST_CHANGES by most repository conventions though not by the API itself; the API accepts a review with no body regardless of verdict.")
        ),
        "\"owner\",\"repo\",\"pull_number\",\"event\"".to_string(),
    ));

    t.push(tool(
        "list_commits",
        "Lists commits on a branch, newest first, each with author, committer, message, and verified signature status.",
        format!(
            "{},{},{},{}",
            or,
            s("sha", "The SHA or ref of the commit to start listing from. Results include this commit and its ancestors along the first-parent-following traversal of the graph. Omit to list from the branch or repository default head."),
            s("path", "Restricts the listing to commits that touched the given path. File renames are followed only when the server elects to; a rename plus rewrite may break the trail. Combine with page parameters to walk long histories of a single file."),
            i("per_page", PER_PAGE_DESC)
        ),
        "\"owner\",\"repo\"".to_string(),
    ));

    t.push(tool(
        "get_commit",
        "Gets a single commit by SHA, including the full diff against its first parent and per-file patch hunks.",
        format!(
            "{},{},{}",
            or,
            s("ref", "The SHA of the commit, or a ref name that resolves to one. Abbreviated SHAs are accepted if they are unambiguous within the repository; ambiguous prefixes return a 422 with a list of candidate objects rather than an arbitrary match. The response embeds the complete file-level diff for the commit."),
            i("page", PAGE_DESC)
        ),
        "\"owner\",\"repo\",\"ref\"".to_string(),
    ));

    t.push(tool(
        "list_branches",
        "Lists branches in a repository with their head commit SHAs and protection summaries.",
        format!(
            "{},{},{}",
            or,
            i("per_page", PER_PAGE_DESC),
            i("page", PAGE_DESC)
        ),
        "\"owner\",\"repo\"".to_string(),
    ));

    t.push(tool(
        "create_branch",
        "Creates a new branch pointing at an existing commit SHA.",
        format!(
            "{},{},{}",
            or,
            s("branch", "The name of the branch to create. Branch names may not contain spaces, tilde, caret, colon, question mark, asterisk, bracket, backslash, or a run of two consecutive dots, and may not begin or end with a slash, dot, or dash. Names that collide with existing refs are rejected."),
            s("sha", "The commit SHA the new branch should point at. The commit must be reachable in the repository; pointing a branch at an unrelated SHA is impossible by construction because refs are created through the git object store.")
        ),
        "\"owner\",\"repo\",\"branch\",\"sha\"".to_string(),
    ));

    t.push(tool(
        "delete_branch",
        "Deletes a branch reference. The underlying commits remain reachable through other refs or the API for a grace period.",
        format!(
            "{},{}",
            or,
            s("branch", "The name of the branch to delete. Deleting the default branch is refused with a 403 in most configurations. Commits unique to the deleted branch eventually become unreachable and are eligible for garbage collection unless referenced elsewhere.")
        ),
        "\"owner\",\"repo\",\"branch\"".to_string(),
    ));

    t.push(tool(
        "list_files",
        "Lists the file tree of a repository path, one entry per file or directory.",
        format!(
            "{},{},{},{}",
            or,
            s("path", "The directory path to list, relative to the repository root. An empty path lists the root. Paths are matched exactly; glob patterns are not supported here. Directory entries are returned with a trailing slash marker in some API versions and a type field in all of them."),
            s("ref", "The name of the commit, branch, or tag whose tree should be listed. Omitting the ref uses the repository's default branch. Listing a historical ref returns the tree as of that ref, which is how deleted files can still be enumerated."),
            i("per_page", PER_PAGE_DESC)
        ),
        "\"owner\",\"repo\"".to_string(),
    ));

    t.push(tool(
        "get_file_contents",
        "Gets the decoded contents of a single file from a repository at a given ref, with size and encoding metadata.",
        format!(
            "{},{},{},{}",
            or,
            s("path", "The path of the file within the repository, relative to the root, using forward slashes. Binary files are returned base64-encoded with the encoding field set accordingly; text files are returned as UTF-8 when they can be decoded, otherwise base64."),
            s("ref", "The commit, branch, or tag to read the file from. Omit for the default branch. Reading from a historical ref is the supported way to inspect a file as it existed before a change; there is no server-side diff between refs here, only point reads."),
            i("page", PAGE_DESC)
        ),
        "\"owner\",\"repo\",\"path\"".to_string(),
    ));

    t.push(tool(
        "create_or_update_file",
        "Creates or replaces a single file with a new blob, producing a commit on the target branch.",
        format!(
            "{},{},{},{},{},{}",
            or,
            s("path", "The path of the file to create or replace. Parent directories are created implicitly. Replacing requires the file's current blob SHA in the sha parameter; creating a file that already exists without that SHA fails with a 422, which is the mechanism that prevents blind overwrites."),
            s("content", "The full new file content, base64-encoded by the client before transmission. The server writes exactly these bytes; no newline normalization or encoding negotiation happens server-side. Empty content is valid and produces an empty file."),
            s("message", "The commit message for the change. The message appears in the repository history attributed to the authenticated user or the app installation. There is no client-side signing; commit verification follows the server's key handling."),
            s("branch", "The branch to commit to. Omit for the default branch. The branch must exist; this call does not create branches. Concurrent updates to the same path fail with a 409 when the fast-forward precondition does not hold."),
            s("sha", "Required when replacing an existing file: the blob SHA of the current content, as returned by get_file_contents. Omit when creating a new file. Supplying a stale SHA fails fast rather than overwriting intermediate changes.")
        ),
        "\"owner\",\"repo\",\"path\",\"content\",\"message\"".to_string(),
    ));

    t.push(tool(
        "push_files",
        "Pushes multiple file changes as a single commit, replacing each listed path wholesale.",
        format!(
            "{},{},{},{}",
            or,
            s("branch", "The branch to push the commit onto. The push is a fast-forward update; non-fast-forward pushes are rejected with a 422 rather than force-updating. Create the branch first when starting from a non-default point."),
            s("message", "The commit message covering all files in this push. A single commit is produced regardless of how many files are included; per-file messages require separate calls and separate commits."),
            s("files", "A JSON array of objects each with path and content fields, content base64-encoded. Files not listed are untouched. Listing the same path twice within one call is rejected; the server does not define an ordering for duplicate writes.")
        ),
        "\"owner\",\"repo\",\"branch\",\"message\",\"files\"".to_string(),
    ));

    t.push(tool(
        "delete_file",
        "Deletes a single file, producing a deletion commit on the target branch.",
        format!(
            "{},{},{},{},{}",
            or,
            s("path", "The path of the file to delete. The path must resolve to a file, not a directory; directory deletion is not a primitive and requires deleting each contained file or rewriting the tree."),
            s("message", "The commit message describing the deletion. Common convention records why the file was removed; the API accepts any non-empty string and rejects empty messages with a 422."),
            s("branch", "The branch to commit the deletion onto. Omit for the default branch. The deletion is subject to the same fast-forward rules as any other commit."),
            s("sha", "The blob SHA of the file being deleted, proving the caller saw the current content. A stale SHA aborts the deletion, protecting against removing a file that changed after it was last read.")
        ),
        "\"owner\",\"repo\",\"path\",\"message\",\"sha\"".to_string(),
    ));

    t.push(tool(
        "search_code",
        "Searches code across a repository or organization using GitHub's code search index.",
        format!(
            "{},{},{},{}",
            or,
            s("query", "The search text or qualified query. Code search matches on file content and path with limited qualifier support; regular expressions are not supported by the legacy endpoint. Results are ranked by the index, not by recency, and index lag can briefly hide just-pushed code."),
            s("order", "The sort order of results: 'desc' or 'asc'. Sorting is by best match by default; supplying an order without a sort key is accepted and applies to the default ranking, which is subtle and rarely what callers want."),
            i("per_page", PER_PAGE_DESC)
        ),
        "\"owner\",\"repo\",\"query\"".to_string(),
    ));

    t.push(tool(
        "search_issues",
        "Searches issues and pull requests across a repository with GitHub's issue search qualifiers.",
        format!(
            "{},{},{},{},{}",
            or,
            s("query", "The search text, optionally with qualifiers like 'is:pr', 'is:issue', 'state:closed', 'label:bug', 'author:login', or 'merged:>date' to narrow the corpus before text matching. Multiple qualifiers combine with AND semantics; the bare text matches titles and bodies."),
            s("sort", "The field to sort results by: 'comments', 'reactions', 'created', 'updated', or best match when omitted. Sorting interacts with the order parameter; 'created' with 'asc' yields the oldest issues first, which is useful for triage archaeology."),
            i("per_page", PER_PAGE_DESC),
            i("page", PAGE_DESC)
        ),
        "\"owner\",\"repo\",\"query\"".to_string(),
    ));

    t.push(tool(
        "search_repositories",
        "Searches repositories visible to the caller by name, description, topics, and metadata qualifiers.",
        format!(
            "{},{},{}",
            s("query", "The search text over repository names, descriptions, and topics, optionally with qualifiers such as 'language:rust', 'stars:>1000', 'fork:true', 'archived:false', 'pushed:>2026-01-01'. Qualifier and text components combine with AND semantics."),
            i("per_page", PER_PAGE_DESC),
            i("page", PAGE_DESC)
        ),
        "\"query\"".to_string(),
    ));

    t.push(tool(
        "get_repository",
        "Gets a repository's metadata: default branch, visibility, fork lineage, permissions, and language stats.",
        format!(
            "{},{}",
            or,
            s("org", "When supplied, fetches the repository under this organization login instead of the owner parameter. This exists for installations that address repos by organization exclusively; supplying both owner and org with different values is an error.")
        ),
        "\"owner\",\"repo\"".to_string(),
    ));

    t.push(tool(
        "create_repository",
        "Creates a new repository under the authenticated user or an organization, with visibility and gitignore/template options.",
        format!(
            "{},{},{},{},{}",
            s("name", "The name of the new repository. Names may contain letters, digits, hyphens, underscores, and periods, may not begin or end with a period, and are flattened to lowercase in the URL. Collisions within the account are rejected with a 422."),
            s("description", "A short description shown under the repository name on the site and in search results. Plain text only; markdown is not rendered here. The description can be updated later through repository settings or the update endpoint."),
            b("private", "Whether the repository is private. Private repositories consume seats on organizations with per-seat billing and have separate rate-limit pools for API traffic in some plans. Once created, visibility can be changed later, but fork and star history carry over, which is occasionally surprising."),
            s("org", "The organization login to create the repository under. Omit to create under the authenticated user. Creating under an organization requires the actor to have repository creation permission on it, which organization owners can restrict per role."),
            s("gitignore_template", "The name of a gitignore template to seed the initial commit with, such as 'Rust' or 'Node'. Only applies to empty repositories; the template list endpoint enumerates valid names. Omit for a completely empty repository with no initial commit.")
        ),
        "\"name\"".to_string(),
    ));

    t.push(tool(
        "list_workflows",
        "Lists GitHub Actions workflow files in a repository with their IDs, states, and paths.",
        format!(
            "{},{},{}",
            or,
            i("per_page", PER_PAGE_DESC),
            i("page", PAGE_DESC)
        ),
        "\"owner\",\"repo\"".to_string(),
    ));

    t.push(tool(
        "get_workflow_run",
        "Gets a single workflow run: status, conclusion, timing, triggering actor, and head commit.",
        format!(
            "{},{},{}",
            or,
            i("run_id", "The id of the workflow run. Run ids are unique across the repository and are not the same as run numbers, which are per-workflow sequences. Jobs and their steps are fetched through separate endpoints using this run id."),
            i("exclude_pull_requests", "When 1, omits pull request information from the response even when the run was triggered by a pull request, reducing payload size for callers that only need the conclusion and timing fields. Pass 0 or omit to include pull request context when present.")
        ),
        "\"owner\",\"repo\",\"run_id\"".to_string(),
    ));

    t.push(tool(
        "rerun_workflow",
        "Re-runs a workflow run, entirely or only its failed jobs, subject to retention windows.",
        format!(
            "{},{},{}",
            or,
            i("run_id", "The id of the workflow run to re-run. Runs older than the repository's retention period cannot be re-run because their logs and intermediate artifacts have been deleted; the request fails with a 403 in that case."),
            b("enable_debug_logging", "When true, the re-run executes with runner debug logging enabled, which surfaces step-level timing and runner diagnostics in the logs. Debug logging is noisy and slow; reserve it for diagnosing runner-side failures rather than ordinary build breaks.")
        ),
        "\"owner\",\"repo\",\"run_id\"".to_string(),
    ));

    t.push(tool(
        "list_deployments",
        "Lists deployments of a repository environment with statuses and creator.",
        format!(
            "{},{},{},{}",
            or,
            s("environment", "Filters deployments to a single named environment, such as 'production' or 'staging'. Environment names are case-insensitive and may contain hyphens and underscores. Omitting the filter lists deployments across all environments interleaved by creation time."),
            s("sha", "Filters deployments to those whose ref resolved to this commit SHA at deploy time. Redeploys of the same SHA appear as separate deployments, so the listing can contain multiple entries per SHA when rollbacks or retries occurred."),
            i("per_page", PER_PAGE_DESC)
        ),
        "\"owner\",\"repo\"".to_string(),
    ));

    t.push(tool(
        "create_deployment_status",
        "Reports a deployment status: success, failure, inactive, in_progress, or queued, with an optional log URL.",
        format!(
            "{},{},{},{},{}",
            or,
            i("deployment_id", "The id of the deployment to attach the status to. Statuses are append-only records under a deployment; the latest status for a deployment determines its rendered state, so out-of-order writes flip the perceived state."),
            s("state", "The new state: 'success', 'failure', 'inactive', 'in_progress', or 'queued'. Setting 'success' with an environment_url marks the deployment live and enables environment protection bookkeeping. 'error' is accepted by the REST API for compatibility and maps to 'failure'."),
            s("log_url", "A URL to the deployment's logs, shown in the interface as the drill-down target for the status. The URL is not fetched or validated by the server. Omit when there are no logs to point at; an empty string is not equivalent to omission in some API versions."),
            s("environment_url", "The publicly reachable URL of the deployed environment, set on success statuses. Setting it updates the environment's live URL and feeds the deployments view. Omit for statuses that do not represent a live endpoint.")
        ),
        "\"owner\",\"repo\",\"deployment_id\",\"state\"".to_string(),
    ));

    t.push(tool(
        "get_me",
        "Gets the authenticated user or installation behind the current credentials, with plan and scope information.",
        format!(
            "{},{}",
            s("username", "The login to fetch. Omit to resolve the authenticated principal itself. Resolving another user returns their public profile; the plan and email fields are only populated for the authenticated principal, and only to the extent the token's scopes permit."),
            common_extra()
        ),
        String::new(),
    ));

    t.push(tool(
        "list_notifications",
        "Lists the actor's notifications across repositories, filtered by unread state and reason.",
        format!(
            "{},{},{}",
            b("all", "When true, includes notifications already marked as read in the listing window. The default lists only unread notifications. Read notification inclusion is bounded by the same since/until windowing as unread ones and does not paginate into unbounded history."),
            s("since", "An ISO-8601 timestamp bounding the listing to notifications updated after this instant. The comparison is against the notification's last update time, not the subject's creation time, so thread activity refreshes older notifications into the window."),
            common_extra()
        ),
        String::new(),
    ));

    t.push(tool(
        "mark_notifications_read",
        "Marks all unread notifications up to a timestamp as read in one bulk operation.",
        format!(
            "{},{},{}",
            s("last_read_at", "An ISO-8601 timestamp; notifications updated at or before it are marked read. Omit to mark everything currently unread. The operation is asynchronous server-side; a small delay before the unread count reflects the change is expected and documented."),
            b("unread", "When true, the call instead marks matching notifications as unread, restoring them to the inbox. This is rarely used except to re-surface items for later attention; it cannot resurrect notifications deleted by the user."),
            common_extra()
        ),
        String::new(),
    ));

    t
}

fn find_field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = rest
            .find(|c: char| c == ',' || c == '}')
            .unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    }
}

fn respond(out: &mut impl Write, id: &str, result: &str) {
    let _ = writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}");
    let _ = out.flush();
}

fn main() {
    if let Ok(path) = env::var("LOOM_GHMOCK_DUMP") {
        let _ = std::fs::write(path, format!("[{}]", tools().join(",")));
        return;
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Some(method) = find_field(&line, "method") else {
            continue;
        };
        let id = find_field(&line, "id").unwrap_or_else(|| "0".to_string());
        match method.as_str() {
            "initialize" => respond(
                &mut out,
                &id,
                "{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{\"listChanged\":false}},\"serverInfo\":{\"name\":\"ghmock\",\"version\":\"0.1.0\"}}",
            ),
            "tools/list" => respond(
                &mut out,
                &id,
                &format!("{{\"tools\":[{}]}}", tools().join(",")),
            ),
            "tools/call" => {
                let name = find_field(&line, "name").unwrap_or_default();
                let text = if name == "get_issue" {
                    ISSUE_TEXT.to_string()
                } else {
                    "noop".to_string()
                };
                respond(
                    &mut out,
                    &id,
                    &format!(
                        "{{\"content\":[{{\"type\":\"text\",\"text\":{}}}],\"isError\":false}}",
                        serde_string(&text)
                    ),
                );
            }
            _ => {}
        }
    }
}

fn serde_string(text: &str) -> String {
    format!("\"{text}\"")
}
