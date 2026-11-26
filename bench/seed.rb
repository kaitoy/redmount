# Deterministic benchmark seed/reset script for local Docker Redmine.
# Run via: docker compose exec -T redmine rails runner /bench-seed.rb
#
# Creates a fixed dataset in project 'demo':
#   - 4 users (admin, alice, bob, carol)
#   - 3 versions (v1.0, v1.1, v2.0), 3 categories (Backend, Frontend, Infra)
#   - 1 custom field (Severity: Low/Medium/High/Critical)
#   - 40 issues (IDs 1..30 Open, 31..40 Closed) with realistic descriptions and comments
#   - 4 wiki pages (Wiki, Architecture, Release_Process, Troubleshooting)

Redmine::DefaultData::Loader.load('en') if Redmine::DefaultData::Loader.no_data?
Setting.rest_api_enabled = '1'
Setting.text_formatting = 'common_mark'

admin = User.find_by_login('admin')
admin.update!(must_change_passwd: false, firstname: 'Admin', lastname: 'User')
KEY = 'redmount0dev0api0key000000000000000000000'
(admin.api_token || Token.create!(user: admin, action: 'api')).update_column(:value, KEY)

project = Project.find_by(identifier: 'demo') ||
  Project.create!(name: 'Demo', identifier: 'demo', tracker_ids: Tracker.ids)
project.update!(tracker_ids: Tracker.ids)
project.enable_module!(:issue_tracking) unless project.module_enabled?(:issue_tracking)
project.enable_module!(:wiki) unless project.module_enabled?(:wiki)

# Ensure users
def ensure_user(login, firstname, lastname, mail)
  u = User.find_by_login(login)
  unless u
    u = User.new(login: login, firstname: firstname, lastname: lastname, mail: mail, status: User::STATUS_ACTIVE)
    u.password = 'BenchPass123!'
    u.password_confirmation = 'BenchPass123!'
    u.must_change_passwd = false
    u.save!
  end
  u
end

alice = ensure_user('alice', 'Alice', 'Tanaka', 'alice@example.com')
bob   = ensure_user('bob',   'Bob',   'Sato',   'bob@example.com')
carol = ensure_user('carol', 'Carol', 'Suzuki', 'carol@example.com')

dev_role = Role.find_by!(name: 'Developer')
[admin, alice, bob, carol].each do |u|
  unless Member.exists?(project: project, user_id: u.id)
    Member.create!(project: project, user_id: u.id, role_ids: [dev_role.id])
  end
end

# Custom field: Severity
cf_severity = IssueCustomField.find_by(name: 'Severity') || IssueCustomField.create!(
  name: 'Severity',
  field_format: 'list',
  possible_values: %w[Low Medium High Critical],
  is_for_all: true,
  is_filter: true,
  tracker_ids: Tracker.ids
)
cf_severity.update!(tracker_ids: Tracker.ids, is_for_all: true)

# Clear existing benchmark data deterministically
JournalDetail.delete_all
Journal.delete_all
CustomValue.where(customized_type: 'Issue').delete_all
Attachment.delete_all
Issue.delete_all
Version.where(project: project).delete_all
IssueCategory.where(project: project).delete_all

if project.wiki
  WikiContentVersion.delete_all
  WikiContent.delete_all
  WikiPage.where(wiki_id: project.wiki.id).delete_all
end

# Reset SQLite autoincrement sequences for deterministic IDs
%w[issues journals journal_details versions issue_categories wiki_pages wiki_contents].each do |tbl|
  ActiveRecord::Base.connection.execute("DELETE FROM sqlite_sequence WHERE name = '#{tbl}'")
rescue ActiveRecord::StatementInvalid
  # ignore if table not yet in sqlite_sequence
end

# Reference lookups
t_bug     = Tracker.find_by!(name: 'Bug')
t_feature = Tracker.find_by!(name: 'Feature')
t_support = Tracker.find_by!(name: 'Support')

s_new      = IssueStatus.find_by!(name: 'New')
s_inprog   = IssueStatus.find_by!(name: 'In Progress')
s_resolved = IssueStatus.find_by!(name: 'Resolved')
s_feedback = IssueStatus.find_by!(name: 'Feedback')
s_closed   = IssueStatus.find_by!(name: 'Closed')

p_low    = IssuePriority.find_by!(name: 'Low')
p_normal = IssuePriority.find_by!(name: 'Normal')
p_high   = IssuePriority.find_by!(name: 'High')
p_urgent = IssuePriority.find_by!(name: 'Urgent')

