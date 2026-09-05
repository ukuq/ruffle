# A note on ellipsis: they indicate when an action won't immediately happen, but needs more information from the user.
# For example, "Open File..." won't open a file, it'll ask the user to choose a file and *then* open the file.
# Whereas "Close" or "About" wouldn't need any information, they'd immediately happen as soon as you click it.
# In general, if clicking the thing immediately does that thing, no ellipsis. If it asks for more information, ellipsis.
# This is the case for English, but other languages may have other rules or ways of showing this.

# Some items may be also be duplicated here, like "About Ruffle" in the menu and "About Ruffle" elsewhere as the header of the about menu.
# This is because some languages may use different wording for one or the other, especially with things like capitalisation.

file-menu = File
file-menu-game-home = Game Home
file-menu-open-file = Open File...
file-menu-open-directory = Open Folder...
file-menu-open-advanced = Open Advanced...
file-menu-close = Close
file-menu-reload = Reload
file-menu-recents = Recents
file-menu-recents-empty = No recent entries
file-menu-preferences = Preferences...
file-menu-exit = Exit
file-menu-export = Export...

controls-menu = Controls
controls-menu-suspend = Suspend
controls-menu-resume = Resume
controls-menu-step-once = Step Once
controls-menu-volume = Volume controls

help-menu = Help
help-menu-join-discord = Join Discord
help-menu-report-a-bug = Report a Bug...
help-menu-sponsor-development = Sponsor Development...
help-menu-translate-ruffle = Translate Ruffle...
help-menu-about = About Ruffle

cache-metrics-menu = Cache
cache-metrics-refresh-manifest = Refresh Version Manifest
cache-metrics-refresh-manifest-tooltip = Clear the in-memory version root and Bloom manifest so the next request fetches them again
cache-metrics-clear-files = Clear Cache (File Cache) ⚠️
cache-metrics-clear-files-tooltip = Delete downloaded encrypted Seer2 resource files
cache-metrics-confirm-title = Confirm Operation
cache-metrics-confirm-body = Clearing the file cache will reduce performance until resources are downloaded again. Continue?
cache-metrics-confirm-yes = Yes
cache-metrics-confirm-no = No
cache-metrics-error-title = Operation Failed
seer2-proxy-label = Local Proxy
seer2-proxy-enable-tooltip = Select a local resource directory; local files take priority over the file cache and network
seer2-proxy-disable-tooltip = Disable the local resource proxy and reload the game
seer2-proxy-picker-title = Select Local Resource Proxy Directory (must contain the seer2 folder)

bookmarks-menu = Bookmarks
bookmarks-menu-add = Add...
bookmarks-menu-manage = Manage Bookmarks...

debug-menu = Debug Tools
debug-menu-open-stage = View Stage Info
debug-menu-open-root-movie-clip = View Root MovieClip
debug-menu-open-movie = View Movie
debug-menu-open-movie-list = Show Known Movies
debug-menu-open-domain-list = Show Domains
debug-menu-search-display-objects = Search Display Objects...
debug-menu-network-monitor = Network Monitor

view-menu = View
view-menu-fullscreen = Full Screen
