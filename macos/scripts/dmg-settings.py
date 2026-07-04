# dmgbuild settings for the TrueNAS Apps Watcher disk image.
# Usage: dmgbuild -s scripts/dmg-settings.py "TrueNAS Apps Watcher" out.dmg
# (run from the macos/ directory; see scripts/make_dmg.sh)

import os.path

app = defines.get("app", "dist/TrueNAS Apps Watcher.app")  # noqa: F821
appname = os.path.basename(app)

format = "UDZO"
files = [app]
symlinks = {"Applications": "/Applications"}
badge_icon = "AppIcon.icns"

# Matches the layout drawn into assets/dmg-background.svg.
background = defines.get("background", "assets/dmg-background.tiff")  # noqa: F821
window_rect = ((200, 120), (600, 400))
icon_size = 100
text_size = 13
icon_locations = {
    appname: (150, 185),
    "Applications": (450, 185),
}
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