v10 = Version.create!(project: project, name: 'v1.0', status: 'open', due_date: Date.new(2026, 6, 30))
v11 = Version.create!(project: project, name: 'v1.1', status: 'open', due_date: Date.new(2026, 9, 30))
v20 = Version.create!(project: project, name: 'v2.0', status: 'open', due_date: Date.new(2026, 12, 31))

c_back  = IssueCategory.create!(project: project, name: 'Backend')
c_front = IssueCategory.create!(project: project, name: 'Frontend')
c_infra = IssueCategory.create!(project: project, name: 'Infra')

# Helper to create issue + comments
def add_issue(project:, author:, tracker:, status:, priority:, subject:, description:,
              assigned_to: nil, version: nil, category: nil, done_ratio: 0,
              severity: 'Medium', cf_severity:, comments: [])
  issue = Issue.new(
    project: project,
    author: author,
    tracker: tracker,
    status: status,
    priority: priority,
    subject: subject,
    description: description,
    assigned_to: assigned_to,
    fixed_version: version,
    category: category,
    done_ratio: done_ratio
  )
  issue.custom_field_values = { cf_severity.id => severity }
  issue.save!

  comments.each do |c_author, c_notes|
    j = Journal.new(journalized: issue, user: c_author, notes: c_notes)
    j.save!
  end
  issue
end

# ---------------------------------------------------------------------------
# Open issues: IDs 1..30
#
# Invariants verified by benchmark scenarios:
#   - S1 (Open Bug + priority=High): EXACTLY IDs 1, 2, 3, 4
#   - S2 (contains 'ERR-4092'):      EXACTLY ID 14 (inside 2nd comment)
#   - S3 (edit target):              ID 7 (Bug, New, Normal, done=0, assigned=Bob)
#   - S4 (comment target):           ID 10 (Feature, 3 comments; last asks for staging port & timeout)
#   - S6 (Open Bug + version=v1.0):  EXACTLY IDs 2, 5, 8
# ---------------------------------------------------------------------------

