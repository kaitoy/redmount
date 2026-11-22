# Idempotent setup for the local dev Redmine (compose.yaml). Runs on every container start.
Redmine::DefaultData::Loader.load('en') if Redmine::DefaultData::Loader.no_data?
Setting.rest_api_enabled = '1'
admin = User.find_by_login('admin')
admin.update!(must_change_passwd: false)
KEY = 'redmount0dev0api0key000000000000000000000' # local only
(admin.api_token || Token.create!(user: admin, action: 'api')).update_column(:value, KEY) # create! ignores a given value
Project.find_by(identifier: 'demo') ||
  Project.create!(name: 'Demo', identifier: 'demo', tracker_ids: Tracker.ids)