specs = [
  # --- Open Bugs (IDs 1..12) ---
  # ID 1: S1 match (Open Bug, High)
  { tracker: t_bug, status: s_new, priority: p_high, assigned_to: alice, version: v11, category: c_back,
    severity: 'High', done_ratio: 0,
    subject: 'OAuth token refresh fails after clock skew',
    description: "When the worker node clock drifts by more than 30 seconds, token refresh returns 401.\n\n### Steps to Reproduce\n1. Skew node clock by +45s\n2. Wait for access token expiry\n3. Observe 401 loop in auth logs",
    comments: [
      [bob, 'Reproduced on staging-worker-02. Adding 60s leeway to JWT validation should mitigate this.']
    ] },

  # ID 2: S1 match (Open Bug, High) AND S6 match (Open Bug, v1.0)
  { tracker: t_bug, status: s_inprog, priority: p_high, assigned_to: bob, version: v10, category: c_front,
    severity: 'High', done_ratio: 40,
    subject: 'Dashboard chart freezes with >10k data points',
    description: "Rendering the monthly throughput chart with over 10,000 points blocks the main thread for ~4 seconds.\n\nExpected: smooth zoom/pan via downsampling or canvas rendering.",
    comments: [
      [alice, 'Confirmed in Chrome 128. LTTB downsampling to 1,000 points looks promising.'],
      [bob, 'Prototype branch reduces frame time from 3800ms to 45ms.']
    ] },

  # ID 3: S1 match (Open Bug, High)
  { tracker: t_bug, status: s_feedback, priority: p_high, assigned_to: carol, version: v11, category: c_infra,
    severity: 'Critical', done_ratio: 70,
    subject: 'Memory leak in webhook retry worker',
    description: "RSS of `webhook-worker` grows by ~150MB/day when downstream endpoints time out repeatedly.\n\nHeap dump shows retained HTTP response buffers.",
    comments: [
      [carol, 'Deployed patched container to canary; monitoring RSS over 24h.']
    ] },

  # ID 4: S1 match (Open Bug, High)
  { tracker: t_bug, status: s_new, priority: p_high, assigned_to: alice, version: v20, category: c_back,
    severity: 'High', done_ratio: 0,
    subject: 'Deadlock during concurrent invoice finalization',
    description: "Two parallel finalization requests for the same customer account occasionally trigger a DB deadlock on `invoices` and `ledger_entries`.",
    comments: [] },

  # ID 5: S6 match (Open Bug, v1.0) - priority Normal (NOT S1)
  { tracker: t_bug, status: s_inprog, priority: p_normal, assigned_to: alice, version: v10, category: c_back,
    severity: 'Medium', done_ratio: 60,
    subject: 'Timezone offset wrong for DST transition dates',
    description: "Scheduled reports configured for America/New_York run 1 hour early on the spring DST switchover day.",
    comments: [
      [alice, 'Switched cron evaluator to IANA zone rules instead of fixed UTC offset.']
    ] },

  # ID 6: Open Bug, Normal, v1.1
  { tracker: t_bug, status: s_new, priority: p_normal, assigned_to: carol, version: v11, category: c_front,
    severity: 'Low', done_ratio: 0,
    subject: 'Sidebar collapses unexpectedly on tablet landscape',
    description: "At 1024px viewport width, the navigation sidebar flickers between collapsed and expanded states.",
    comments: [] },

  # ID 7: S3 edit target (Bug, New, Normal, done=0, assigned=Bob)
  { tracker: t_bug, status: s_new, priority: p_normal, assigned_to: bob, version: v11, category: c_back,
    severity: 'Medium', done_ratio: 0,
    subject: 'Search pagination returns duplicate items on page 2',
    description: "When sorting search results by `updated_at DESC`, items with identical timestamps can appear on both page 1 and page 2.\n\n### Proposed Fix\nAdd `id DESC` as a deterministic tie-breaker in the SQL `ORDER BY` clause.",
    comments: [
      [carol, 'Verified this happens whenever bulk import updates multiple rows within the same second.']
    ] },

  # ID 8: S6 match (Open Bug, v1.0) - priority Low (NOT S1)
  { tracker: t_bug, status: s_new, priority: p_low, assigned_to: carol, version: v10, category: c_front,
    severity: 'Low', done_ratio: 20,
    subject: 'Tooltip clipped inside overflow-hidden modal',
    description: "Help tooltips near the top edge of the settings modal are partially clipped by the modal container.",
    comments: [
      [bob, 'Using a portal or CSS anchor positioning should fix this cleanly.']
    ] },

  # ID 9: Open Bug, Urgent (NOT High, so NOT S1)
  { tracker: t_bug, status: s_inprog, priority: p_urgent, assigned_to: alice, version: v11, category: c_infra,
    severity: 'Critical', done_ratio: 30,
    subject: 'Primary replica lag spikes during nightly vacuum',
    description: "Read replica lag exceeds 15 seconds during the 03:00 UTC autovacuum window.",
    comments: [
      [alice, 'Throttling vacuum_cost_limit to 200 reduced lag to <2s in load test.']
    ] },

  # ID 10: S4 comment target (Feature, 3 comments)
  { tracker: t_feature, status: s_inprog, priority: p_normal, assigned_to: bob, version: v11, category: c_infra,
    severity: 'Medium', done_ratio: 50,
    subject: 'Mutual TLS support for internal gRPC gateway',
    description: "Enable mTLS between the API edge proxy and internal billing/notification services.\n\n### Requirements\n- Automatic certificate rotation via short-lived certs\n- Configurable TLS port and handshake timeout per environment",
    comments: [
      [alice, 'CA bundle distribution is ready in staging. Certificates rotate every 24h.'],
      [bob, 'Gateway config template is merged. I need the staging parameters before enabling enforcement.'],
      [carol, 'Before we flip the switch, could you confirm the staging TLS port number and the handshake timeout value we should configure?']
    ] },

  # ID 11: Open Bug, Low
  { tracker: t_bug, status: s_resolved, priority: p_low, assigned_to: bob, version: v11, category: c_front,
    severity: 'Low', done_ratio: 90,
    subject: 'Keyboard shortcut Cmd+K ignored in Safari',
    description: "Pressing Cmd+K while focus is inside the filter input does not open the command palette on Safari 17.",
    comments: [] },

  # ID 12: Open Bug, Normal
  { tracker: t_bug, status: s_new, priority: p_normal, assigned_to: nil, version: v20, category: c_back,
    severity: 'Medium', done_ratio: 0,
    subject: 'CSV importer rejects UTF-8 BOM header',
    description: "Files exported from Excel on Windows start with a UTF-8 BOM, causing the first column name not to match `email`.",
    comments: [] },

  # ID 13: Open Bug, Normal
  { tracker: t_bug, status: s_feedback, priority: p_normal, assigned_to: alice, version: v11, category: c_back,
    severity: 'Medium', done_ratio: 80,
    subject: 'Rate limiter resets counter on Redis failover',
    description: "During Redis Sentinel promotion, per-IP sliding window counters briefly reset.",
    comments: [
      [carol, 'Acceptable if brief, but let us document the behavior in Troubleshooting wiki.']
    ] },

  # ID 14: S2 target (contains ERR-4092 ONLY in 2nd comment)
  { tracker: t_support, status: s_inprog, priority: p_normal, assigned_to: carol, version: v11, category: c_back,
    severity: 'High', done_ratio: 40,
    subject: 'Nightly batch job failed for tenant acme-corp',
    description: "Customer reported that the 02:00 UTC nightly aggregation did not produce output files for `acme-corp`.\n\nNo alerts fired on the web tier; investigating worker logs.",
    comments: [
      [alice, 'Checked worker-03 logs around 02:04 UTC. The job started normally and processed 12,400 records before stalling.'],
      [carol, 'Found the root cause in the worker crash dump: `ERR-4092` (Connection pool exhausted due to unclosed transaction in BillingWorker when a retryable deadlock occurs).'],
      [bob, 'I will prepare a fix ensuring the transaction is rolled back in an `ensure` block.']
    ] },

  # --- Open Features (IDs 15..23) ---
  # ID 15: Open Feature, High (NOT Bug, so NOT S1)
  { tracker: t_feature, status: s_new, priority: p_high, assigned_to: alice, version: v11, category: c_back,
    severity: 'Medium', done_ratio: 0,
    subject: 'Webhook signing with HMAC-SHA256',
    description: "Sign outgoing webhook payloads with a per-endpoint secret so receivers can verify authenticity.",
    comments: [
      [alice, 'Header format: `X-Signature-256: sha256=<hex>`.']
    ] },

  # ID 16: Open Feature, Normal
  { tracker: t_feature, status: s_inprog, priority: p_normal, assigned_to: bob, version: v11, category: c_front,
    severity: 'Low', done_ratio: 30,
    subject: 'Dark mode theme persistence per user',
    description: "Store user theme preference (light/dark/system) in user profile settings.",
    comments: [] },

  # ID 17: Open Feature, High (NOT Bug, so NOT S1)
  { tracker: t_feature, status: s_new, priority: p_high, assigned_to: carol, version: v20, category: c_infra,
    severity: 'High', done_ratio: 0,
    subject: 'Multi-region active-passive failover runbook automation',
    description: "Automate DNS weight shift and read-replica promotion via Terraform and GitHub Actions.",
    comments: [] },

  # ID 18: Open Feature, v1.0 (NOT Bug, so NOT S6)
  { tracker: t_feature, status: s_inprog, priority: p_normal, assigned_to: alice, version: v10, category: c_front,
    severity: 'Medium', done_ratio: 70,
    subject: 'Bulk tag assignment from issue list',
    description: "Allow selecting multiple rows in the list view and applying tags in a single action.",
    comments: [
      [bob, 'API endpoint supports up to 100 IDs per request.']
    ] },

  # ID 19: Open Feature, Low
  { tracker: t_feature, status: s_new, priority: p_low, assigned_to: nil, version: v20, category: c_front,
    severity: 'Low', done_ratio: 0,
    subject: 'Custom column ordering in table views',
    description: "Let users drag and reorder table columns and persist the layout in localStorage.",
    comments: [] },

  # ID 20: Open Feature, Normal
  { tracker: t_feature, status: s_inprog, priority: p_normal, assigned_to: bob, version: v11, category: c_back,
    severity: 'Medium', done_ratio: 20,
    subject: 'Audit log export in JSONL format',
    description: "Provide a streaming endpoint to export audit events as newline-delimited JSON for SIEM ingestion.",
    comments: [] },

  # ID 21: Open Feature, Normal
  { tracker: t_feature, status: s_new, priority: p_normal, assigned_to: carol, version: v20, category: c_infra,
    severity: 'Medium', done_ratio: 0,
    subject: 'OpenTelemetry trace propagation to background jobs',
    description: "Inject W3C traceparent headers into job payloads so async workers appear in the same distributed trace.",
    comments: [
      [carol, 'Verified with Jaeger in dev; overhead is negligible (<0.2ms/job).']
    ] },

  # ID 22: Open Feature, Low
  { tracker: t_feature, status: s_new, priority: p_low, assigned_to: alice, version: v20, category: c_front,
    severity: 'Low', done_ratio: 0,
    subject: 'Markdown table copy button in preview',
    description: "Add a one-click copy-as-TSV button on rendered Markdown tables.",
    comments: [] },

  # ID 23: Open Feature, Normal
  { tracker: t_feature, status: s_feedback, priority: p_normal, assigned_to: bob, version: v11, category: c_back,
    severity: 'Medium', done_ratio: 80,
    subject: 'Per-project API rate limit quotas',
    description: "Allow admins to configure custom requests-per-minute limits for enterprise tenants.",
    comments: [] },

  # --- Open Support (IDs 24..30) ---
  # ID 24: Open Support, High (NOT Bug, so NOT S1)
  { tracker: t_support, status: s_new, priority: p_high, assigned_to: carol, version: v11, category: c_infra,
    severity: 'High', done_ratio: 0,
    subject: 'SSO SAML metadata certificate rotation for globex',
    description: "Globex IdP certificate expires next month; coordinate metadata update without downtime.",
    comments: [] },

  # ID 25: Open Support, Normal
  { tracker: t_support, status: s_inprog, priority: p_normal, assigned_to: alice, version: v11, category: c_back,
    severity: 'Low', done_ratio: 50,
    subject: 'Clarify API pagination limits in customer docs',
    description: "Customer asked whether `limit=100` is the hard cap for `/issues.json`.",
    comments: [
      [alice, 'Drafted FAQ entry explaining offset/limit pagination and cursor recommendations.']
    ] },

  # ID 26: Open Support, Low
  { tracker: t_support, status: s_new, priority: p_low, assigned_to: bob, version: nil, category: c_front,
    severity: 'Low', done_ratio: 0,
    subject: 'How to configure custom domain CNAME',
    description: "Customer needs step-by-step DNS instructions for pointing `status.customer.example` to our edge.",
    comments: [] },

  # ID 27: Open Support, Normal
  { tracker: t_support, status: s_feedback, priority: p_normal, assigned_to: carol, version: v10, category: c_infra,
    severity: 'Medium', done_ratio: 70,
    subject: 'IP allowlist update for EU webhook egress',
    description: "Provide the updated NAT gateway CIDR blocks for the Frankfurt region.",
    comments: [] },

  # ID 28: Open Support, Normal
  { tracker: t_support, status: s_new, priority: p_normal, assigned_to: alice, version: nil, category: c_back,
    severity: 'Low', done_ratio: 0,
    subject: 'Data retention policy inquiry for deleted accounts',
    description: "Security questionnaire asks how long backups retain purged tenant rows.",
    comments: [] },

  # ID 29: Open Support, Low
  { tracker: t_support, status: s_new, priority: p_low, assigned_to: nil, version: nil, category: c_front,
    severity: 'Low', done_ratio: 0,
    subject: 'Browser compatibility matrix request',
    description: "Enterprise procurement requested the official list of supported browser versions.",
    comments: [] },

  # ID 30: Open Support, Normal
  { tracker: t_support, status: s_inprog, priority: p_normal, assigned_to: bob, version: v11, category: c_back,
    severity: 'Medium', done_ratio: 30,
    subject: 'Sandbox environment reset for partner integration',
    description: "Partner team requested a fresh sandbox seed with sample OAuth clients.",
    comments: [
      [bob, 'Scheduled reset for Friday 10:00 UTC.']
    ] },

  # ---------------------------------------------------------------------------
  # Closed issues: IDs 31..40 (must NOT match Open-only queries like S1 and S6)
  # ---------------------------------------------------------------------------
  # ID 31: Closed Bug, High (must NOT match S1)
  { tracker: t_bug, status: s_closed, priority: p_high, assigned_to: alice, version: v10, category: c_back,
    severity: 'High', done_ratio: 100,
    subject: 'Session cookie missing SameSite attribute',
    description: "Fixed by setting `SameSite=Lax` in session middleware.",
    comments: [[alice, 'Verified in production.']] },

  # ID 32: Closed Bug, High (must NOT match S1)
  { tracker: t_bug, status: s_closed, priority: p_high, assigned_to: bob, version: v11, category: c_front,
    severity: 'High', done_ratio: 100,
    subject: 'XSS sanitization bypass in markdown link title',
    description: "Upgraded sanitizer and added regression tests.",
    comments: [[bob, 'Released in hotfix 1.0.2.']] },

  # ID 33: Closed Bug, v1.0 (must NOT match S6)
  { tracker: t_bug, status: s_closed, priority: p_normal, assigned_to: carol, version: v10, category: c_infra,
    severity: 'Medium', done_ratio: 100,
    subject: 'Healthcheck endpoint returned 500 during graceful shutdown',
    description: "Return 503 during drain phase instead of 500.",
    comments: [] },

  # ID 34: Closed Bug, Low
  { tracker: t_bug, status: s_closed, priority: p_low, assigned_to: alice, version: v10, category: c_front,
    severity: 'Low', done_ratio: 100,
    subject: 'Typo in password reset email footer',
    description: "Corrected company address in email template.",
    comments: [] },

  # ID 35: Closed Feature
  { tracker: t_feature, status: s_closed, priority: p_normal, assigned_to: bob, version: v10, category: c_back,
    severity: 'Medium', done_ratio: 100,
    subject: 'API key rotation without downtime',
    description: "Allow two active API keys per user during rotation window.",
    comments: [] },

  # ID 36: Closed Feature
  { tracker: t_feature, status: s_closed, priority: p_high, assigned_to: carol, version: v10, category: c_infra,
    severity: 'High', done_ratio: 100,
    subject: 'Automated nightly SQLite/Postgres backup verification',
    description: "Restore latest backup to ephemeral instance and run integrity checks.",
    comments: [] },

  # ID 37: Closed Feature
  { tracker: t_feature, status: s_closed, priority: p_low, assigned_to: alice, version: v10, category: c_front,
    severity: 'Low', done_ratio: 100,
    subject: 'Show relative timestamps with absolute tooltip',
    description: "Render `2 hours ago` with ISO-8601 title attribute.",
    comments: [] },

  # ID 38: Closed Support
  { tracker: t_support, status: s_closed, priority: p_normal, assigned_to: bob, version: v10, category: c_back,
    severity: 'Low', done_ratio: 100,
    subject: 'Initial onboarding walkthrough for acme-corp',
    description: "Completed kickoff call and shared API credentials.",
    comments: [] },

  # ID 39: Closed Support
  { tracker: t_support, status: s_closed, priority: p_low, assigned_to: carol, version: v10, category: c_infra,
    severity: 'Low', done_ratio: 100,
    subject: 'Provide SOC2 Type II report',
    description: "Shared NDA-gated report link via customer portal.",
    comments: [] },

  # ID 40: Closed Support
  { tracker: t_support, status: s_closed, priority: p_normal, assigned_to: alice, version: v10, category: c_front,
    severity: 'Low', done_ratio: 100,
    subject: 'Import legacy tickets from CSV',
    description: "Imported 150 historical tickets into staging for validation.",
    comments: [] }
]

specs.each do |sp|
  add_issue(project: project, author: admin, cf_severity: cf_severity, **sp)
end

# Wiki pages
wiki = project.wiki || project.create_wiki!(start_page: 'Wiki')
wiki_pages = [
  ['Wiki', nil, "# Demo Project Wiki\n\nWelcome to the Demo project documentation.\n\n- [[Architecture]]\n- [[Release_Process]]\n- [[Troubleshooting]]\n"],
  ['Architecture', 'Wiki', "# System Architecture\n\n## Overview\nThe service consists of an edge API gateway, stateless web workers, and background job processors backed by PostgreSQL and Redis.\n\n## Data Flow\n1. Clients send HTTPS requests to the edge proxy.\n2. Requests are authenticated via OAuth2 / API keys.\n3. Long-running tasks are enqueued onto Redis streams.\n"],
  ['Release_Process', 'Wiki', "# Release Process\n\n1. Cut release branch `release/vX.Y`.\n2. Run full integration suite on staging.\n3. Verify all `vX.Y` Bug issues are closed or deferred.\n4. Tag and deploy via CI.\n"],
  ['Troubleshooting', 'Wiki', "# Troubleshooting\n\n## High Latency\nCheck replica lag metrics and Redis memory fragmentation.\n\n## Webhook Failures\nInspect `webhook-worker` logs and verify downstream TLS certificates.\n"]
]

created_pages = {}
wiki_pages.each do |title, parent_title, text|
  page = wiki.pages.create!(title: title, parent: created_pages[parent_title])
  page.content = WikiContent.create!(page: page, author: admin, text: text)
  created_pages[title] = page
end

puts "Seeded demo project: #{Issue.count} issues (#{Issue.open.count} open, #{Issue.where(status: s_closed).count} closed), #{Journal.count} comments, #{wiki.pages.count} wiki pages."
